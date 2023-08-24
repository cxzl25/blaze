// Copyright 2022 The Blaze Authors
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

use crate::common::memory_manager::{MemConsumer, MemConsumerInfo, MemManager};
use crate::common::BatchesInterleaver;
use crate::shuffle::rss::{rss_flush, rss_write_batch};
use crate::shuffle::sort_repartitioner::PI;
use crate::shuffle::{evaluate_hashes, evaluate_partition_ids, ShuffleRepartitioner};
use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use datafusion::common::Result;
use datafusion::execution::context::TaskContext;
use datafusion::physical_plan::Partitioning;
use futures::lock::Mutex;
use jni::objects::GlobalRef;
use std::mem::size_of;
use std::sync::{Arc, Weak};

pub struct RssSortShuffleRepartitioner {
    name: String,
    mem_consumer_info: Option<Weak<MemConsumerInfo>>,
    schema: SchemaRef,
    buffered_batches: Mutex<Vec<RecordBatch>>,
    partitioning: Partitioning,
    rss_partition_writer: GlobalRef,
    num_output_partitions: usize,
    batch_size: usize,
}

impl RssSortShuffleRepartitioner {
    pub fn new(
        partition_id: usize,
        rss_partition_writer: GlobalRef,
        schema: SchemaRef,
        partitioning: Partitioning,
        context: Arc<TaskContext>,
    ) -> Self {
        let num_output_partitions = partitioning.partition_count();
        let batch_size = context.session_config().batch_size();

        Self {
            name: format!("RssSortShufflePartitioner[partition={}]", partition_id),
            mem_consumer_info: None,
            schema,
            buffered_batches: Mutex::default(),
            partitioning,
            rss_partition_writer,
            num_output_partitions,
            batch_size,
        }
    }

    fn build_sorted_pi_vec(&self, buffered_batches: &[RecordBatch]) -> Result<Vec<PI>> {
        // combine all buffered batches
        let num_buffered_rows = buffered_batches
            .iter()
            .map(|batch| batch.num_rows())
            .sum::<usize>();

        let mut pi_vec = Vec::with_capacity(num_buffered_rows);
        for (batch_idx, batch) in buffered_batches.iter().enumerate() {
            let hashes = evaluate_hashes(&self.partitioning, batch)?;
            let partition_ids = evaluate_partition_ids(&hashes, self.num_output_partitions);

            // compute partition ids and sorted indices
            pi_vec.extend(
                hashes
                    .into_iter()
                    .zip(partition_ids.into_iter())
                    .enumerate()
                    .map(|(i, (hash, partition_id))| PI {
                        partition_id,
                        hash,
                        batch_idx: batch_idx as u32,
                        row_idx: i as u32,
                    }),
            );
        }
        pi_vec.shrink_to_fit();
        pi_vec.sort_unstable();
        Ok(pi_vec)
    }

    fn write_buffered_batches_to_rss(&self, buffered_batches: &[RecordBatch]) -> Result<()> {
        let pi_vec = self.build_sorted_pi_vec(buffered_batches)?;
        let interleaver = BatchesInterleaver::new(self.schema.clone(), buffered_batches);
        let mut cur_partition_id = 0;
        let mut cur_slice_start = 0;

        macro_rules! write_sub_batch {
            ($range:expr) => {{
                let sub_pi_vec = &pi_vec[$range];
                let sub_indices = sub_pi_vec
                    .iter()
                    .map(|pi| (pi.batch_idx as usize, pi.row_idx as usize))
                    .collect::<Vec<_>>();
                let sub_batch = interleaver.interleave(&sub_indices)?;
                rss_write_batch(&self.rss_partition_writer, cur_partition_id, sub_batch)?;
            }};
        }

        // write sorted data
        for cur_offset in 0..pi_vec.len() {
            if pi_vec[cur_offset].partition_id as usize > cur_partition_id
                || cur_offset - cur_slice_start >= self.batch_size
            {
                if cur_slice_start < cur_offset {
                    write_sub_batch!(cur_slice_start..cur_offset);
                    cur_slice_start = cur_offset;
                }
                while pi_vec[cur_offset].partition_id as usize > cur_partition_id {
                    cur_partition_id += 1;
                }
            }
        }
        if cur_slice_start < pi_vec.len() {
            write_sub_batch!(cur_slice_start..);
        }
        Ok(())
    }
}

#[async_trait]
impl MemConsumer for RssSortShuffleRepartitioner {
    fn name(&self) -> &str {
        &self.name
    }

    fn set_consumer_info(&mut self, consumer_info: Weak<MemConsumerInfo>) {
        self.mem_consumer_info = Some(consumer_info);
    }

    fn get_consumer_info(&self) -> &Weak<MemConsumerInfo> {
        self.mem_consumer_info
            .as_ref()
            .expect("consumer info not set")
    }

    async fn spill(&self) -> Result<()> {
        let batches = std::mem::take(&mut *self.buffered_batches.lock().await);
        if !batches.is_empty() {
            self.write_buffered_batches_to_rss(&batches)?;
        }
        rss_flush(&self.rss_partition_writer)?;
        self.update_mem_used(0).await?;
        Ok(())
    }
}

impl Drop for RssSortShuffleRepartitioner {
    fn drop(&mut self) {
        MemManager::deregister_consumer(self);
    }
}

#[async_trait]
impl ShuffleRepartitioner for RssSortShuffleRepartitioner {
    async fn insert_batch(&self, input: RecordBatch) -> Result<()> {
        self.buffered_batches.lock().await.push(input.clone());

        let mem_increase = input.get_array_memory_size() + input.num_rows() * size_of::<PI>();
        self.update_mem_used_with_diff(mem_increase as isize)
            .await?;

        // we are likely to spill more frequently because the cost of spilling a shuffle
        // repartition is lower than other consumers.
        // rss shuffle spill has even lower cost than normal shuffle
        if self.mem_used_percent() > 0.25 {
            self.spill().await?;
        }
        Ok(())
    }

    async fn shuffle_write(&self) -> Result<()> {
        self.spill().await
    }
}