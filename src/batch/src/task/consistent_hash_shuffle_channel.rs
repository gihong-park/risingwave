// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::ops::BitAnd;
use std::option::Option;

use itertools::Itertools;
use risingwave_common::array::DataChunk;
use risingwave_common::buffer::Bitmap;
use risingwave_common::error::ErrorCode::InternalError;
use risingwave_common::error::Result;
use risingwave_common::util::hash_util::Crc32FastBuilder;
use risingwave_pb::batch_plan::exchange_info::ConsistentHashInfo;
use risingwave_pb::batch_plan::*;
use tokio::sync::mpsc;

use crate::error::BatchError::SenderError;
use crate::error::Result as BatchResult;
use crate::task::channel::{ChanReceiver, ChanReceiverImpl, ChanSender, ChanSenderImpl};
use crate::task::data_chunk_in_channel::DataChunkInChannel;

#[derive(Clone)]
pub struct ConsistentHashShuffleSender {
    senders: Vec<mpsc::Sender<Option<DataChunkInChannel>>>,
    consistent_hash_info: ConsistentHashInfo,
    output_count: usize,
}

impl Debug for ConsistentHashShuffleSender {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsistentHashShuffleSender")
            .field("consistent_hash_info", &self.consistent_hash_info)
            .finish()
    }
}

pub struct ConsistentHashShuffleReceiver {
    receiver: mpsc::Receiver<Option<DataChunkInChannel>>,
}

fn generate_hash_values(
    chunk: &DataChunk,
    consistent_hash_info: &ConsistentHashInfo,
) -> BatchResult<Vec<usize>> {
    let hasher_builder = Crc32FastBuilder;

    let hash_values = chunk
        .get_hash_values(
            &consistent_hash_info
                .key
                .iter()
                .map(|idx| *idx as usize)
                .collect::<Vec<_>>(),
            hasher_builder,
        )
        .iter_mut()
        .map(|hash_value| consistent_hash_info.vmap[hash_value.to_vnode().to_index()] as usize)
        .collect::<Vec<_>>();
    Ok(hash_values)
}

/// The returned chunks must have cardinality > 0.
fn generate_new_data_chunks(
    chunk: &DataChunk,
    output_count: usize,
    hash_values: &[usize],
) -> Vec<DataChunk> {
    let mut vis_maps = vec![vec![]; output_count];
    hash_values.iter().for_each(|hash| {
        for (sink_id, vis_map) in vis_maps.iter_mut().enumerate() {
            if *hash == sink_id {
                vis_map.push(true);
            } else {
                vis_map.push(false);
            }
        }
    });
    let mut res = Vec::with_capacity(output_count);
    for (sink_id, vis_map_vec) in vis_maps.into_iter().enumerate() {
        let vis_map: Bitmap = vis_map_vec.into_iter().collect();
        let vis_map = if let Some(visibility) = chunk.visibility() {
            vis_map.bitand(visibility)
        } else {
            vis_map
        };
        let new_data_chunk = chunk.with_visibility(vis_map);
        trace!(
            "send to sink:{}, cardinality:{}",
            sink_id,
            new_data_chunk.cardinality()
        );
        res.push(new_data_chunk);
    }
    res
}

impl ChanSender for ConsistentHashShuffleSender {
    type SendFuture<'a> = impl Future<Output = BatchResult<()>> + 'a;

    fn send(&mut self, chunk: Option<DataChunk>) -> Self::SendFuture<'_> {
        async move {
            match chunk {
                Some(c) => self.send_chunk(c).await,
                None => self.send_done().await,
            }
        }
    }
}

impl ConsistentHashShuffleSender {
    async fn send_chunk(&mut self, chunk: DataChunk) -> BatchResult<()> {
        let hash_values = generate_hash_values(&chunk, &self.consistent_hash_info)?;
        let new_data_chunks = generate_new_data_chunks(&chunk, self.output_count, &hash_values);

        for (sink_id, new_data_chunk) in new_data_chunks.into_iter().enumerate() {
            trace!(
                "send to sink:{}, cardinality:{}",
                sink_id,
                new_data_chunk.cardinality()
            );
            // The reason we need to add this filter only in HashShuffleSender is that
            // `generate_new_data_chunks` may generate an empty chunk.
            if new_data_chunk.cardinality() > 0 {
                self.senders[sink_id]
                    .send(Some(DataChunkInChannel::new(new_data_chunk)))
                    .await
                    .map_err(|_| SenderError)?
            }
        }
        Ok(())
    }

    async fn send_done(&mut self) -> BatchResult<()> {
        for sender in &self.senders {
            sender.send(None).await.map_err(|_| SenderError)?
        }

        Ok(())
    }
}

impl ChanReceiver for ConsistentHashShuffleReceiver {
    type RecvFuture<'a> = impl Future<Output = Result<Option<DataChunkInChannel>>> + 'a;

    fn recv(&mut self) -> Self::RecvFuture<'_> {
        async move {
            match self.receiver.recv().await {
                Some(data_chunk) => Ok(data_chunk),
                // Early close should be treated as error.
                None => Err(InternalError("broken hash_shuffle_channel".to_string()).into()),
            }
        }
    }
}

pub fn new_consistent_shuffle_channel(
    shuffle: &ExchangeInfo,
    output_channel_size: usize,
) -> (ChanSenderImpl, Vec<ChanReceiverImpl>) {
    let consistent_hash_info = match shuffle.distribution {
        Some(exchange_info::Distribution::ConsistentHashInfo(ref v)) => v.clone(),
        _ => exchange_info::ConsistentHashInfo::default(),
    };

    let output_count = consistent_hash_info
        .vmap
        .iter()
        .copied()
        .sorted()
        .dedup()
        .count();

    let mut senders = Vec::with_capacity(output_count);
    let mut receivers = Vec::with_capacity(output_count);
    for _ in 0..output_count {
        let (s, r) = mpsc::channel(output_channel_size);
        senders.push(s);
        receivers.push(r);
    }
    let channel_sender = ChanSenderImpl::ConsistentHashShuffle(ConsistentHashShuffleSender {
        senders,
        consistent_hash_info,
        output_count,
    });
    let channel_receivers = receivers
        .into_iter()
        .map(|receiver| {
            ChanReceiverImpl::ConsistentHashShuffle(ConsistentHashShuffleReceiver { receiver })
        })
        .collect::<Vec<_>>();
    (channel_sender, channel_receivers)
}
