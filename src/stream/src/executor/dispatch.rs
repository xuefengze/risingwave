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

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::future::Future;
use std::iter::repeat_with;
use std::sync::Arc;

use await_tree::InstrumentAwait;
use futures::Stream;
use futures_async_stream::try_stream;
use itertools::Itertools;
use risingwave_common::array::{Op, StreamChunk};
use risingwave_common::buffer::BitmapBuilder;
use risingwave_common::hash::{ActorMapping, ExpandedActorMapping, VirtualNode};
use risingwave_common::row::Row;
use risingwave_common::types::ScalarRefImpl;
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_pb::stream_plan::update_mutation::PbDispatcherUpdate;
use risingwave_pb::stream_plan::PbDispatcher;
use smallvec::{smallvec, SmallVec};
use tokio::time::Instant;
use tracing::{event, Instrument};

use super::exchange::output::{new_output, BoxedOutput};
use super::Watermark;
use crate::error::StreamResult;
use crate::executor::monitor::StreamingMetrics;
use crate::executor::{Barrier, BoxedExecutor, Message, Mutation, StreamConsumer};
use crate::task::{ActorId, DispatcherId, SharedContext};

/// [`DispatchExecutor`] consumes messages and send them into downstream actors. Usually,
/// data chunks will be dispatched with some specified policy, while control message
/// such as barriers will be distributed to all receivers.
pub struct DispatchExecutor {
    input: BoxedExecutor,
    inner: DispatchExecutorInner,
}

struct DispatchExecutorInner {
    dispatchers: Vec<DispatcherImpl>,
    actor_id: u32,
    actor_id_str: String,
    fragment_id_str: String,
    context: Arc<SharedContext>,
    metrics: Arc<StreamingMetrics>,
}

impl DispatchExecutorInner {
    fn single_inner_mut(&mut self) -> &mut DispatcherImpl {
        assert_eq!(
            self.dispatchers.len(),
            1,
            "only support mutation on one-dispatcher actors"
        );
        &mut self.dispatchers[0]
    }

    async fn dispatch(&mut self, msg: Message) -> StreamResult<()> {
        match msg {
            Message::Watermark(watermark) => {
                for dispatcher in &mut self.dispatchers {
                    let start_time = Instant::now();
                    dispatcher.dispatch_watermark(watermark.clone()).await?;
                    self.metrics
                        .actor_output_buffer_blocking_duration_ns
                        .with_label_values(&[
                            &self.actor_id_str,
                            &self.fragment_id_str,
                            dispatcher.dispatcher_id_str(),
                        ])
                        .inc_by(start_time.elapsed().as_nanos() as u64);
                }
            }
            Message::Chunk(chunk) => {
                self.metrics
                    .actor_out_record_cnt
                    .with_label_values(&[&self.actor_id_str, &self.fragment_id_str])
                    .inc_by(chunk.cardinality() as _);
                if self.dispatchers.len() == 1 {
                    // special clone optimization when there is only one downstream dispatcher
                    let start_time = Instant::now();
                    self.single_inner_mut().dispatch_data(chunk).await?;
                    self.metrics
                        .actor_output_buffer_blocking_duration_ns
                        .with_label_values(&[
                            &self.actor_id_str,
                            &self.fragment_id_str,
                            self.dispatchers[0].dispatcher_id_str(),
                        ])
                        .inc_by(start_time.elapsed().as_nanos() as u64);
                } else {
                    for dispatcher in &mut self.dispatchers {
                        let start_time = Instant::now();
                        dispatcher.dispatch_data(chunk.clone()).await?;
                        self.metrics
                            .actor_output_buffer_blocking_duration_ns
                            .with_label_values(&[
                                &self.actor_id_str,
                                &self.fragment_id_str,
                                dispatcher.dispatcher_id_str(),
                            ])
                            .inc_by(start_time.elapsed().as_nanos() as u64);
                    }
                }
            }
            Message::Barrier(barrier) => {
                let mutation = barrier.mutation.clone();
                self.pre_mutate_dispatchers(&mutation)?;
                for dispatcher in &mut self.dispatchers {
                    let start_time = Instant::now();
                    dispatcher.dispatch_barrier(barrier.clone()).await?;
                    self.metrics
                        .actor_output_buffer_blocking_duration_ns
                        .with_label_values(&[
                            &self.actor_id_str,
                            &self.fragment_id_str,
                            dispatcher.dispatcher_id_str(),
                        ])
                        .inc_by(start_time.elapsed().as_nanos() as u64);
                }
                self.post_mutate_dispatchers(&mutation)?;
            }
        };
        Ok(())
    }

    /// Add new dispatchers to the executor. Will check whether their ids are unique.
    fn add_dispatchers<'a>(
        &mut self,
        new_dispatchers: impl IntoIterator<Item = &'a PbDispatcher>,
    ) -> StreamResult<()> {
        let new_dispatchers: Vec<_> = new_dispatchers
            .into_iter()
            .map(|d| DispatcherImpl::new(&self.context, self.actor_id, d))
            .try_collect()?;

        self.dispatchers.extend(new_dispatchers);

        assert!(
            self.dispatchers
                .iter()
                .map(|d| d.dispatcher_id())
                .all_unique(),
            "dispatcher ids must be unique: {:?}",
            self.dispatchers
        );

        Ok(())
    }

    fn find_dispatcher(&mut self, dispatcher_id: DispatcherId) -> &mut DispatcherImpl {
        self.dispatchers
            .iter_mut()
            .find(|d| d.dispatcher_id() == dispatcher_id)
            .unwrap_or_else(|| panic!("dispatcher {}:{} not found", self.actor_id, dispatcher_id))
    }

    /// Update the dispatcher BEFORE we actually dispatch this barrier. We'll only add the new
    /// outputs.
    fn pre_update_dispatcher(&mut self, update: &PbDispatcherUpdate) -> StreamResult<()> {
        let outputs: Vec<_> = update
            .added_downstream_actor_id
            .iter()
            .map(|&id| new_output(&self.context, self.actor_id, id))
            .try_collect()?;

        let dispatcher = self.find_dispatcher(update.dispatcher_id);
        dispatcher.add_outputs(outputs);

        Ok(())
    }

    /// Update the dispatcher AFTER we dispatch this barrier. We'll remove some outputs and finally
    /// update the hash mapping.
    fn post_update_dispatcher(&mut self, update: &PbDispatcherUpdate) -> StreamResult<()> {
        let ids = update.removed_downstream_actor_id.iter().copied().collect();

        let dispatcher = self.find_dispatcher(update.dispatcher_id);
        dispatcher.remove_outputs(&ids);

        // The hash mapping is only used by the hash dispatcher.
        //
        // We specify a single upstream hash mapping for scaling the downstream fragment. However,
        // it's possible that there're multiple upstreams with different exchange types, for
        // example, the `Broadcast` inner side of the dynamic filter. There're too many combinations
        // to handle here, so we just ignore the `hash_mapping` field for any other exchange types.
        if let DispatcherImpl::Hash(dispatcher) = dispatcher {
            dispatcher.hash_mapping =
                ActorMapping::from_protobuf(update.get_hash_mapping()?).to_expanded();
        }

        Ok(())
    }

    /// For `Add` and `Update`, update the dispatchers before we dispatch the barrier.
    fn pre_mutate_dispatchers(&mut self, mutation: &Option<Arc<Mutation>>) -> StreamResult<()> {
        let Some(mutation) = mutation.as_deref() else {
            return Ok(());
        };

        match mutation {
            Mutation::Add { adds, .. } => {
                if let Some(new_dispatchers) = adds.get(&self.actor_id) {
                    self.add_dispatchers(new_dispatchers)?;
                }
            }
            Mutation::Update {
                dispatchers,
                actor_new_dispatchers: actor_dispatchers,
                ..
            } => {
                if let Some(new_dispatchers) = actor_dispatchers.get(&self.actor_id) {
                    self.add_dispatchers(new_dispatchers)?;
                }

                if let Some(updates) = dispatchers.get(&self.actor_id) {
                    for update in updates {
                        self.pre_update_dispatcher(update)?;
                    }
                }
            }
            _ => {}
        };

        Ok(())
    }

    /// For `Stop` and `Update`, update the dispatchers after we dispatch the barrier.
    fn post_mutate_dispatchers(&mut self, mutation: &Option<Arc<Mutation>>) -> StreamResult<()> {
        let Some(mutation) = mutation.as_deref() else {
            return Ok(());
        };

        match mutation {
            Mutation::Stop(stops) => {
                // Remove outputs only if this actor itself is not to be stopped.
                if !stops.contains(&self.actor_id) {
                    for dispatcher in &mut self.dispatchers {
                        dispatcher.remove_outputs(stops);
                    }
                }
            }
            Mutation::Update {
                dispatchers,
                dropped_actors,
                ..
            } => {
                if let Some(updates) = dispatchers.get(&self.actor_id) {
                    for update in updates {
                        self.post_update_dispatcher(update)?;
                    }
                }

                if !dropped_actors.contains(&self.actor_id) {
                    for dispatcher in &mut self.dispatchers {
                        dispatcher.remove_outputs(dropped_actors);
                    }
                }
            }

            _ => {}
        };

        // After stopping the downstream mview, the outputs of some dispatcher might be empty and we
        // should clean up them.
        self.dispatchers.retain(|d| !d.is_empty());

        Ok(())
    }
}

impl DispatchExecutor {
    pub fn new(
        input: BoxedExecutor,
        dispatchers: Vec<DispatcherImpl>,
        actor_id: u32,
        fragment_id: u32,
        context: Arc<SharedContext>,
        metrics: Arc<StreamingMetrics>,
    ) -> Self {
        Self {
            input,
            inner: DispatchExecutorInner {
                dispatchers,
                actor_id,
                actor_id_str: actor_id.to_string(),
                fragment_id_str: fragment_id.to_string(),
                context,
                metrics,
            },
        }
    }
}

impl StreamConsumer for DispatchExecutor {
    type BarrierStream = impl Stream<Item = StreamResult<Barrier>> + Send;

    fn execute(mut self: Box<Self>) -> Self::BarrierStream {
        #[try_stream]
        async move {
            let input = self.input.execute();

            #[for_await]
            for msg in input {
                let msg: Message = msg?;
                let (barrier, span) = match msg {
                    Message::Chunk(_) => (None, "dispatch_chunk"),
                    Message::Barrier(ref barrier) => (Some(barrier.clone()), "dispatch_barrier"),
                    Message::Watermark(_) => (None, "dispatch_watermark"),
                };

                let tracing_span = if let Some(_barrier) = &barrier {
                    tracing::info_span!("dispatch_barrier")
                } else {
                    tracing::Span::none()
                };

                self.inner
                    .dispatch(msg)
                    .instrument(tracing_span)
                    .instrument_await(span)
                    .await?;
                if let Some(barrier) = barrier {
                    yield barrier;
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum DispatcherImpl {
    Hash(HashDataDispatcher),
    Broadcast(BroadcastDispatcher),
    Simple(SimpleDispatcher),
    RoundRobin(RoundRobinDataDispatcher),
    CdcTableName(CdcTableNameDispatcher),
}

impl DispatcherImpl {
    pub fn new(
        context: &SharedContext,
        actor_id: ActorId,
        dispatcher: &PbDispatcher,
    ) -> StreamResult<Self> {
        let outputs = dispatcher
            .downstream_actor_id
            .iter()
            .map(|&down_id| new_output(context, actor_id, down_id))
            .collect::<StreamResult<Vec<_>>>()?;

        let output_indices = dispatcher
            .output_indices
            .iter()
            .map(|&i| i as usize)
            .collect_vec();

        use risingwave_pb::stream_plan::DispatcherType::*;
        let dispatcher_impl = match dispatcher.get_type()? {
            Hash => {
                assert!(!outputs.is_empty());
                let dist_key_indices = dispatcher
                    .dist_key_indices
                    .iter()
                    .map(|i| *i as usize)
                    .collect();

                let hash_mapping =
                    ActorMapping::from_protobuf(dispatcher.get_hash_mapping()?).to_expanded();

                DispatcherImpl::Hash(HashDataDispatcher::new(
                    outputs,
                    dist_key_indices,
                    output_indices,
                    hash_mapping,
                    dispatcher.dispatcher_id,
                ))
            }
            CdcTablename => {
                assert!(!outputs.is_empty());
                assert!(dispatcher.downstream_table_name.is_some());
                let dist_key_indices: Vec<usize> = dispatcher
                    .dist_key_indices
                    .iter()
                    .map(|i| *i as usize)
                    .collect_vec();

                assert_eq!(
                    dist_key_indices.len(),
                    1,
                    "expect only one table name column index"
                );
                DispatcherImpl::CdcTableName(CdcTableNameDispatcher::new(
                    outputs,
                    dist_key_indices[0],
                    output_indices,
                    dispatcher.dispatcher_id,
                    dispatcher.downstream_table_name.clone(),
                ))
            }

            Broadcast => DispatcherImpl::Broadcast(BroadcastDispatcher::new(
                outputs,
                output_indices,
                dispatcher.dispatcher_id,
            )),
            Simple | NoShuffle => {
                let [output]: [_; 1] = outputs.try_into().unwrap();
                DispatcherImpl::Simple(SimpleDispatcher::new(
                    output,
                    output_indices,
                    dispatcher.dispatcher_id,
                ))
            }
            Unspecified => unreachable!(),
        };

        Ok(dispatcher_impl)
    }
}

macro_rules! impl_dispatcher {
    ($( { $variant_name:ident } ),*) => {
        impl DispatcherImpl {
            pub async fn dispatch_data(&mut self, chunk: StreamChunk) -> StreamResult<()> {
                match self {
                    $( Self::$variant_name(inner) => inner.dispatch_data(chunk).await, )*
                }
            }

            pub async fn dispatch_barrier(&mut self, barrier: Barrier) -> StreamResult<()> {
                match self {
                    $( Self::$variant_name(inner) => inner.dispatch_barrier(barrier).await, )*
                }
            }

            pub async fn dispatch_watermark(&mut self, watermark: Watermark) -> StreamResult<()> {
                match self {
                    $( Self::$variant_name(inner) => inner.dispatch_watermark(watermark).await, )*
                }
            }

            pub fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
                match self {
                    $(Self::$variant_name(inner) => inner.add_outputs(outputs), )*
                }
            }

            pub fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
                match self {
                    $(Self::$variant_name(inner) => inner.remove_outputs(actor_ids), )*
                }
            }

            pub fn dispatcher_id(&self) -> DispatcherId {
                match self {
                    $(Self::$variant_name(inner) => inner.dispatcher_id(), )*
                }
            }

            pub fn dispatcher_id_str(&self) -> &str {
                match self {
                    $(Self::$variant_name(inner) => inner.dispatcher_id_str(), )*
                }
            }

            pub fn is_empty(&self) -> bool {
                match self {
                    $(Self::$variant_name(inner) => inner.is_empty(), )*
                }
            }
        }
    }
}

macro_rules! for_all_dispatcher_variants {
    ($macro:ident) => {
        $macro! {
            { Hash },
            { Broadcast },
            { Simple },
            { RoundRobin },
            { CdcTableName }
        }
    };
}

for_all_dispatcher_variants! { impl_dispatcher }

pub trait DispatchFuture<'a> = Future<Output = StreamResult<()>> + Send;

pub trait Dispatcher: Debug + 'static {
    /// Dispatch a data chunk to downstream actors.
    fn dispatch_data(&mut self, chunk: StreamChunk) -> impl DispatchFuture<'_>;
    /// Dispatch a barrier to downstream actors, generally by broadcasting it.
    fn dispatch_barrier(&mut self, barrier: Barrier) -> impl DispatchFuture<'_>;
    /// Dispatch a watermark to downstream actors, generally by broadcasting it.
    fn dispatch_watermark(&mut self, watermark: Watermark) -> impl DispatchFuture<'_>;

    /// Add new outputs to the dispatcher.
    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>);
    /// Remove outputs to `actor_ids` from the dispatcher.
    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>);

    /// The ID of the dispatcher. A [`DispatchExecutor`] may have multiple dispatchers with
    /// different IDs.
    ///
    /// Note that the dispatcher id is always equal to the downstream fragment id.
    /// See also `proto/stream_plan.proto`.
    fn dispatcher_id(&self) -> DispatcherId;

    /// Dispatcher id in string. See [`Dispatcher::dispatcher_id`].
    fn dispatcher_id_str(&self) -> &str;

    /// Whether the dispatcher has no outputs. If so, it'll be cleaned up from the
    /// [`DispatchExecutor`].
    fn is_empty(&self) -> bool;
}

#[derive(Debug)]
pub struct RoundRobinDataDispatcher {
    outputs: Vec<BoxedOutput>,
    output_indices: Vec<usize>,
    cur: usize,
    dispatcher_id: DispatcherId,
    dispatcher_id_str: String,
}

impl RoundRobinDataDispatcher {
    pub fn new(
        outputs: Vec<BoxedOutput>,
        output_indices: Vec<usize>,
        dispatcher_id: DispatcherId,
    ) -> Self {
        Self {
            outputs,
            output_indices,
            cur: 0,
            dispatcher_id,
            dispatcher_id_str: dispatcher_id.to_string(),
        }
    }
}

impl Dispatcher for RoundRobinDataDispatcher {
    async fn dispatch_data(&mut self, chunk: StreamChunk) -> StreamResult<()> {
        let chunk = chunk.project(&self.output_indices);
        self.outputs[self.cur].send(Message::Chunk(chunk)).await?;
        self.cur += 1;
        self.cur %= self.outputs.len();
        Ok(())
    }

    async fn dispatch_barrier(&mut self, barrier: Barrier) -> StreamResult<()> {
        // always broadcast barrier
        for output in &mut self.outputs {
            output.send(Message::Barrier(barrier.clone())).await?;
        }
        Ok(())
    }

    async fn dispatch_watermark(&mut self, watermark: Watermark) -> StreamResult<()> {
        if let Some(watermark) = watermark.transform_with_indices(&self.output_indices) {
            // always broadcast watermark
            for output in &mut self.outputs {
                output.send(Message::Watermark(watermark.clone())).await?;
            }
        }
        Ok(())
    }

    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.outputs.extend(outputs);
    }

    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
        self.outputs
            .extract_if(|output| actor_ids.contains(&output.actor_id()))
            .count();
        self.cur = self.cur.min(self.outputs.len() - 1);
    }

    fn dispatcher_id(&self) -> DispatcherId {
        self.dispatcher_id
    }

    fn dispatcher_id_str(&self) -> &str {
        &self.dispatcher_id_str
    }

    fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }
}

pub struct HashDataDispatcher {
    outputs: Vec<BoxedOutput>,
    keys: Vec<usize>,
    output_indices: Vec<usize>,
    /// Mapping from virtual node to actor id, used for hash data dispatcher to dispatch tasks to
    /// different downstream actors.
    hash_mapping: ExpandedActorMapping,
    dispatcher_id: DispatcherId,
    dispatcher_id_str: String,
}

impl Debug for HashDataDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashDataDispatcher")
            .field("outputs", &self.outputs)
            .field("keys", &self.keys)
            .field("dispatcher_id", &self.dispatcher_id)
            .finish_non_exhaustive()
    }
}

impl HashDataDispatcher {
    pub fn new(
        outputs: Vec<BoxedOutput>,
        keys: Vec<usize>,
        output_indices: Vec<usize>,
        hash_mapping: ExpandedActorMapping,
        dispatcher_id: DispatcherId,
    ) -> Self {
        Self {
            outputs,
            keys,
            output_indices,
            hash_mapping,
            dispatcher_id,
            dispatcher_id_str: dispatcher_id.to_string(),
        }
    }
}

impl Dispatcher for HashDataDispatcher {
    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.outputs.extend(outputs);
    }

    async fn dispatch_barrier(&mut self, barrier: Barrier) -> StreamResult<()> {
        // always broadcast barrier
        for output in &mut self.outputs {
            output.send(Message::Barrier(barrier.clone())).await?;
        }
        Ok(())
    }

    async fn dispatch_watermark(&mut self, watermark: Watermark) -> StreamResult<()> {
        if let Some(watermark) = watermark.transform_with_indices(&self.output_indices) {
            // always broadcast watermark
            for output in &mut self.outputs {
                output.send(Message::Watermark(watermark.clone())).await?;
            }
        }
        Ok(())
    }

    async fn dispatch_data(&mut self, chunk: StreamChunk) -> StreamResult<()> {
        // A chunk can be shuffled into multiple output chunks that to be sent to downstreams.
        // In these output chunks, the only difference are visibility map, which is calculated
        // by the hash value of each line in the input chunk.
        let num_outputs = self.outputs.len();

        // get hash value of every line by its key
        let vnodes = VirtualNode::compute_chunk(chunk.data_chunk(), &self.keys);

        tracing::debug!(target: "events::stream::dispatch::hash", "\n{}\n keys {:?} => {:?}", chunk.to_pretty(), self.keys, vnodes);

        let mut vis_maps = repeat_with(|| BitmapBuilder::with_capacity(chunk.capacity()))
            .take(num_outputs)
            .collect_vec();
        let mut last_vnode_when_update_delete = None;
        let mut new_ops: Vec<Op> = Vec::with_capacity(chunk.capacity());

        // Apply output indices after calculating the vnode.
        let chunk = chunk.project(&self.output_indices);

        for ((vnode, &op), visible) in vnodes
            .iter()
            .copied()
            .zip_eq_fast(chunk.ops())
            .zip_eq_fast(chunk.visibility().iter())
        {
            // Build visibility map for every output chunk.
            for (output, vis_map) in self.outputs.iter().zip_eq_fast(vis_maps.iter_mut()) {
                vis_map.append(visible && self.hash_mapping[vnode.to_index()] == output.actor_id());
            }

            if !visible {
                new_ops.push(op);
                continue;
            }

            // The 'update' message, noted by an `UpdateDelete` and a successive `UpdateInsert`,
            // need to be rewritten to common `Delete` and `Insert` if they were dispatched to
            // different actors.
            if op == Op::UpdateDelete {
                last_vnode_when_update_delete = Some(vnode);
            } else if op == Op::UpdateInsert {
                if vnode != last_vnode_when_update_delete.unwrap() {
                    new_ops.push(Op::Delete);
                    new_ops.push(Op::Insert);
                } else {
                    new_ops.push(Op::UpdateDelete);
                    new_ops.push(Op::UpdateInsert);
                }
            } else {
                new_ops.push(op);
            }
        }

        let ops = new_ops;

        // individually output StreamChunk integrated with vis_map
        for (vis_map, output) in vis_maps.into_iter().zip_eq_fast(self.outputs.iter_mut()) {
            let vis_map = vis_map.finish();
            // columns is not changed in this function
            let new_stream_chunk =
                StreamChunk::with_visibility(ops.clone(), chunk.columns().into(), vis_map);
            if new_stream_chunk.cardinality() > 0 {
                event!(
                    tracing::Level::TRACE,
                    msg = "chunk",
                    downstream = output.actor_id(),
                    "send = \n{:#?}",
                    new_stream_chunk
                );
                output.send(Message::Chunk(new_stream_chunk)).await?;
            }
        }
        Ok(())
    }

    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
        self.outputs
            .extract_if(|output| actor_ids.contains(&output.actor_id()))
            .count();
    }

    fn dispatcher_id(&self) -> DispatcherId {
        self.dispatcher_id
    }

    fn dispatcher_id_str(&self) -> &str {
        &self.dispatcher_id_str
    }

    fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }
}

/// `BroadcastDispatcher` dispatches message to all outputs.
#[derive(Debug)]
pub struct BroadcastDispatcher {
    outputs: HashMap<ActorId, BoxedOutput>,
    output_indices: Vec<usize>,
    dispatcher_id: DispatcherId,
    dispatcher_id_str: String,
}

impl BroadcastDispatcher {
    pub fn new(
        outputs: impl IntoIterator<Item = BoxedOutput>,
        output_indices: Vec<usize>,
        dispatcher_id: DispatcherId,
    ) -> Self {
        Self {
            outputs: Self::into_pairs(outputs).collect(),
            output_indices,
            dispatcher_id,
            dispatcher_id_str: dispatcher_id.to_string(),
        }
    }

    fn into_pairs(
        outputs: impl IntoIterator<Item = BoxedOutput>,
    ) -> impl Iterator<Item = (ActorId, BoxedOutput)> {
        outputs
            .into_iter()
            .map(|output| (output.actor_id(), output))
    }
}

impl Dispatcher for BroadcastDispatcher {
    async fn dispatch_data(&mut self, chunk: StreamChunk) -> StreamResult<()> {
        let chunk = chunk.project(&self.output_indices);
        for output in self.outputs.values_mut() {
            output.send(Message::Chunk(chunk.clone())).await?;
        }
        Ok(())
    }

    async fn dispatch_barrier(&mut self, barrier: Barrier) -> StreamResult<()> {
        for output in self.outputs.values_mut() {
            output.send(Message::Barrier(barrier.clone())).await?;
        }
        Ok(())
    }

    async fn dispatch_watermark(&mut self, watermark: Watermark) -> StreamResult<()> {
        if let Some(watermark) = watermark.transform_with_indices(&self.output_indices) {
            // always broadcast watermark
            for output in self.outputs.values_mut() {
                output.send(Message::Watermark(watermark.clone())).await?;
            }
        }
        Ok(())
    }

    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.outputs.extend(Self::into_pairs(outputs));
    }

    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
        self.outputs
            .extract_if(|actor_id, _| actor_ids.contains(actor_id))
            .count();
    }

    fn dispatcher_id(&self) -> DispatcherId {
        self.dispatcher_id
    }

    fn dispatcher_id_str(&self) -> &str {
        &self.dispatcher_id_str
    }

    fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }
}

/// Dispatch stream chunk based on table name from upstream DB
#[derive(Debug)]
pub struct CdcTableNameDispatcher {
    outputs: Vec<BoxedOutput>,
    // column index to the `_rw_table_name` column
    table_name_col_index: usize,
    output_indices: Vec<usize>,
    dispatcher_id: DispatcherId,
    dispatcher_id_str: String,
    downstream_table_name: Option<String>,
}

impl CdcTableNameDispatcher {
    pub fn new(
        outputs: Vec<BoxedOutput>,
        table_name_col_index: usize,
        output_indices: Vec<usize>,
        dispatcher_id: DispatcherId,
        downstream_table_name: Option<String>,
    ) -> Self {
        Self {
            outputs,
            table_name_col_index,
            output_indices,
            dispatcher_id,
            dispatcher_id_str: dispatcher_id.to_string(),
            downstream_table_name,
        }
    }
}

impl Dispatcher for CdcTableNameDispatcher {
    async fn dispatch_data(&mut self, chunk: StreamChunk) -> StreamResult<()> {
        let num_outputs = self.outputs.len();

        let mut vis_maps = repeat_with(|| BitmapBuilder::with_capacity(chunk.capacity()))
            .take(num_outputs)
            .collect_vec();

        let chunk = chunk.project(&self.output_indices);

        // TODO: use a more efficient way to filter data, e.g. add a Filter node before Chain
        for (visible, row) in chunk
            .visibility()
            .iter()
            .zip_eq_fast(chunk.data_chunk().rows_with_holes())
        {
            // Build visibility map for every output chunk.
            for vis_map in &mut vis_maps {
                let should_emit = if let Some(row) = row && let Some(full_table_name) = self.downstream_table_name.as_ref() {
                    let table_name_datum = row.datum_at(self.table_name_col_index).unwrap();
                    tracing::trace!(target: "events::stream::dispatch::hash::cdc", "keys: {:?}, table: {}", self.table_name_col_index, full_table_name);
                    // dispatch based on downstream table name
                    table_name_datum == ScalarRefImpl::Utf8(full_table_name)
                } else {
                    true
                };
                vis_map.append(visible && should_emit);
            }
        }

        for (vis_map, output) in vis_maps.into_iter().zip_eq_fast(self.outputs.iter_mut()) {
            let vis_map = vis_map.finish();
            let new_stream_chunk =
                StreamChunk::with_visibility(chunk.ops(), chunk.columns().into(), vis_map);
            if new_stream_chunk.cardinality() > 0 {
                event!(
                    tracing::Level::TRACE,
                    msg = "chunk",
                    downstream = output.actor_id(),
                    "send = \n{:#?}",
                    new_stream_chunk
                );
                output.send(Message::Chunk(new_stream_chunk)).await?;
            }
        }

        Ok(())
    }

    async fn dispatch_barrier(&mut self, barrier: Barrier) -> StreamResult<()> {
        // always broadcast barrier
        for output in &mut self.outputs {
            output.send(Message::Barrier(barrier.clone())).await?;
        }
        Ok(())
    }

    async fn dispatch_watermark(&mut self, watermark: Watermark) -> StreamResult<()> {
        if let Some(watermark) = watermark.transform_with_indices(&self.output_indices) {
            // always broadcast watermark
            for output in &mut self.outputs {
                output.send(Message::Watermark(watermark.clone())).await?;
            }
        }
        Ok(())
    }

    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.outputs.extend(outputs);
    }

    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
        self.outputs
            .extract_if(|output| actor_ids.contains(&output.actor_id()))
            .count();
    }

    fn dispatcher_id(&self) -> DispatcherId {
        self.dispatcher_id
    }

    fn dispatcher_id_str(&self) -> &str {
        &self.dispatcher_id_str
    }

    fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }
}

/// `SimpleDispatcher` dispatches message to a single output.
#[derive(Debug)]
pub struct SimpleDispatcher {
    /// In most cases, there is exactly one output. However, in some cases of configuration change,
    /// the field needs to be temporarily set to 0 or 2 outputs.
    ///
    /// - When dropping a materialized view, the output will be removed and this field becomes
    ///   empty. The [`DispatchExecutor`] will immediately clean-up this empty dispatcher before
    ///   finishing processing the current mutation.
    /// - When migrating a singleton fragment, the new output will be temporarily added in `pre`
    ///   stage and this field becomes multiple, which is for broadcasting this configuration
    ///   change barrier to both old and new downstream actors. In `post` stage, the old output
    ///   will be removed and this field becomes single again.
    ///
    /// Therefore, when dispatching data, we assert that there's exactly one output by
    /// `Self::output`.
    output: SmallVec<[BoxedOutput; 2]>,
    output_indices: Vec<usize>,
    dispatcher_id: DispatcherId,
    dispatcher_id_str: String,
}

impl SimpleDispatcher {
    pub fn new(
        output: BoxedOutput,
        output_indices: Vec<usize>,
        dispatcher_id: DispatcherId,
    ) -> Self {
        Self {
            output: smallvec![output],
            output_indices,
            dispatcher_id,
            dispatcher_id_str: dispatcher_id.to_string(),
        }
    }
}

impl Dispatcher for SimpleDispatcher {
    fn add_outputs(&mut self, outputs: impl IntoIterator<Item = BoxedOutput>) {
        self.output.extend(outputs);
        assert!(self.output.len() <= 2);
    }

    async fn dispatch_barrier(&mut self, barrier: Barrier) -> StreamResult<()> {
        // Only barrier is allowed to be dispatched to multiple outputs during migration.
        for output in &mut self.output {
            output.send(Message::Barrier(barrier.clone())).await?;
        }
        Ok(())
    }

    async fn dispatch_data(&mut self, chunk: StreamChunk) -> StreamResult<()> {
        let output = self
            .output
            .iter_mut()
            .exactly_one()
            .expect("expect exactly one output");

        let chunk = chunk.project(&self.output_indices);
        output.send(Message::Chunk(chunk)).await
    }

    async fn dispatch_watermark(&mut self, watermark: Watermark) -> StreamResult<()> {
        let output = self
            .output
            .iter_mut()
            .exactly_one()
            .expect("expect exactly one output");

        if let Some(watermark) = watermark.transform_with_indices(&self.output_indices) {
            output.send(Message::Watermark(watermark)).await?;
        }
        Ok(())
    }

    fn remove_outputs(&mut self, actor_ids: &HashSet<ActorId>) {
        self.output
            .retain(|output| !actor_ids.contains(&output.actor_id()));
    }

    fn dispatcher_id(&self) -> DispatcherId {
        self.dispatcher_id
    }

    fn dispatcher_id_str(&self) -> &str {
        &self.dispatcher_id_str
    }

    fn is_empty(&self) -> bool {
        self.output.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use futures::{pin_mut, StreamExt};
    use itertools::Itertools;
    use risingwave_common::array::stream_chunk::StreamChunkTestExt;
    use risingwave_common::array::{Array, ArrayBuilder, I32ArrayBuilder, Op};
    use risingwave_common::catalog::Schema;
    use risingwave_common::hash::VirtualNode;
    use risingwave_common::util::hash_util::Crc32FastBuilder;
    use risingwave_common::util::iter_util::ZipEqFast;
    use risingwave_pb::stream_plan::DispatcherType;

    use super::*;
    use crate::executor::exchange::output::Output;
    use crate::executor::exchange::permit::channel_for_test;
    use crate::executor::receiver::ReceiverExecutor;
    use crate::task::test_utils::helper_make_local_actor;

    #[derive(Debug)]
    pub struct MockOutput {
        actor_id: ActorId,
        data: Arc<Mutex<Vec<Message>>>,
    }

    impl MockOutput {
        pub fn new(actor_id: ActorId, data: Arc<Mutex<Vec<Message>>>) -> Self {
            Self { actor_id, data }
        }
    }

    #[async_trait]
    impl Output for MockOutput {
        async fn send(&mut self, message: Message) -> StreamResult<()> {
            self.data.lock().unwrap().push(message);
            Ok(())
        }

        fn actor_id(&self) -> ActorId {
            self.actor_id
        }
    }

    // TODO: this test contains update being shuffled to different partitions, which is not
    // supported for now.
    #[tokio::test]
    async fn test_hash_dispatcher_complex() {
        test_hash_dispatcher_complex_inner().await
    }

    async fn test_hash_dispatcher_complex_inner() {
        // This test only works when VirtualNode::COUNT is 256.
        static_assertions::const_assert_eq!(VirtualNode::COUNT, 256);

        let num_outputs = 2; // actor id ranges from 1 to 2
        let key_indices = &[0, 2];
        let output_data_vecs = (0..num_outputs)
            .map(|_| Arc::new(Mutex::new(Vec::new())))
            .collect::<Vec<_>>();
        let outputs = output_data_vecs
            .iter()
            .enumerate()
            .map(|(actor_id, data)| {
                Box::new(MockOutput::new(1 + actor_id as u32, data.clone())) as BoxedOutput
            })
            .collect::<Vec<_>>();
        let mut hash_mapping = (1..num_outputs + 1)
            .flat_map(|id| vec![id as ActorId; VirtualNode::COUNT / num_outputs])
            .collect_vec();
        hash_mapping.resize(VirtualNode::COUNT, num_outputs as u32);
        let mut hash_dispatcher = HashDataDispatcher::new(
            outputs,
            key_indices.to_vec(),
            vec![0, 1, 2],
            hash_mapping,
            0,
        );

        let chunk = StreamChunk::from_pretty(
            "  I I I
            +  4 6 8
            +  5 7 9
            +  0 0 0
            -  1 1 1 D
            U- 2 0 2
            U+ 2 0 2
            U- 3 3 2
            U+ 3 3 4",
        );
        hash_dispatcher.dispatch_data(chunk).await.unwrap();

        assert_eq!(
            *output_data_vecs[0].lock().unwrap()[0].as_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I I
                +  4 6 8
                +  5 7 9
                +  0 0 0
                -  1 1 1 D
                U- 2 0 2
                U+ 2 0 2
                -  3 3 2 D  // Should rewrite UpdateDelete to Delete
                +  3 3 4    // Should rewrite UpdateInsert to Insert",
            )
        );
        assert_eq!(
            *output_data_vecs[1].lock().unwrap()[0].as_chunk().unwrap(),
            StreamChunk::from_pretty(
                "  I I I
                +  4 6 8 D
                +  5 7 9 D
                +  0 0 0 D
                -  1 1 1 D  // Should keep original invisible mark
                U- 2 0 2 D  // Should keep UpdateDelete
                U+ 2 0 2 D  // Should keep UpdateInsert
                -  3 3 2    // Should rewrite UpdateDelete to Delete
                +  3 3 4 D  // Should rewrite UpdateInsert to Insert",
            )
        );
    }

    #[tokio::test]
    async fn test_configuration_change() {
        let _schema = Schema { fields: vec![] };
        let (tx, rx) = channel_for_test();
        let actor_id = 233;
        let fragment_id = 666;
        let input = Box::new(ReceiverExecutor::for_test(rx));
        let ctx = Arc::new(SharedContext::for_test());
        let metrics = Arc::new(StreamingMetrics::unused());

        let (untouched, old, new) = (234, 235, 238); // broadcast downstream actors
        let (old_simple, new_simple) = (114, 514); // simple downstream actors

        // 1. Register info in context.
        {
            let mut actor_infos = ctx.actor_infos.write();

            for local_actor_id in [actor_id, untouched, old, new, old_simple, new_simple] {
                actor_infos.insert(local_actor_id, helper_make_local_actor(local_actor_id));
            }
        }
        // actor_id -> untouched, old, new, old_simple, new_simple

        let broadcast_dispatcher_id = 666;
        let broadcast_dispatcher = DispatcherImpl::new(
            &ctx,
            actor_id,
            &PbDispatcher {
                r#type: DispatcherType::Broadcast as _,
                dispatcher_id: broadcast_dispatcher_id,
                downstream_actor_id: vec![untouched, old],
                ..Default::default()
            },
        )
        .unwrap();

        let simple_dispatcher_id = 888;
        let simple_dispatcher = DispatcherImpl::new(
            &ctx,
            actor_id,
            &PbDispatcher {
                r#type: DispatcherType::Simple as _,
                dispatcher_id: simple_dispatcher_id,
                downstream_actor_id: vec![old_simple],
                ..Default::default()
            },
        )
        .unwrap();

        let executor = Box::new(DispatchExecutor::new(
            input,
            vec![broadcast_dispatcher, simple_dispatcher],
            actor_id,
            fragment_id,
            ctx.clone(),
            metrics,
        ))
        .execute();
        pin_mut!(executor);

        // 2. Take downstream receivers.
        let mut rxs = [untouched, old, new, old_simple, new_simple]
            .into_iter()
            .map(|id| (id, ctx.take_receiver(&(actor_id, id)).unwrap()))
            .collect::<HashMap<_, _>>();
        macro_rules! try_recv {
            ($down_id:expr) => {
                rxs.get_mut(&$down_id).unwrap().try_recv()
            };
        }

        // 3. Send a chunk.
        tx.send(Message::Chunk(StreamChunk::default()))
            .await
            .unwrap();

        // 4. Send a configuration change barrier for broadcast dispatcher.
        let dispatcher_updates = maplit::hashmap! {
            actor_id => vec![PbDispatcherUpdate {
                actor_id,
                dispatcher_id: broadcast_dispatcher_id,
                added_downstream_actor_id: vec![new],
                removed_downstream_actor_id: vec![old],
                hash_mapping: Default::default(),
            }]
        };
        let b1 = Barrier::new_test_barrier(1).with_mutation(Mutation::Update {
            dispatchers: dispatcher_updates,
            merges: Default::default(),
            vnode_bitmaps: Default::default(),
            dropped_actors: Default::default(),
            actor_splits: Default::default(),
            actor_new_dispatchers: Default::default(),
        });
        tx.send(Message::Barrier(b1)).await.unwrap();
        executor.next().await.unwrap().unwrap();

        // 5. Check downstream.
        try_recv!(untouched).unwrap().as_chunk().unwrap();
        try_recv!(untouched).unwrap().as_barrier().unwrap();

        try_recv!(old).unwrap().as_chunk().unwrap();
        try_recv!(old).unwrap().as_barrier().unwrap(); // It should still receive the barrier even if it's to be removed.

        try_recv!(new).unwrap().as_barrier().unwrap(); // Since it's just added, it won't receive the chunk.

        try_recv!(old_simple).unwrap().as_chunk().unwrap();
        try_recv!(old_simple).unwrap().as_barrier().unwrap(); // Untouched.

        // 6. Send another barrier.
        tx.send(Message::Barrier(Barrier::new_test_barrier(2)))
            .await
            .unwrap();
        executor.next().await.unwrap().unwrap();

        // 7. Check downstream.
        try_recv!(untouched).unwrap().as_barrier().unwrap();
        try_recv!(old).unwrap_err(); // Since it's stopped, we can't receive the new messages.
        try_recv!(new).unwrap().as_barrier().unwrap();

        try_recv!(old_simple).unwrap().as_barrier().unwrap(); // Untouched.
        try_recv!(new_simple).unwrap_err(); // Untouched.

        // 8. Send another chunk.
        tx.send(Message::Chunk(StreamChunk::default()))
            .await
            .unwrap();

        // 9. Send a configuration change barrier for simple dispatcher.
        let dispatcher_updates = maplit::hashmap! {
            actor_id => vec![PbDispatcherUpdate {
                actor_id,
                dispatcher_id: simple_dispatcher_id,
                added_downstream_actor_id: vec![new_simple],
                removed_downstream_actor_id: vec![old_simple],
                hash_mapping: Default::default(),
            }]
        };
        let b3 = Barrier::new_test_barrier(3).with_mutation(Mutation::Update {
            dispatchers: dispatcher_updates,
            merges: Default::default(),
            vnode_bitmaps: Default::default(),
            dropped_actors: Default::default(),
            actor_splits: Default::default(),
            actor_new_dispatchers: Default::default(),
        });
        tx.send(Message::Barrier(b3)).await.unwrap();
        executor.next().await.unwrap().unwrap();

        // 10. Check downstream.
        try_recv!(old_simple).unwrap().as_chunk().unwrap();
        try_recv!(old_simple).unwrap().as_barrier().unwrap(); // It should still receive the barrier even if it's to be removed.

        try_recv!(new_simple).unwrap().as_barrier().unwrap(); // Since it's just added, it won't receive the chunk.

        // 11. Send another barrier.
        tx.send(Message::Barrier(Barrier::new_test_barrier(4)))
            .await
            .unwrap();
        executor.next().await.unwrap().unwrap();

        // 12. Check downstream.
        try_recv!(old_simple).unwrap_err(); // Since it's stopped, we can't receive the new messages.
        try_recv!(new_simple).unwrap().as_barrier().unwrap();
    }

    #[tokio::test]
    async fn test_hash_dispatcher() {
        let num_outputs = 5; // actor id ranges from 1 to 5
        let cardinality = 10;
        let dimension = 4;
        let key_indices = &[0, 2];
        let output_data_vecs = (0..num_outputs)
            .map(|_| Arc::new(Mutex::new(Vec::new())))
            .collect::<Vec<_>>();
        let outputs = output_data_vecs
            .iter()
            .enumerate()
            .map(|(actor_id, data)| {
                Box::new(MockOutput::new(1 + actor_id as u32, data.clone())) as BoxedOutput
            })
            .collect::<Vec<_>>();
        let mut hash_mapping = (1..num_outputs + 1)
            .flat_map(|id| vec![id as ActorId; VirtualNode::COUNT / num_outputs])
            .collect_vec();
        hash_mapping.resize(VirtualNode::COUNT, num_outputs as u32);
        let mut hash_dispatcher = HashDataDispatcher::new(
            outputs,
            key_indices.to_vec(),
            (0..dimension).collect(),
            hash_mapping.clone(),
            0,
        );

        let mut ops = Vec::new();
        for idx in 0..cardinality {
            if idx % 2 == 0 {
                ops.push(Op::Insert);
            } else {
                ops.push(Op::Delete);
            }
        }

        let mut start = 19260817i32..;
        let mut builders = (0..dimension)
            .map(|_| I32ArrayBuilder::new(cardinality))
            .collect_vec();
        let mut output_cols = vec![vec![vec![]; dimension]; num_outputs];
        let mut output_ops = vec![vec![]; num_outputs];
        for op in &ops {
            let hash_builder = Crc32FastBuilder;
            let mut hasher = hash_builder.build_hasher();
            let one_row = (0..dimension).map(|_| start.next().unwrap()).collect_vec();
            for key_idx in key_indices {
                let val = one_row[*key_idx];
                let bytes = val.to_le_bytes();
                hasher.update(&bytes);
            }
            let output_idx =
                hash_mapping[hasher.finish() as usize % VirtualNode::COUNT] as usize - 1;
            for (builder, val) in builders.iter_mut().zip_eq_fast(one_row.iter()) {
                builder.append(Some(*val));
            }
            output_cols[output_idx]
                .iter_mut()
                .zip_eq_fast(one_row.iter())
                .for_each(|(each_column, val)| each_column.push(*val));
            output_ops[output_idx].push(op);
        }

        let columns = builders
            .into_iter()
            .map(|builder| {
                let array = builder.finish();
                array.into_ref()
            })
            .collect();

        let chunk = StreamChunk::new(ops, columns);
        hash_dispatcher.dispatch_data(chunk).await.unwrap();

        for (output_idx, output) in output_data_vecs.into_iter().enumerate() {
            let guard = output.lock().unwrap();
            // It is possible that there is no chunks, as a key doesn't belong to any hash bucket.
            assert!(guard.len() <= 1);
            if guard.is_empty() {
                assert!(output_cols[output_idx].iter().all(|x| { x.is_empty() }));
            } else {
                let message = guard.get(0).unwrap();
                let real_chunk = match message {
                    Message::Chunk(chunk) => chunk,
                    _ => panic!(),
                };
                real_chunk
                    .columns()
                    .iter()
                    .zip_eq_fast(output_cols[output_idx].iter())
                    .for_each(|(real_col, expect_col)| {
                        let real_vals = real_chunk
                            .visibility()
                            .iter_ones()
                            .map(|row_idx| real_col.as_int32().value_at(row_idx).unwrap())
                            .collect::<Vec<_>>();
                        assert_eq!(real_vals.len(), expect_col.len());
                        assert_eq!(real_vals, *expect_col);
                    });
            }
        }
    }
}
