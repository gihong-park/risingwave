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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::try_join_all;
use itertools::Itertools;
use risingwave_common::util::epoch::Epoch;
use risingwave_pb::common::worker_node::State;
use risingwave_pb::common::{ActorInfo, WorkerNode, WorkerType};
use risingwave_pb::stream_plan::barrier::Mutation;
use risingwave_pb::stream_plan::AddMutation;
use risingwave_pb::stream_service::{
    BroadcastActorInfoTableRequest, BuildActorsRequest, ForceStopActorsRequest, UpdateActorsRequest,
};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::barrier::command::CommandContext;
use crate::barrier::info::BarrierActorInfo;
use crate::barrier::{CheckpointControl, Command, GlobalBarrierManager};
use crate::manager::WorkerId;
use crate::model::ActorId;
use crate::storage::MetaStore;
use crate::stream::build_actor_connector_splits;
use crate::MetaResult;

impl<S> GlobalBarrierManager<S>
where
    S: MetaStore,
{
    // Retry base interval in milliseconds.
    const RECOVERY_RETRY_BASE_INTERVAL: u64 = 20;
    // Retry max interval.
    const RECOVERY_RETRY_MAX_INTERVAL: Duration = Duration::from_secs(5);

    #[inline(always)]
    /// Initialize a retry strategy for operation in recovery.
    fn get_retry_strategy() -> impl Iterator<Item = Duration> {
        ExponentialBackoff::from_millis(Self::RECOVERY_RETRY_BASE_INTERVAL)
            .max_delay(Self::RECOVERY_RETRY_MAX_INTERVAL)
            .map(jitter)
    }

    async fn resolve_actor_info_for_recovery(&self) -> BarrierActorInfo {
        self.resolve_actor_info(
            &mut CheckpointControl::new(self.metrics.clone()),
            &Command::barrier(),
        )
        .await
    }

    /// Clean up all dirty streaming jobs.
    async fn clean_dirty_fragments(&self) -> MetaResult<()> {
        let stream_job_ids = self.catalog_manager.list_stream_job_ids().await?;
        let table_fragments = self.fragment_manager.list_table_fragments().await?;
        let to_drop_table_fragments = table_fragments
            .into_iter()
            .filter(|table_fragment| {
                !stream_job_ids.contains(&table_fragment.table_id().table_id)
                    || !table_fragment.is_created()
            })
            .collect_vec();

        let to_drop_streaming_ids = to_drop_table_fragments
            .iter()
            .map(|t| t.table_id())
            .collect();

        debug!("clean dirty table fragments: {:?}", to_drop_streaming_ids);
        self.fragment_manager
            .drop_table_fragments_vec(&to_drop_streaming_ids)
            .await?;

        // unregister compaction group for dirty table fragments.
        let _ = self.hummock_manager
            .unregister_table_ids(
                &to_drop_streaming_ids
                    .iter()
                    .map(|t| t.table_id)
                    .collect_vec(),
            )
            .await.inspect_err(|e|
            tracing::warn!(
                "Failed to unregister compaction group for {:#?}.\nThey will be cleaned up on node restart.\n{:#?}",
                to_drop_streaming_ids,
                e)
        );

        // clean up source connector dirty changes.
        self.source_manager
            .drop_source_change(&to_drop_table_fragments)
            .await;

        Ok(())
    }

    /// Recovery the whole cluster from the latest epoch.
    pub(crate) async fn recovery(&self, prev_epoch: Epoch) -> Epoch {
        // pause discovery of all connector split changes and trigger config change.
        let _source_pause_guard = self.source_manager.paused.lock().await;

        // Abort buffered schedules, they might be dirty already.
        self.scheduled_barriers.abort().await;

        tracing::info!("recovery start!");
        self.clean_dirty_fragments()
            .await
            .expect("clean dirty fragments");
        let retry_strategy = Self::get_retry_strategy();
        let (new_epoch, _responses) = tokio_retry::Retry::spawn(retry_strategy, || async {
            let mut info = self.resolve_actor_info_for_recovery().await;
            let mut new_epoch = prev_epoch.next();

            // Migrate actors in expired CN to newly joined one.
            let migrated = self.migrate_actors(&info).await.inspect_err(|err| {
                error!(err = ?err, "migrate actors failed");
            })?;
            if migrated {
                info = self.resolve_actor_info_for_recovery().await;
            }

            // Reset all compute nodes, stop and drop existing actors.
            self.reset_compute_nodes(&info).await.inspect_err(|err| {
                error!(err = ?err, "reset compute nodes failed");
            })?;

            // update and build all actors.
            self.update_actors(&info).await.inspect_err(|err| {
                error!(err = ?err, "update actors failed");
            })?;
            self.build_actors(&info).await.inspect_err(|err| {
                error!(err = ?err, "build_actors failed");
            })?;

            // get split assignments for all actors
            let source_split_assignments = self.source_manager.list_assignments().await;
            let command = Command::Plain(Some(Mutation::Add(AddMutation {
                actor_dispatchers: Default::default(),
                actor_splits: build_actor_connector_splits(&source_split_assignments),
            })));

            let prev_epoch = new_epoch;
            new_epoch = prev_epoch.next();
            // checkpoint, used as init barrier to initialize all executors.
            let command_ctx = Arc::new(CommandContext::new(
                self.fragment_manager.clone(),
                self.snapshot_manager.clone(),
                self.env.stream_client_pool_ref(),
                info,
                prev_epoch,
                new_epoch,
                command,
                true,
                self.source_manager.clone(),
            ));

            let (barrier_complete_tx, mut barrier_complete_rx) =
                tokio::sync::mpsc::unbounded_channel();
            self.inject_barrier(command_ctx.clone(), barrier_complete_tx)
                .await;
            match barrier_complete_rx.recv().await.unwrap() {
                (_, Ok(response)) => {
                    if let Err(err) = command_ctx.post_collect().await {
                        error!(err = ?err, "post_collect failed");
                        return Err(err);
                    }
                    Ok((new_epoch, response))
                }
                (_, Err(err)) => {
                    error!(err = ?err, "inject_barrier failed");
                    Err(err)
                }
            }
        })
        .await
        .expect("Retry until recovery success.");
        tracing::info!("recovery success");

        new_epoch
    }

    /// map expired CNs to newly joined CNs, so we can migrate actors later
    /// wait until get a sufficient amount of new CNs
    /// return "map of `ActorId` in expired CN to new CN id" and "map of `WorkerId` to
    /// `WorkerNode` struct in new CNs"
    async fn get_migrate_map_plan(
        &self,
        info: &BarrierActorInfo,
        expired_workers: &[WorkerId],
    ) -> (HashMap<ActorId, WorkerId>, HashMap<WorkerId, WorkerNode>) {
        let mut cur = 0;
        let mut migrate_map = HashMap::new();
        let mut node_map = HashMap::new();
        let start = Instant::now();
        while cur < expired_workers.len() {
            let current_nodes = self
                .cluster_manager
                .list_worker_node(WorkerType::ComputeNode, Some(State::Running))
                .await;
            let new_nodes = current_nodes
                .into_iter()
                .filter(|node| {
                    !info.actor_map.contains_key(&node.id) && !node_map.contains_key(&node.id)
                })
                .collect_vec();
            for new_node in new_nodes {
                let actors = info.actor_map.get(&expired_workers[cur]).unwrap();
                for actor in actors {
                    migrate_map.insert(*actor, new_node.id);
                }
                cur += 1;
                debug!(
                    "new worker joined: {}, migrate process ({}/{})",
                    new_node.id,
                    cur,
                    expired_workers.len()
                );
                node_map.insert(new_node.id, new_node);
                if cur == expired_workers.len() {
                    return (migrate_map, node_map);
                }
            }
            warn!(
                "waiting for new worker to join, elapsed: {}s",
                start.elapsed().as_secs()
            );
            // wait to get newly joined CN
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        (migrate_map, node_map)
    }

    async fn migrate_actors(&self, info: &BarrierActorInfo) -> MetaResult<bool> {
        debug!("start migrate actors.");

        // 1. get expired workers
        let expired_workers = info
            .actor_map
            .iter()
            .filter(|(&worker, actors)| !actors.is_empty() && !info.node_map.contains_key(&worker))
            .map(|(&worker, _)| worker)
            .collect_vec();
        if expired_workers.is_empty() {
            debug!("no expired workers, skipping.");
            return Ok(false);
        }
        debug!("got expired workers {:#?}", expired_workers);

        let (migrate_map, node_map) = self.get_migrate_map_plan(info, &expired_workers).await;
        // 2. migrate actors in fragments
        self.fragment_manager
            .migrate_actors(&migrate_map, &node_map)
            .await?;
        debug!("migrate actors succeed.");

        Ok(true)
    }

    /// Update all actors in compute nodes.
    async fn update_actors(&self, info: &BarrierActorInfo) -> MetaResult<()> {
        let mut actor_infos = vec![];
        for (node_id, actors) in &info.actor_map {
            let host = info
                .node_map
                .get(node_id)
                .ok_or_else(|| anyhow::anyhow!("worker evicted, wait for online."))?
                .host
                .clone();
            actor_infos.extend(actors.iter().map(|&actor_id| ActorInfo {
                actor_id,
                host: host.clone(),
            }));
        }

        let node_actors = self.fragment_manager.all_node_actors(false).await;
        for (node_id, actors) in &info.actor_map {
            let node = info.node_map.get(node_id).unwrap();
            let client = self.env.stream_client_pool().get(node).await?;

            client
                .broadcast_actor_info_table(BroadcastActorInfoTableRequest {
                    info: actor_infos.clone(),
                })
                .await?;

            let request_id = Uuid::new_v4().to_string();
            tracing::debug!(request_id = request_id.as_str(), actors = ?actors, "update actors");
            client
                .update_actors(UpdateActorsRequest {
                    request_id,
                    actors: node_actors.get(node_id).cloned().unwrap_or_default(),
                    ..Default::default()
                })
                .await?;
        }

        Ok(())
    }

    /// Build all actors in compute nodes.
    async fn build_actors(&self, info: &BarrierActorInfo) -> MetaResult<()> {
        for (node_id, actors) in &info.actor_map {
            let node = info.node_map.get(node_id).unwrap();
            let client = self.env.stream_client_pool().get(node).await?;

            let request_id = Uuid::new_v4().to_string();
            tracing::debug!(request_id = request_id.as_str(), actors = ?actors, "build actors");
            client
                .build_actors(BuildActorsRequest {
                    request_id,
                    actor_id: actors.to_owned(),
                })
                .await?;
        }

        Ok(())
    }

    /// Reset all compute nodes by calling `force_stop_actors`.
    async fn reset_compute_nodes(&self, info: &BarrierActorInfo) -> MetaResult<()> {
        let futures = info.node_map.values().map(|worker_node| async move {
            let client = self.env.stream_client_pool().get(worker_node).await?;
            debug!(worker = ?worker_node.id, "force stop actors");
            client
                .force_stop_actors(ForceStopActorsRequest {
                    request_id: Uuid::new_v4().to_string(),
                })
                .await
        });

        try_join_all(futures).await?;
        debug!("all compute nodes have been reset.");

        Ok(())
    }
}
