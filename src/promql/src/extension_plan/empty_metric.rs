// Copyright 2023 Greptime Team
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
use std::collections::HashMap;
use std::ops::Div;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use datafusion::arrow::array::ArrayRef;
use datafusion::arrow::datatypes::{DataType, TimeUnit};
use datafusion::common::arrow::datatypes::Field;
use datafusion::common::stats::Precision;
use datafusion::common::{
    DFSchema, DFSchemaRef, Result as DataFusionResult, Statistics, TableReference,
};
use datafusion::error::DataFusionError;
use datafusion::execution::context::{SessionState, TaskContext};
use datafusion::logical_expr::{ExprSchemable, LogicalPlan, UserDefinedLogicalNodeCore};
use datafusion::physical_expr::{EquivalenceProperties, PhysicalExprRef};
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::metrics::{BaselineMetrics, ExecutionPlanMetricsSet, MetricsSet};
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties, RecordBatchStream,
    SendableRecordBatchStream,
};
use datafusion::physical_planner::PhysicalPlanner;
use datafusion::prelude::{col, lit, Expr};
use datatypes::arrow::array::TimestampMillisecondArray;
use datatypes::arrow::datatypes::SchemaRef;
use datatypes::arrow::record_batch::RecordBatch;
use futures::Stream;

use crate::extension_plan::Millisecond;

/// Empty source plan that generate record batch with two columns:
/// - time index column, computed from start, end and interval
/// - value column, generated by the input expr. The expr should not
///   reference any column except the time index column.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EmptyMetric {
    start: Millisecond,
    end: Millisecond,
    interval: Millisecond,
    expr: Option<Expr>,
    /// Schema that only contains the time index column.
    /// This is for intermediate result only.
    time_index_schema: DFSchemaRef,
    /// Schema of the output record batch
    result_schema: DFSchemaRef,
}

impl EmptyMetric {
    pub fn new(
        start: Millisecond,
        end: Millisecond,
        interval: Millisecond,
        time_index_column_name: String,
        field_column_name: String,
        field_expr: Option<Expr>,
    ) -> DataFusionResult<Self> {
        let qualifier = Some(TableReference::bare(""));
        let ts_only_schema = build_ts_only_schema(&time_index_column_name);
        let mut fields = vec![(qualifier.clone(), Arc::new(ts_only_schema.field(0).clone()))];
        if let Some(field_expr) = &field_expr {
            let field_data_type = field_expr.get_type(&ts_only_schema)?;
            fields.push((
                qualifier.clone(),
                Arc::new(Field::new(field_column_name, field_data_type, true)),
            ));
        }
        let schema = Arc::new(DFSchema::new_with_metadata(fields, HashMap::new())?);

        Ok(Self {
            start,
            end,
            interval,
            time_index_schema: Arc::new(ts_only_schema),
            result_schema: schema,
            expr: field_expr,
        })
    }

    pub const fn name() -> &'static str {
        "EmptyMetric"
    }

    pub fn to_execution_plan(
        &self,
        session_state: &SessionState,
        physical_planner: &dyn PhysicalPlanner,
    ) -> DataFusionResult<Arc<dyn ExecutionPlan>> {
        let physical_expr = self
            .expr
            .as_ref()
            .map(|expr| {
                physical_planner.create_physical_expr(expr, &self.time_index_schema, session_state)
            })
            .transpose()?;
        let result_schema: SchemaRef = Arc::new(self.result_schema.as_ref().into());
        let properties = Arc::new(PlanProperties::new(
            EquivalenceProperties::new(result_schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        ));
        Ok(Arc::new(EmptyMetricExec {
            start: self.start,
            end: self.end,
            interval: self.interval,
            time_index_schema: Arc::new(self.time_index_schema.as_ref().into()),
            result_schema,
            expr: physical_expr,
            properties,
            metric: ExecutionPlanMetricsSet::new(),
        }))
    }
}

impl UserDefinedLogicalNodeCore for EmptyMetric {
    fn name(&self) -> &str {
        Self::name()
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![]
    }

    fn schema(&self) -> &DFSchemaRef {
        &self.result_schema
    }

    fn expressions(&self) -> Vec<Expr> {
        if let Some(expr) = &self.expr {
            vec![expr.clone()]
        } else {
            vec![]
        }
    }

    fn fmt_for_explain(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "EmptyMetric: range=[{}..{}], interval=[{}]",
            self.start, self.end, self.interval,
        )
    }

    fn with_exprs_and_inputs(
        &self,
        exprs: Vec<Expr>,
        _inputs: Vec<LogicalPlan>,
    ) -> DataFusionResult<Self> {
        Ok(Self {
            start: self.start,
            end: self.end,
            interval: self.interval,
            expr: exprs.into_iter().next(),
            time_index_schema: self.time_index_schema.clone(),
            result_schema: self.result_schema.clone(),
        })
    }
}

impl PartialOrd for EmptyMetric {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        // Compare fields in order excluding schema fields
        match self.start.partial_cmp(&other.start) {
            Some(core::cmp::Ordering::Equal) => {}
            ord => return ord,
        }
        match self.end.partial_cmp(&other.end) {
            Some(core::cmp::Ordering::Equal) => {}
            ord => return ord,
        }
        match self.interval.partial_cmp(&other.interval) {
            Some(core::cmp::Ordering::Equal) => {}
            ord => return ord,
        }
        self.expr.partial_cmp(&other.expr)
    }
}

#[derive(Debug, Clone)]
pub struct EmptyMetricExec {
    start: Millisecond,
    end: Millisecond,
    interval: Millisecond,
    /// Schema that only contains the time index column.
    /// This is for intermediate result only.
    time_index_schema: SchemaRef,
    /// Schema of the output record batch
    result_schema: SchemaRef,
    expr: Option<PhysicalExprRef>,
    properties: Arc<PlanProperties>,
    metric: ExecutionPlanMetricsSet,
}

impl ExecutionPlan for EmptyMetricExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.result_schema.clone()
    }

    fn properties(&self) -> &PlanProperties {
        self.properties.as_ref()
    }

    fn maintains_input_order(&self) -> Vec<bool> {
        vec![]
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DataFusionResult<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(self.as_ref().clone()))
    }

    fn execute(
        &self,
        partition: usize,
        _context: Arc<TaskContext>,
    ) -> DataFusionResult<SendableRecordBatchStream> {
        let baseline_metric = BaselineMetrics::new(&self.metric, partition);
        Ok(Box::pin(EmptyMetricStream {
            start: self.start,
            end: self.end,
            interval: self.interval,
            expr: self.expr.clone(),
            is_first_poll: true,
            time_index_schema: self.time_index_schema.clone(),
            result_schema: self.result_schema.clone(),
            metric: baseline_metric,
        }))
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metric.clone_inner())
    }

    fn statistics(&self) -> DataFusionResult<Statistics> {
        let estimated_row_num = (self.end - self.start) as f64 / self.interval as f64;
        let total_byte_size = estimated_row_num * std::mem::size_of::<Millisecond>() as f64;

        Ok(Statistics {
            num_rows: Precision::Inexact(estimated_row_num.floor() as _),
            total_byte_size: Precision::Inexact(total_byte_size.floor() as _),
            column_statistics: Statistics::unknown_column(&self.schema()),
        })
    }

    fn name(&self) -> &str {
        "EmptyMetricExec"
    }
}

impl DisplayAs for EmptyMetricExec {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => write!(
                f,
                "EmptyMetric: range=[{}..{}], interval=[{}]",
                self.start, self.end, self.interval,
            ),
        }
    }
}

pub struct EmptyMetricStream {
    start: Millisecond,
    end: Millisecond,
    interval: Millisecond,
    expr: Option<PhysicalExprRef>,
    /// This stream only generate one record batch at the first poll
    is_first_poll: bool,
    /// Schema that only contains the time index column.
    /// This is for intermediate result only.
    time_index_schema: SchemaRef,
    /// Schema of the output record batch
    result_schema: SchemaRef,
    metric: BaselineMetrics,
}

impl RecordBatchStream for EmptyMetricStream {
    fn schema(&self) -> SchemaRef {
        self.result_schema.clone()
    }
}

impl Stream for EmptyMetricStream {
    type Item = DataFusionResult<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = if self.is_first_poll {
            self.is_first_poll = false;
            let _timer = self.metric.elapsed_compute().timer();

            // build the time index array, and a record batch that
            // only contains that array as the input of field expr
            let time_array = (self.start..=self.end)
                .step_by(self.interval as _)
                .collect::<Vec<_>>();
            let time_array = Arc::new(TimestampMillisecondArray::from(time_array));
            let num_rows = time_array.len();
            let input_record_batch =
                RecordBatch::try_new(self.time_index_schema.clone(), vec![time_array.clone()])
                    .map_err(|e| DataFusionError::ArrowError(e, None))?;
            let mut result_arrays: Vec<ArrayRef> = vec![time_array];

            // evaluate the field expr and get the result
            if let Some(field_expr) = &self.expr {
                result_arrays.push(
                    field_expr
                        .evaluate(&input_record_batch)
                        .and_then(|x| x.into_array(num_rows))?,
                );
            }

            // assemble the output record batch
            let batch = RecordBatch::try_new(self.result_schema.clone(), result_arrays)
                .map_err(|e| DataFusionError::ArrowError(e, None));

            Poll::Ready(Some(batch))
        } else {
            Poll::Ready(None)
        };
        self.metric.record_poll(result)
    }
}

/// Build a schema that only contains **millisecond** timestamp column
fn build_ts_only_schema(column_name: &str) -> DFSchema {
    let ts_field = Field::new(
        column_name,
        DataType::Timestamp(TimeUnit::Millisecond, None),
        false,
    );
    // safety: should not fail (UT covers this)
    DFSchema::new_with_metadata(
        vec![(Some(TableReference::bare("")), Arc::new(ts_field))],
        HashMap::new(),
    )
    .unwrap()
}

// Convert timestamp column to UNIX epoch second:
// https://prometheus.io/docs/prometheus/latest/querying/functions/#time
pub fn build_special_time_expr(time_index_column_name: &str) -> Expr {
    let input_schema = build_ts_only_schema(time_index_column_name);
    // safety: should not failed (UT covers this)
    col(time_index_column_name)
        .cast_to(&DataType::Int64, &input_schema)
        .unwrap()
        .cast_to(&DataType::Float64, &input_schema)
        .unwrap()
        .div(lit(1000.0)) // cast to second will lost precision, so we cast to float64 first and manually divide by 1000
}

#[cfg(test)]
mod test {
    use datafusion::physical_planner::DefaultPhysicalPlanner;
    use datafusion::prelude::SessionContext;

    use super::*;

    async fn do_empty_metric_test(
        start: Millisecond,
        end: Millisecond,
        interval: Millisecond,
        time_column_name: String,
        field_column_name: String,
        expected: String,
    ) {
        let session_context = SessionContext::default();
        let df_default_physical_planner = DefaultPhysicalPlanner::default();
        let time_expr = build_special_time_expr(&time_column_name);
        let empty_metric = EmptyMetric::new(
            start,
            end,
            interval,
            time_column_name,
            field_column_name,
            Some(time_expr),
        )
        .unwrap();
        let empty_metric_exec = empty_metric
            .to_execution_plan(&session_context.state(), &df_default_physical_planner)
            .unwrap();

        let result =
            datafusion::physical_plan::collect(empty_metric_exec, session_context.task_ctx())
                .await
                .unwrap();
        let result_literal = datatypes::arrow::util::pretty::pretty_format_batches(&result)
            .unwrap()
            .to_string();

        assert_eq!(result_literal, expected);
    }

    #[tokio::test]
    async fn normal_empty_metric_test() {
        do_empty_metric_test(
            0,
            100,
            10,
            "time".to_string(),
            "value".to_string(),
            String::from(
                "+-------------------------+-------+\
                \n| time                    | value |\
                \n+-------------------------+-------+\
                \n| 1970-01-01T00:00:00     | 0.0   |\
                \n| 1970-01-01T00:00:00.010 | 0.01  |\
                \n| 1970-01-01T00:00:00.020 | 0.02  |\
                \n| 1970-01-01T00:00:00.030 | 0.03  |\
                \n| 1970-01-01T00:00:00.040 | 0.04  |\
                \n| 1970-01-01T00:00:00.050 | 0.05  |\
                \n| 1970-01-01T00:00:00.060 | 0.06  |\
                \n| 1970-01-01T00:00:00.070 | 0.07  |\
                \n| 1970-01-01T00:00:00.080 | 0.08  |\
                \n| 1970-01-01T00:00:00.090 | 0.09  |\
                \n| 1970-01-01T00:00:00.100 | 0.1   |\
                \n+-------------------------+-------+",
            ),
        )
        .await
    }

    #[tokio::test]
    async fn unaligned_empty_metric_test() {
        do_empty_metric_test(
            0,
            100,
            11,
            "time".to_string(),
            "value".to_string(),
            String::from(
                "+-------------------------+-------+\
                \n| time                    | value |\
                \n+-------------------------+-------+\
                \n| 1970-01-01T00:00:00     | 0.0   |\
                \n| 1970-01-01T00:00:00.011 | 0.011 |\
                \n| 1970-01-01T00:00:00.022 | 0.022 |\
                \n| 1970-01-01T00:00:00.033 | 0.033 |\
                \n| 1970-01-01T00:00:00.044 | 0.044 |\
                \n| 1970-01-01T00:00:00.055 | 0.055 |\
                \n| 1970-01-01T00:00:00.066 | 0.066 |\
                \n| 1970-01-01T00:00:00.077 | 0.077 |\
                \n| 1970-01-01T00:00:00.088 | 0.088 |\
                \n| 1970-01-01T00:00:00.099 | 0.099 |\
                \n+-------------------------+-------+",
            ),
        )
        .await
    }

    #[tokio::test]
    async fn one_row_empty_metric_test() {
        do_empty_metric_test(
            0,
            100,
            1000,
            "time".to_string(),
            "value".to_string(),
            String::from(
                "+---------------------+-------+\
                \n| time                | value |\
                \n+---------------------+-------+\
                \n| 1970-01-01T00:00:00 | 0.0   |\
                \n+---------------------+-------+",
            ),
        )
        .await
    }

    #[tokio::test]
    async fn negative_range_empty_metric_test() {
        do_empty_metric_test(
            1000,
            -1000,
            10,
            "time".to_string(),
            "value".to_string(),
            String::from(
                "+------+-------+\
                \n| time | value |\
                \n+------+-------+\
                \n+------+-------+",
            ),
        )
        .await
    }

    #[tokio::test]
    async fn no_field_expr() {
        let session_context = SessionContext::default();
        let df_default_physical_planner = DefaultPhysicalPlanner::default();
        let empty_metric =
            EmptyMetric::new(0, 200, 1000, "time".to_string(), "value".to_string(), None).unwrap();
        let empty_metric_exec = empty_metric
            .to_execution_plan(&session_context.state(), &df_default_physical_planner)
            .unwrap();

        let result =
            datafusion::physical_plan::collect(empty_metric_exec, session_context.task_ctx())
                .await
                .unwrap();
        let result_literal = datatypes::arrow::util::pretty::pretty_format_batches(&result)
            .unwrap()
            .to_string();

        let expected = String::from(
            "+---------------------+\
            \n| time                |\
            \n+---------------------+\
            \n| 1970-01-01T00:00:00 |\
            \n+---------------------+",
        );
        assert_eq!(result_literal, expected);
    }
}
