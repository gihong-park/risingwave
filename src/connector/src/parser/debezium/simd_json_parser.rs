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

use std::fmt::Debug;

use futures_async_stream::try_stream;
use risingwave_common::error::ErrorCode::ProtocolError;
use risingwave_common::error::{Result, RwError};
use simd_json::{BorrowedValue, StaticNode, ValueAccess};

use super::operators::*;
use crate::impl_common_parser_logic;
use crate::parser::common::simd_json_parse_value;
use crate::parser::{SourceStreamChunkRowWriter, WriteGuard};
use crate::source::SourceColumnDesc;

const BEFORE: &str = "before";
const AFTER: &str = "after";
const OP: &str = "op";

#[inline]
fn ensure_not_null<'a, 'b: 'a>(value: &'a BorrowedValue<'b>) -> Option<&'a BorrowedValue<'b>> {
    if let BorrowedValue::Static(StaticNode::Null) = value {
        None
    } else {
        Some(value)
    }
}

impl_common_parser_logic!(DebeziumJsonParser);

#[derive(Debug)]
pub struct DebeziumJsonParser {
    pub(crate) rw_columns: Vec<SourceColumnDesc>,
}

impl DebeziumJsonParser {
    pub fn new(rw_columns: Vec<SourceColumnDesc>) -> Result<Self> {
        Ok(Self { rw_columns })
    }

    #[allow(clippy::unused_async)]
    pub async fn parse_inner(
        &self,
        payload: &[u8],
        mut writer: SourceStreamChunkRowWriter<'_>,
    ) -> Result<WriteGuard> {
        let mut payload_mut = payload.to_vec();
        let event: BorrowedValue<'_> = simd_json::to_borrowed_value(&mut payload_mut)
            .map_err(|e| RwError::from(ProtocolError(e.to_string())))?;

        let payload = event
            .get("payload")
            .and_then(ensure_not_null)
            .ok_or_else(|| {
                RwError::from(ProtocolError("no payload in debezium event".to_owned()))
            })?;

        let op = payload.get(OP).and_then(|v| v.as_str()).ok_or_else(|| {
            RwError::from(ProtocolError(
                "op field not found in debezium json".to_owned(),
            ))
        })?;

        match op {
            DEBEZIUM_UPDATE_OP => {
                let before = payload.get(BEFORE).and_then(ensure_not_null).ok_or_else(|| {
                    RwError::from(ProtocolError(
                        "before is missing for updating event. If you are using postgres, you may want to try ALTER TABLE $TABLE_NAME REPLICA IDENTITY FULL;".to_string(),
                    ))
                })?;

                let after = payload
                    .get(AFTER)
                    .and_then(ensure_not_null)
                    .ok_or_else(|| {
                        RwError::from(ProtocolError(
                            "after is missing for updating event".to_string(),
                        ))
                    })?;

                writer.update(|column| {
                    let before = simd_json_parse_value(
                        &column.data_type,
                        before.get(column.name.to_ascii_lowercase().as_str()),
                    )?;
                    let after = simd_json_parse_value(
                        &column.data_type,
                        after.get(column.name.to_ascii_lowercase().as_str()),
                    )?;

                    Ok((before, after))
                })
            }
            DEBEZIUM_CREATE_OP | DEBEZIUM_READ_OP => {
                let after = payload
                    .get(AFTER)
                    .and_then(ensure_not_null)
                    .ok_or_else(|| {
                        RwError::from(ProtocolError(
                            "after is missing for creating event".to_string(),
                        ))
                    })?;

                writer.insert(|column| {
                    simd_json_parse_value(
                        &column.data_type,
                        after.get(column.name.to_ascii_lowercase().as_str()),
                    )
                    .map_err(Into::into)
                })
            }
            DEBEZIUM_DELETE_OP => {
                let before = payload
                    .get(BEFORE)
                    .and_then(ensure_not_null)
                    .ok_or_else(|| {
                        RwError::from(ProtocolError(
                            "before is missing for delete event".to_string(),
                        ))
                    })?;

                writer.delete(|column| {
                    simd_json_parse_value(
                        &column.data_type,
                        before.get(column.name.to_ascii_lowercase().as_str()),
                    )
                    .map_err(Into::into)
                })
            }
            _ => Err(RwError::from(ProtocolError(format!(
                "unknown debezium op: {}",
                op
            )))),
        }
    }
}
