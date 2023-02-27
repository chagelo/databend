// Copyright 2023 Datafuse Labs.
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

use std::any::Any;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::mem::take;
use std::sync::Arc;

use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::with_hash_method;
use common_expression::BlockMetaInfo;
use common_expression::BlockMetaInfoDowncast;
use common_expression::BlockMetaInfoPtr;
use common_expression::DataBlock;
use common_expression::HashMethodKind;
use common_hashtable::hash2bucket;
use common_hashtable::HashtableLike;
use common_pipeline_core::pipe::Pipe;
use common_pipeline_core::pipe::PipeItem;
use common_pipeline_core::processors::port::InputPort;
use common_pipeline_core::processors::port::OutputPort;
use common_pipeline_core::processors::processor::Event;
use common_pipeline_core::processors::processor::ProcessorPtr;
use common_pipeline_core::processors::Processor;
use common_pipeline_core::Pipeline;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;

use crate::pipelines::processors::transforms::aggregator::aggregate_meta::AggregateMeta;
use crate::pipelines::processors::transforms::aggregator::aggregate_meta::HashTablePayload;
use crate::pipelines::processors::transforms::aggregator::AggregateInfo;
use crate::pipelines::processors::transforms::aggregator::BucketAggregator;
use crate::pipelines::processors::transforms::group_by::HashMethodBounds;
use crate::pipelines::processors::transforms::group_by::KeysColumnIter;
use crate::pipelines::processors::transforms::group_by::PartitionedHashMethod;
use crate::pipelines::processors::AggregatorParams;

static SINGLE_LEVEL_BUCKET_NUM: isize = -1;

struct InputPortState {
    port: Arc<InputPort>,
    bucket: isize,
}

pub struct TransformPartitionBucket<Method: HashMethodBounds, V: Copy + Send + Sync + 'static> {
    output: Arc<OutputPort>,
    inputs: Vec<InputPortState>,

    method: Method,
    working_bucket: isize,
    pushing_bucket: isize,
    initialized_all_inputs: bool,
    buckets_blocks: BTreeMap<isize, Vec<DataBlock>>,
    unsplitted_blocks: Vec<DataBlock>,
    _phantom: PhantomData<V>,
}

impl<Method: HashMethodBounds, V: Copy + Send + Sync + 'static>
    TransformPartitionBucket<Method, V>
{
    pub fn create(
        method: Method,
        params: Arc<AggregatorParams>,
        input_nums: usize,
    ) -> Result<Self> {
        let mut inputs = Vec::with_capacity(input_nums);

        for _index in 0..input_nums {
            inputs.push(InputPortState {
                bucket: -1,
                port: InputPort::create(),
            });
        }

        Ok(TransformPartitionBucket {
            method,
            // params,
            inputs,
            working_bucket: 0,
            pushing_bucket: 0,
            output: OutputPort::create(),
            buckets_blocks: BTreeMap::new(),
            unsplitted_blocks: vec![],
            initialized_all_inputs: false,
            _phantom: Default::default(),
        })
    }

    pub fn get_inputs(&self) -> Vec<Arc<InputPort>> {
        let mut inputs = Vec::with_capacity(self.inputs.len());

        for input_state in &self.inputs {
            inputs.push(input_state.port.clone());
        }

        inputs
    }

    pub fn get_output(&self) -> Arc<OutputPort> {
        self.output.clone()
    }

    fn initialize_all_inputs(&mut self) -> Result<bool> {
        self.initialized_all_inputs = true;

        for index in 0..self.inputs.len() {
            if self.inputs[index].port.is_finished() {
                continue;
            }

            // We pull the first unsplitted data block
            if self.inputs[index].bucket > SINGLE_LEVEL_BUCKET_NUM {
                continue;
            }

            if !self.inputs[index].port.has_data() {
                self.inputs[index].port.set_need_data();
                self.initialized_all_inputs = false;
                continue;
            }

            let data_block = self.inputs[index].port.pull_data().unwrap()?;
            self.inputs[index].bucket = self.add_bucket(data_block);

            if self.inputs[index].bucket <= SINGLE_LEVEL_BUCKET_NUM {
                self.inputs[index].port.set_need_data();
                self.initialized_all_inputs = false;
            }
        }

        Ok(self.initialized_all_inputs)
    }

    fn add_bucket(&mut self, data_block: DataBlock) -> isize {
        if let Some(block_meta) = data_block.get_meta() {
            if let Some(block_meta) = AggregateMeta::<Method, V>::downcast_ref_from(block_meta) {
                match block_meta {
                    AggregateMeta::Partitioned { .. } => unreachable!(),
                    AggregateMeta::HashTable(hashtable_payload) => {
                        let bucket = hashtable_payload.bucket;

                        if bucket > SINGLE_LEVEL_BUCKET_NUM {
                            match self.buckets_blocks.entry(bucket) {
                                Entry::Vacant(v) => {
                                    v.insert(vec![data_block]);
                                }
                                Entry::Occupied(mut v) => {
                                    v.get_mut().push(data_block);
                                }
                            };

                            return bucket;
                        }
                    }
                }
            }
        }

        self.unsplitted_blocks.push(data_block);
        SINGLE_LEVEL_BUCKET_NUM
    }

    fn try_push_data_block(&mut self) -> bool {
        match self.buckets_blocks.is_empty() {
            true => self.try_push_single_level(),
            false => self.try_push_two_level(),
        }
    }

    fn try_push_two_level(&mut self) -> bool {
        while self.pushing_bucket < self.working_bucket {
            if let Some(bucket_blocks) = self.buckets_blocks.remove(&self.pushing_bucket) {
                let data_block = Self::convert_blocks(self.pushing_bucket, bucket_blocks);
                self.output.push_data(Ok(data_block));
                self.pushing_bucket += 1;
                return true;
            }

            self.pushing_bucket += 1;
        }

        false
    }

    fn try_push_single_level(&mut self) -> bool {
        if !self.unsplitted_blocks.is_empty() {
            let data_blocks = take(&mut self.unsplitted_blocks);
            self.output.push_data(Ok(Self::convert_blocks(
                SINGLE_LEVEL_BUCKET_NUM,
                data_blocks,
            )));
            return true;
        }

        false
    }

    fn convert_blocks(bucket: isize, data_blocks: Vec<DataBlock>) -> DataBlock {
        let mut data = Vec::with_capacity(data_blocks.len());
        for mut data_block in data_blocks.into_iter() {
            if let Some(block_meta) = data_block.take_meta() {
                if let Some(block_meta) = AggregateMeta::<Method, V>::downcast_from(block_meta) {
                    data.push(block_meta);
                }
            }
        }

        DataBlock::empty_with_meta(AggregateMeta::<Method, V>::create_partitioned(bucket, data))
    }

    fn partition_hashtable(
        &self,
        payload: HashTablePayload<Method::HashTable<V>>,
    ) -> Result<Vec<Option<DataBlock>>> {
        let mut data_blocks = Vec::with_capacity(256);
        let temp = PartitionedHashMethod::convert_hashtable(&self.method, payload.hashtable)?;
        for (bucket, hashtable) in temp.into_iter_tables().enumerate() {
            data_blocks.push(match hashtable.len() == 0 {
                true => None,
                false => Some(DataBlock::empty_with_meta(
                    AggregateMeta::<Method, V>::create_hashtable(
                        bucket as isize,
                        hashtable,
                        payload.arena_holder.clone(),
                    ),
                )),
            })
        }

        Ok(data_blocks)
    }
}

#[async_trait::async_trait]
impl<Method: HashMethodBounds, V: Copy + Send + Sync + 'static> Processor
    for TransformPartitionBucket<Method, V>
{
    fn name(&self) -> String {
        String::from("TransformPartitionBucket")
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn event(&mut self) -> Result<Event> {
        if self.output.is_finished() {
            for input_state in &self.inputs {
                input_state.port.finish();
            }

            self.buckets_blocks.clear();
            return Ok(Event::Finished);
        }

        // We pull the first unsplitted data block
        if !self.initialized_all_inputs && !self.initialize_all_inputs()? {
            return Ok(Event::NeedData);
        }

        if !self.buckets_blocks.is_empty() && !self.unsplitted_blocks.is_empty() {
            // Split data blocks if it's unsplitted.
            return Ok(Event::Sync);
        }

        if !self.output.can_push() {
            for input_state in &self.inputs {
                input_state.port.set_not_need_data();
            }

            return Ok(Event::NeedConsume);
        }

        let pushed_data_block = self.try_push_data_block();

        loop {
            // Try to pull the next data or until the port is closed
            let mut all_inputs_is_finished = true;
            let mut all_port_prepared_data = true;

            for index in 0..self.inputs.len() {
                if self.inputs[index].port.is_finished() {
                    continue;
                }

                all_inputs_is_finished = false;
                if self.inputs[index].bucket > self.working_bucket {
                    continue;
                }

                if !self.inputs[index].port.has_data() {
                    all_port_prepared_data = false;
                    self.inputs[index].port.set_need_data();
                    continue;
                }

                let data_block = self.inputs[index].port.pull_data().unwrap()?;
                self.inputs[index].bucket = self.add_bucket(data_block);
                debug_assert!(self.unsplitted_blocks.is_empty());

                if self.inputs[index].bucket <= self.working_bucket {
                    all_port_prepared_data = false;
                    self.inputs[index].port.set_need_data();
                }
            }

            if all_inputs_is_finished {
                break;
            }

            if !all_port_prepared_data {
                return Ok(Event::NeedData);
            }

            self.working_bucket += 1;
        }

        if pushed_data_block || self.try_push_data_block() {
            return Ok(Event::NeedConsume);
        }

        if let Some((bucket, bucket_blocks)) = self.buckets_blocks.pop_first() {
            let data_block = Self::convert_blocks(bucket, bucket_blocks);
            self.output.push_data(Ok(data_block));
            return Ok(Event::NeedConsume);
        }

        self.output.finish();
        Ok(Event::Finished)
    }

    fn process(&mut self) -> Result<()> {
        if let Some(mut data_block) = self.unsplitted_blocks.pop() {
            if let Some(block_meta) = data_block.take_meta() {
                if let Some(block_meta) = AggregateMeta::<Method, V>::downcast_from(block_meta) {
                    let data_blocks = match block_meta {
                        AggregateMeta::Partitioned { .. } => unreachable!(),
                        AggregateMeta::HashTable(payload) => self.partition_hashtable(payload)?,
                    };

                    for (bucket, block) in data_blocks.into_iter().enumerate() {
                        if let Some(data_block) = block {
                            match self.buckets_blocks.entry(bucket as isize) {
                                Entry::Vacant(v) => {
                                    v.insert(vec![data_block]);
                                }
                                Entry::Occupied(mut v) => {
                                    v.get_mut().push(data_block);
                                }
                            };
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
