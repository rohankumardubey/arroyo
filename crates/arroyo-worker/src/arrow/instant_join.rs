use super::sync::streams::KeyedCloneableStreamFuture;
use anyhow::Result;
use arrow::compute::{max, min, partition, sort_to_indices, take};
use arrow_array::{RecordBatch, TimestampNanosecondArray};
use arroyo_operator::context::{Collector, OperatorContext};
use arroyo_operator::operator::{
    ArrowOperator, ConstructedOperator, DisplayableOperator, OperatorConstructor, Registry,
};
use arroyo_planner::physical::{ArroyoPhysicalExtensionCodec, DecodingContext};
use arroyo_rpc::{
    df::{ArroyoSchema, ArroyoSchemaRef},
    grpc::{api, rpc::TableConfig},
};
use arroyo_state::timestamp_table_config;
use arroyo_types::{from_nanos, print_time, CheckpointBarrier, Watermark};
use datafusion::execution::context::SessionContext;
use datafusion::execution::{
    runtime_env::{RuntimeConfig, RuntimeEnv},
    SendableRecordBatchStream,
};
use datafusion::physical_plan::ExecutionPlan;
use datafusion_proto::{physical_plan::AsExecutionPlan, protobuf::PhysicalPlanNode};
use futures::StreamExt;
use futures::{lock::Mutex, stream::FuturesUnordered, Future};
use prost::Message;
use std::borrow::Cow;
use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    pin::Pin,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::debug;
type NextBatchFuture<K> = KeyedCloneableStreamFuture<K, SendableRecordBatchStream>;

pub struct InstantJoin {
    left_input_schema: ArroyoSchemaRef,
    right_input_schema: ArroyoSchemaRef,
    execs: BTreeMap<SystemTime, InstantComputeHolder>,
    futures: Arc<Mutex<FuturesUnordered<NextBatchFuture<SystemTime>>>>,
    left_receiver: Arc<RwLock<Option<UnboundedReceiver<RecordBatch>>>>,
    right_receiver: Arc<RwLock<Option<UnboundedReceiver<RecordBatch>>>>,
    join_exec: Arc<dyn ExecutionPlan>,
}

struct InstantComputeHolder {
    active_exec: NextBatchFuture<SystemTime>,
    left_sender: UnboundedSender<RecordBatch>,
    right_sender: UnboundedSender<RecordBatch>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Side {
    Left,
    Right,
}

impl Side {
    fn name(&self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
        }
    }
}

impl InstantComputeHolder {
    fn insert(&mut self, batch: RecordBatch, side: Side) -> Result<()> {
        match side {
            Side::Left => self.left_sender.send(batch)?,
            Side::Right => self.right_sender.send(batch)?,
        }
        Ok(())
    }
}

impl InstantJoin {
    fn input_schema(&mut self, side: Side) -> ArroyoSchemaRef {
        match side {
            Side::Left => self.left_input_schema.clone(),
            Side::Right => self.right_input_schema.clone(),
        }
    }
    async fn get_or_insert_exec(&mut self, time: SystemTime) -> Result<&mut InstantComputeHolder> {
        if let std::collections::btree_map::Entry::Vacant(e) = self.execs.entry(time) {
            let (left_sender, left_receiver) = unbounded_channel();
            let (right_sender, right_receiver) = unbounded_channel();
            self.left_receiver.write().unwrap().replace(left_receiver);
            self.right_receiver.write().unwrap().replace(right_receiver);
            self.join_exec.reset()?;

            let new_exec = self
                .join_exec
                .execute(0, SessionContext::new().task_ctx())?;
            let next_batch_future = NextBatchFuture::new(time, new_exec);
            self.futures.lock().await.push(next_batch_future.clone());
            let exec = InstantComputeHolder {
                active_exec: next_batch_future,
                left_sender,
                right_sender,
            };
            e.insert(exec);
        }
        Ok(self.execs.get_mut(&time).unwrap())
    }

    async fn process_side(
        &mut self,
        side: Side,
        batch: RecordBatch,
        ctx: &mut OperatorContext,
    ) -> Result<()> {
        let table = ctx
            .table_manager
            .get_expiring_time_key_table(side.name(), ctx.last_present_watermark())
            .await
            .expect("should have table");

        let time_column = batch
            .column(self.input_schema(side).timestamp_index)
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .expect("should have timestamp column");
        let max_timestamp = max(time_column).expect("should have max timestamp");
        table.insert(from_nanos(max_timestamp as u128), batch.clone());
        let min_timestamp = min(time_column).expect("should have min timestamp");
        if ctx
            .last_present_watermark()
            .map(|watermark| watermark > from_nanos(min_timestamp as u128))
            .unwrap_or(false)
        {
            panic!(
                "shouldn't have a batch with timestamp {} before the watermark {:?}",
                min_timestamp,
                ctx.last_present_watermark().map(print_time)
            );
        }
        let batch = self.input_schema(side).unkeyed_batch(&batch)?;
        // We expect that a record batch will usually only be a single timestamp, so we special case that.
        if max_timestamp == min_timestamp {
            let exec = self
                .get_or_insert_exec(from_nanos(max_timestamp as u128))
                .await?;
            exec.insert(batch, side)?;
            return Ok(());
        }
        // otherwise, partition by time and send to the appropriate exec
        let indices = sort_to_indices(time_column, None, None).expect("should be able to sort");
        let columns = batch
            .columns()
            .iter()
            .map(|c| take(c, &indices, None).unwrap())
            .collect();
        let sorted = RecordBatch::try_new(batch.schema(), columns).unwrap();
        let sorted_timestamps = take(time_column, &indices, None).unwrap();
        let ranges = partition(&[sorted_timestamps.clone()]).unwrap().ranges();
        let typed_timestamps = sorted_timestamps
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .expect("should be able to downcast");
        for range in ranges {
            let batch = sorted.slice(range.start, range.end - range.start);
            let time = from_nanos(typed_timestamps.value(range.start) as u128);
            let exec = self.get_or_insert_exec(time).await?;
            exec.insert(batch, side)?;
        }
        Ok(())
    }
    async fn process_left(
        &mut self,
        record_batch: RecordBatch,
        ctx: &mut OperatorContext,
    ) -> Result<()> {
        self.process_side(Side::Left, record_batch, ctx).await
    }

    async fn process_right(
        &mut self,
        right_batch: RecordBatch,
        ctx: &mut OperatorContext,
    ) -> Result<()> {
        self.process_side(Side::Right, right_batch, ctx).await
    }
}

type PolledFutureT = <NextBatchFuture<SystemTime> as Future>::Output;

#[async_trait::async_trait]
impl ArrowOperator for InstantJoin {
    fn name(&self) -> String {
        "InstantJoin".to_string()
    }

    fn display(&self) -> DisplayableOperator {
        DisplayableOperator {
            name: Cow::Borrowed("InstantJoin"),
            fields: vec![("join_execution_plan", self.join_exec.as_ref().into())],
        }
    }

    async fn on_start(&mut self, ctx: &mut OperatorContext) {
        let watermark = ctx.last_present_watermark();
        let left_table = ctx
            .table_manager
            .get_expiring_time_key_table("left", watermark)
            .await
            .expect("should have left table");
        let left_batches: Vec<_> = left_table
            .all_batches_for_watermark(watermark)
            .flat_map(|(_time, batches)| batches.clone())
            .collect();
        for batch in left_batches {
            self.process_left(batch.clone(), ctx)
                .await
                .expect("should be able to add left from state");
        }
        let right_table = ctx
            .table_manager
            .get_expiring_time_key_table("right", watermark)
            .await
            .expect("should have right table");
        let right_batches: Vec<_> = right_table
            .all_batches_for_watermark(watermark)
            .flat_map(|(_time, batches)| batches.clone())
            .collect();
        for batch in right_batches {
            self.process_right(batch.clone(), ctx)
                .await
                .expect("should be able to add right from state");
        }
    }

    async fn process_batch(
        &mut self,
        _: RecordBatch,
        _: &mut OperatorContext,
        _: &mut dyn Collector,
    ) {
        unreachable!();
    }

    async fn process_batch_index(
        &mut self,
        index: usize,
        total_inputs: usize,
        record_batch: RecordBatch,
        ctx: &mut OperatorContext,
        _: &mut dyn Collector,
    ) {
        match index / (total_inputs / 2) {
            0 => self
                .process_left(record_batch, ctx)
                .await
                .expect("should process left"),
            1 => self
                .process_right(record_batch, ctx)
                .await
                .expect("should process right"),
            _ => unreachable!(),
        }
    }
    async fn handle_watermark(
        &mut self,
        watermark: Watermark,
        ctx: &mut OperatorContext,
        collector: &mut dyn Collector,
    ) -> Option<Watermark> {
        let Some(watermark) = ctx.last_present_watermark() else {
            return Some(watermark);
        };
        let futures_to_drain = {
            let mut futures_to_drain = vec![];
            while !self.execs.is_empty() {
                let first_watermark = self.execs.first_key_value().unwrap().0;
                if *first_watermark >= watermark {
                    break;
                }
                let (_time, exec) = self.execs.pop_first().expect("should have exec");
                futures_to_drain.push(exec.active_exec);
            }
            futures_to_drain
        };
        for mut future in futures_to_drain {
            while let (_time, Some((batch, new_exec))) = future.await {
                match batch {
                    Ok(batch) => {
                        collector.collect(batch).await;
                    }
                    Err(err) => {
                        panic!("error in future: {err:?}");
                    }
                }
                future = new_exec;
            }
        }
        Some(Watermark::EventTime(watermark))
    }

    async fn handle_checkpoint(
        &mut self,
        _: CheckpointBarrier,
        ctx: &mut OperatorContext,
        _: &mut dyn Collector,
    ) {
        let watermark = ctx.last_present_watermark();
        ctx.table_manager
            .get_expiring_time_key_table("left", watermark)
            .await
            .expect("should have left table")
            .flush(watermark)
            .await
            .expect("should flush");
        ctx.table_manager
            .get_expiring_time_key_table("right", watermark)
            .await
            .expect("should have right table")
            .flush(watermark)
            .await
            .expect("should flush");
    }

    fn tables(&self) -> HashMap<String, TableConfig> {
        let mut tables = HashMap::new();
        tables.insert(
            "left".to_string(),
            timestamp_table_config(
                "left",
                "left join data",
                Duration::ZERO,
                false,
                self.left_input_schema.as_ref().clone(),
            ),
        );
        tables.insert(
            "right".to_string(),
            timestamp_table_config(
                "right",
                "right join data",
                Duration::ZERO,
                false,
                self.right_input_schema.as_ref().clone(),
            ),
        );
        tables
    }

    fn future_to_poll(
        &mut self,
    ) -> Option<Pin<Box<dyn Future<Output = Box<dyn Any + Send>> + Send>>> {
        if self.futures.try_lock().unwrap().is_empty() {
            return None;
        }
        let future = self.futures.clone();
        Some(Box::pin(async move {
            let result: Option<PolledFutureT> = future.lock().await.next().await;
            Box::new(result) as Box<dyn Any + Send>
        }))
    }

    async fn handle_future_result(
        &mut self,
        result: Box<dyn Any + Send>,
        _: &mut OperatorContext,
        collector: &mut dyn Collector,
    ) {
        let data: Box<Option<PolledFutureT>> = result.downcast().expect("invalid data in future");
        if let Some((bin, batch_option)) = *data {
            match batch_option {
                None => {
                    debug!("future for {} was finished elsewhere", print_time(bin));
                }
                Some((batch, future)) => match self.execs.get_mut(&bin) {
                    Some(exec) => {
                        exec.active_exec = future.clone();
                        collector
                            .collect(batch.expect("should compute batch in future"))
                            .await;
                        self.futures.lock().await.push(future);
                    }
                    None => unreachable!(
                        "FuturesUnordered returned a batch, but we can't find the exec"
                    ),
                },
            }
        }
    }
}

pub struct InstantJoinConstructor;
impl OperatorConstructor for InstantJoinConstructor {
    type ConfigT = api::JoinOperator;
    fn with_config(
        &self,
        config: Self::ConfigT,
        registry: Arc<Registry>,
    ) -> anyhow::Result<ConstructedOperator> {
        let join_physical_plan_node = PhysicalPlanNode::decode(&mut config.join_plan.as_slice())?;

        let left_input_schema: Arc<ArroyoSchema> =
            Arc::new(config.left_schema.unwrap().try_into()?);
        let right_input_schema: Arc<ArroyoSchema> =
            Arc::new(config.right_schema.unwrap().try_into()?);

        let left_receiver = Arc::new(RwLock::new(None));
        let right_receiver = Arc::new(RwLock::new(None));

        let codec = ArroyoPhysicalExtensionCodec {
            context: DecodingContext::LockedJoinStream {
                left: left_receiver.clone(),
                right: right_receiver.clone(),
            },
        };
        let join_exec = join_physical_plan_node.try_into_physical_plan(
            registry.as_ref(),
            &RuntimeEnv::try_new(RuntimeConfig::new())?,
            &codec,
        )?;

        Ok(ConstructedOperator::from_operator(Box::new(InstantJoin {
            left_input_schema,
            right_input_schema,
            execs: BTreeMap::new(),
            futures: Arc::new(Mutex::new(FuturesUnordered::new())),
            left_receiver,
            right_receiver,
            join_exec,
        })))
    }
}
