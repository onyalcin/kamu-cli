// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use datafusion::parquet::file::properties::WriterProperties;
use datafusion::prelude::*;
use internal_error::*;
use kamu_core::ingest::*;
use kamu_core::*;
use kamu_data_utils::data::dataframe_ext::*;
use odf::AsTypedBlock;
use opendatafabric as odf;

///////////////////////////////////////////////////////////////////////////////

pub struct DataWriterDataFusion {
    dataset: Arc<dyn Dataset>,
    merge_strategy: Arc<dyn MergeStrategy>,
    vocab: odf::DatasetVocabularyResolvedOwned,
    block_ref: BlockRef,
    ctx: SessionContext,

    // Mutable
    head: odf::Multihash,
    prev_data_slices: Vec<odf::Multihash>,
    next_offset: i64,
    prev_checkpoint: Option<odf::Multihash>,
    prev_watermark: Option<DateTime<Utc>>,
}

///////////////////////////////////////////////////////////////////////////////

impl DataWriterDataFusion {
    pub fn builder(dataset: Arc<dyn Dataset>, ctx: SessionContext) -> DataWriterDataFusionBuilder {
        DataWriterDataFusionBuilder::new(dataset, ctx)
    }

    /// Use [Self::builder] to create an instance
    fn new(
        dataset: Arc<dyn Dataset>,
        merge_strategy: Arc<dyn MergeStrategy>,
        vocab: odf::DatasetVocabularyResolvedOwned,
        block_ref: BlockRef,
        ctx: SessionContext,
        head: odf::Multihash,
        prev_data_slices: Vec<odf::Multihash>,
        next_offset: i64,
        prev_checkpoint: Option<odf::Multihash>,
        prev_watermark: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            dataset,
            merge_strategy,
            vocab,
            block_ref,
            ctx,
            head,
            prev_data_slices,
            next_offset,
            prev_checkpoint,
            prev_watermark,
        }
    }

    // TODO: This function currently ensures that all timestamps in the ouput are
    // represeted as `Timestamp(Millis, "UTC")` for compatibility with other engines
    // (e.g. Flink does not support event time with nanosecond precision).
    fn normalize_raw_result(&self, df: DataFrame) -> Result<DataFrame, InternalError> {
        use datafusion::arrow::datatypes::{DataType, TimeUnit};

        let utc_tz: Arc<str> = Arc::from("UTC");
        let mut select: Vec<Expr> = Vec::new();
        let mut noop = true;

        for field in df.schema().fields() {
            let expr = match field.data_type() {
                DataType::Timestamp(TimeUnit::Millisecond, Some(tz)) if tz.as_ref() == "UTC" => {
                    col(field.unqualified_column())
                }
                DataType::Timestamp(_, _) => {
                    noop = false;
                    cast(
                        col(field.unqualified_column()),
                        DataType::Timestamp(TimeUnit::Millisecond, Some(utc_tz.clone())),
                    )
                    .alias(field.name())
                }
                _ => col(field.unqualified_column()),
            };
            select.push(expr);
        }

        if noop {
            Ok(df)
        } else {
            let df = df.select(select).int_err()?;
            tracing::info!(schema = ?df.schema(), "Schema after timestamp normalization");
            Ok(df)
        }
    }

    // TODO: PERF: This will not scale well as number of blocks grows
    async fn get_all_previous_data(
        &self,
        prev_data_slices: &Vec<odf::Multihash>,
    ) -> Result<Option<DataFrame>, InternalError> {
        if prev_data_slices.is_empty() {
            return Ok(None);
        }

        let data_repo = self.dataset.as_data_repo();

        use futures::StreamExt;
        let prev_data_paths: Vec<_> = futures::stream::iter(prev_data_slices.iter().rev())
            .then(|hash| data_repo.get_internal_url(hash))
            .map(|url| url.to_string())
            .collect()
            .await;

        let df = self
            .ctx
            .read_parquet(
                prev_data_paths,
                ParquetReadOptions {
                    // TODO: Specify schema
                    schema: None,
                    file_extension: "",
                    // TODO: PERF: Possibly speed up by specifying `offset`
                    file_sort_order: Vec::new(),
                    table_partition_cols: Vec::new(),
                    parquet_pruning: None,
                    skip_metadata: None,
                    insert_mode: datafusion::datasource::listing::ListingTableInsertMode::Error,
                },
            )
            .await
            .int_err()?;

        Ok(Some(df))
    }

    fn validate_input(&self, df: &DataFrame) -> Result<(), InternalError> {
        use datafusion::arrow::datatypes::DataType;

        for system_column in [&self.vocab.offset_column, &self.vocab.system_time_column] {
            if df.schema().has_column_with_unqualified_name(system_column) {
                return Err(format!(
                    "Transformed data contains a column that conflicts with the system column \
                     name, you should either rename the data column or configure the dataset \
                     vocabulary to use a different name: {}",
                    system_column
                )
                .int_err());
            }
        }

        let event_time_col = df
            .schema()
            .fields()
            .iter()
            .find(|f| f.name().as_str() == self.vocab.event_time_column);

        if let Some(event_time_col) = event_time_col {
            match event_time_col.data_type() {
                DataType::Date32 | DataType::Date64 | DataType::Timestamp(_, _) => {}
                typ => {
                    return Err(format!(
                        "Event time column '{}' should be either Date or Timestamp, but found: {}",
                        self.vocab.event_time_column, typ
                    )
                    .int_err());
                }
            }
        }

        Ok(())
    }

    async fn with_system_columns(
        &self,
        df: DataFrame,
        system_time: DateTime<Utc>,
        fallback_event_time: DateTime<Utc>,
        start_offset: i64,
    ) -> Result<DataFrame, InternalError> {
        use datafusion::arrow::datatypes::DataType;
        use datafusion::logical_expr as expr;
        use datafusion::logical_expr::expr::WindowFunction;
        use datafusion::scalar::ScalarValue;

        // Collect non-system column names for later
        let mut raw_columns_wo_event_time: Vec<_> = df
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .filter(|n| n.as_str() != self.vocab.event_time_column)
            .collect();

        // Offset
        // TODO: For some reason this adds two collumns: the expected "offset", but also
        // "ROW_NUMBER()" for now we simply filter out the latter.
        let df = df
            .with_column(
                &self.vocab.offset_column,
                Expr::WindowFunction(WindowFunction {
                    fun: expr::WindowFunction::BuiltInWindowFunction(
                        expr::BuiltInWindowFunction::RowNumber,
                    ),
                    args: vec![],
                    partition_by: vec![],
                    order_by: vec![],
                    window_frame: expr::WindowFrame::new(false),
                }),
            )
            .int_err()?;

        let df = df
            .with_column(
                &self.vocab.offset_column,
                cast(
                    col(&self.vocab.offset_column as &str) + lit(start_offset - 1),
                    DataType::Int64,
                ),
            )
            .int_err()?;

        // System time
        let df = df
            .with_column(
                &self.vocab.system_time_column,
                Expr::Literal(ScalarValue::TimestampMillisecond(
                    Some(system_time.timestamp_millis()),
                    Some("UTC".into()),
                )),
            )
            .int_err()?;

        // Event time: Add from source event time if missing in data
        let df = if df
            .schema()
            .has_column_with_unqualified_name(&self.vocab.event_time_column)
        {
            df
        } else {
            df.with_column(
                &self.vocab.event_time_column,
                Expr::Literal(ScalarValue::TimestampMillisecond(
                    Some(fallback_event_time.timestamp_millis()),
                    Some("UTC".into()),
                )),
            )
            .int_err()?
        };

        // Reorder columns for nice looks
        let mut full_columns = vec![
            self.vocab.offset_column.to_string(),
            self.vocab.system_time_column.to_string(),
            self.vocab.event_time_column.to_string(),
        ];
        full_columns.append(&mut raw_columns_wo_event_time);
        let full_columns_str: Vec<_> = full_columns.iter().map(String::as_str).collect();

        let df = df.select_columns(&full_columns_str).int_err()?;
        Ok(df)
    }

    // TODO: Externalize configuration
    fn get_write_properties(&self) -> WriterProperties {
        // TODO: `offset` column is sorted integers so we could use delta encoding, but
        // Flink does not support it.
        // See: https://github.com/kamu-data/kamu-engine-flink/issues/3
        WriterProperties::builder()
            .set_writer_version(datafusion::parquet::file::properties::WriterVersion::PARQUET_1_0)
            .set_compression(datafusion::parquet::basic::Compression::SNAPPY)
            // system_time value will be the same for all rows in a batch
            .set_column_dictionary_enabled(self.vocab.system_time_column.as_ref().into(), true)
            .build()
    }

    #[tracing::instrument(level = "debug", skip_all, fields(?path))]
    async fn write_output(&self, path: PathBuf, df: DataFrame) -> Result<OwnedFile, InternalError> {
        self.ctx
            .write_parquet_single_file(df, &path, Some(self.get_write_properties()))
            .await
            .int_err()?;

        Ok(OwnedFile::new(path))
    }

    // Read output file back (metadata-only query) to get offsets and watermark
    async fn compute_offset_and_watermark(
        &self,
        path: &Path,
        prev_watermark: Option<DateTime<Utc>>,
        input_checkpoint: Option<odf::Multihash>,
        source_state: Option<odf::SourceState>,
    ) -> Result<AddDataParams, InternalError> {
        use datafusion::arrow::array::{
            Date32Array,
            Date64Array,
            Int64Array,
            TimestampMillisecondArray,
        };

        let df = self
            .ctx
            .read_parquet(
                path.to_str().unwrap(),
                ParquetReadOptions {
                    schema: None,
                    file_sort_order: Vec::new(),
                    file_extension: path.extension().unwrap_or_default().to_str().unwrap(),
                    table_partition_cols: Vec::new(),
                    parquet_pruning: None,
                    skip_metadata: None,
                    insert_mode: datafusion::datasource::listing::ListingTableInsertMode::Error,
                },
            )
            .await
            .int_err()?;

        // Result is empty?
        if df.clone().count().await.int_err()? == 0 {
            std::fs::remove_file(&path).int_err()?;

            return Ok(AddDataParams {
                input_checkpoint,
                output_data: None,
                output_watermark: prev_watermark,
                source_state,
            });
        }

        // Calculate stats
        let stats = df
            .aggregate(
                vec![],
                vec![
                    min(col(self.vocab.offset_column.as_ref())),
                    max(col(self.vocab.offset_column.as_ref())),
                    // TODO: Add support for more watermark strategies
                    max(col(self.vocab.event_time_column.as_ref())),
                ],
            )
            .int_err()?;

        let batches = stats.collect().await.int_err()?;
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 1);

        let offset_min = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .value(0);

        let offset_max = batches[0]
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .value(0);

        // Event time is either Date or Timestamp(Millisecond, UTC)
        let event_time_arr = batches[0].column(2).as_any();
        let event_time_max = if let Some(event_time_arr) =
            event_time_arr.downcast_ref::<TimestampMillisecondArray>()
        {
            let event_time_max_millis = event_time_arr.value(0);
            Utc.timestamp_millis_opt(event_time_max_millis).unwrap()
        } else if let Some(event_time_arr) = event_time_arr.downcast_ref::<Date64Array>() {
            let naive_datetime = event_time_arr.value_as_datetime(0).unwrap();
            DateTime::from_utc(naive_datetime, Utc)
        } else if let Some(event_time_arr) = event_time_arr.downcast_ref::<Date32Array>() {
            let naive_datetime = event_time_arr.value_as_datetime(0).unwrap();
            DateTime::from_utc(naive_datetime, Utc)
        } else {
            return Err(format!(
                "Expected event time column to be Date64 or Timestamp(Millisecond, UTC), but got \
                 {}",
                batches[0].schema().field(2)
            )
            .int_err()
            .into());
        };

        // Ensure watermark is monotonically non-decreasing
        let output_watermark = match prev_watermark {
            None => Some(event_time_max),
            Some(prev) if prev < event_time_max => Some(event_time_max),
            prev => prev,
        };

        Ok(AddDataParams {
            input_checkpoint,
            output_data: Some(odf::OffsetInterval {
                start: offset_min,
                end: offset_max,
            }),
            output_watermark,
            source_state,
        })
    }
}

///////////////////////////////////////////////////////////////////////////////

#[async_trait::async_trait]
impl DataWriter for DataWriterDataFusion {
    async fn write(
        &mut self,
        new_data: DataFrame,
        opts: WriteDataOpts,
    ) -> Result<WriteDataResult, WriteDataError> {
        self.validate_input(&new_data)?;

        // Normalize timestamps
        let df = self.normalize_raw_result(new_data)?;

        // Merge step
        // TODO: PERF: We could likely benefit from checkpointing here
        let prev = self.get_all_previous_data(&self.prev_data_slices).await?;

        let df = self.merge_strategy.merge(prev, df)?;

        tracing::debug!(
            schema = ?df.schema(),
            logical_plan = ?df.logical_plan(),
            "Performing merge step",
        );

        // Add system columns
        let df = self
            .with_system_columns(
                df,
                opts.system_time,
                opts.source_event_time,
                self.next_offset,
            )
            .await?;

        tracing::info!(schema = ?df.schema(), "Final output schema");

        // Write output
        let data_file = self.write_output(opts.data_staging_path, df).await?;

        // Prepare commit info
        let add_data = self
            .compute_offset_and_watermark(
                data_file.as_path(),
                self.prev_watermark.clone(),
                self.prev_checkpoint.clone(),
                opts.source_state,
            )
            .await?;

        let data_file = if add_data.output_data.is_none() {
            // Deletes empty output file
            None
        } else {
            Some(data_file)
        };

        // Commit data
        let commit_result = self
            .dataset
            .commit_add_data(
                add_data,
                data_file,
                None,
                CommitOpts {
                    block_ref: &self.block_ref,
                    system_time: Some(opts.system_time),
                    prev_block_hash: Some(Some(&self.head)),
                    check_object_refs: false,
                },
            )
            .await?;

        // Update state for next append
        let new_block = self
            .dataset
            .as_metadata_chain()
            .get_block(&commit_result.new_head)
            .await
            .int_err()?
            .into_typed::<odf::AddData>()
            .unwrap();

        self.head = commit_result.new_head.clone();

        if let Some(output_data) = &new_block.event.output_data {
            self.next_offset = output_data.interval.end + 1;
            self.prev_data_slices
                .push(output_data.physical_hash.clone());
        }

        self.prev_checkpoint = new_block
            .event
            .output_checkpoint
            .as_ref()
            .map(|c| c.physical_hash.clone());
        self.prev_watermark = new_block.event.output_watermark;

        Ok(WriteDataResult {
            old_head: commit_result.old_head.unwrap(),
            new_head: commit_result.new_head,
            new_block,
        })
    }
}

///////////////////////////////////////////////////////////////////////////////

pub struct DataWriterDataFusionBuilder {
    dataset: Arc<dyn Dataset>,
    ctx: SessionContext,
    block_ref: BlockRef,

    merge_strategy_def: Option<odf::MergeStrategy>,
    vocab: Option<odf::DatasetVocabularyResolvedOwned>,
    head: Option<odf::Multihash>,
    prev_data_slices: Option<Vec<odf::Multihash>>,
    next_offset: Option<i64>,
    prev_checkpoint: Option<Option<odf::Multihash>>,
    prev_watermark: Option<Option<DateTime<Utc>>>,
}

impl DataWriterDataFusionBuilder {
    pub fn new(dataset: Arc<dyn Dataset>, ctx: SessionContext) -> Self {
        Self {
            dataset,
            ctx,
            block_ref: BlockRef::Head,
            merge_strategy_def: None,
            vocab: None,
            head: None,
            prev_data_slices: None,
            next_offset: None,
            prev_checkpoint: None,
            prev_watermark: None,
        }
    }

    pub fn with_block_ref(self, block_ref: BlockRef) -> Self {
        Self { block_ref, ..self }
    }

    /// Allows builder not to avoid scanning the metadatachain
    pub fn with_metadata(
        self,
        block_ref: BlockRef,
        head: odf::Multihash,
        merge_strategy_def: odf::MergeStrategy,
        next_offset: i64,
        prev_checkpoint: Option<odf::Multihash>,
        prev_watermark: Option<DateTime<Utc>>,
        prev_data_slices: Vec<odf::Multihash>,
        vocab: impl Into<odf::DatasetVocabularyResolvedOwned>,
    ) -> Self {
        Self {
            block_ref,
            head: Some(head),
            merge_strategy_def: Some(merge_strategy_def),
            next_offset: Some(next_offset),
            prev_checkpoint: Some(prev_checkpoint),
            prev_watermark: Some(prev_watermark),
            prev_data_slices: Some(prev_data_slices),
            vocab: Some(vocab.into()),
            ..self
        }
    }

    pub async fn build(self) -> Result<DataWriterDataFusion, InternalError> {
        let this = if self.merge_strategy_def.is_some() {
            self
        } else {
            self.scan_metadata().await?
        };

        let merge_strategy = Self::merge_strategy_for(
            this.merge_strategy_def.unwrap(),
            this.vocab.as_ref().unwrap(),
        );

        Ok(DataWriterDataFusion::new(
            this.dataset,
            merge_strategy,
            this.vocab.unwrap(),
            this.block_ref,
            this.ctx,
            this.head.unwrap(),
            this.prev_data_slices.unwrap(),
            this.next_offset.unwrap(),
            this.prev_checkpoint.unwrap(),
            this.prev_watermark.unwrap(),
        ))
    }

    // TODO: PERF: Full metadata scan below - this is expensive and should be
    // improved using skip lists and caching
    async fn scan_metadata(self) -> Result<Self, InternalError> {
        let head = self
            .dataset
            .as_metadata_chain()
            .get_ref(&self.block_ref)
            .await
            .int_err()?;

        let mut merge_strategy_def = None;
        let mut prev_data_slices = Vec::new();
        let mut prev_checkpoint = None;
        let mut prev_watermark = None;
        let mut vocab = None;
        let mut next_offset = None;

        {
            use futures::stream::TryStreamExt;
            let mut block_stream = self
                .dataset
                .as_metadata_chain()
                .iter_blocks_interval(&head, None, false);

            while let Some((_, block)) = block_stream.try_next().await.int_err()? {
                match block.event {
                    odf::MetadataEvent::AddData(add_data) => {
                        if let Some(output_data) = &add_data.output_data {
                            prev_data_slices.push(output_data.physical_hash.clone());

                            if next_offset.is_none() {
                                next_offset = Some(output_data.interval.end + 1);
                            }
                        }
                        if prev_checkpoint.is_none() {
                            prev_checkpoint =
                                Some(add_data.output_checkpoint.map(|cp| cp.physical_hash));
                        }
                        if prev_watermark.is_none() {
                            prev_watermark = Some(add_data.output_watermark);
                        }
                    }
                    odf::MetadataEvent::SetWatermark(set_wm) => {
                        if prev_watermark.is_none() {
                            prev_watermark = Some(Some(set_wm.output_watermark));
                        }
                    }
                    odf::MetadataEvent::SetPollingSource(src) => {
                        if merge_strategy_def.is_none() {
                            merge_strategy_def = Some(src.merge);
                        }
                    }
                    odf::MetadataEvent::SetVocab(set_vocab) => {
                        vocab = Some(set_vocab.into());
                    }
                    odf::MetadataEvent::Seed(seed) => {
                        assert_eq!(seed.dataset_kind, odf::DatasetKind::Root);
                        if next_offset.is_none() {
                            next_offset = Some(0);
                        }
                    }
                    odf::MetadataEvent::ExecuteQuery(_) => unreachable!(),
                    odf::MetadataEvent::SetAttachments(_)
                    | odf::MetadataEvent::SetInfo(_)
                    | odf::MetadataEvent::SetLicense(_)
                    | odf::MetadataEvent::SetTransform(_) => (),
                }
            }
        }

        Ok(Self {
            head: Some(head),
            merge_strategy_def: Some(merge_strategy_def.unwrap()),
            prev_data_slices: Some(prev_data_slices),
            prev_checkpoint: Some(prev_checkpoint.unwrap_or_default()),
            prev_watermark: Some(prev_watermark.unwrap_or_default()),
            vocab: Some(vocab.unwrap_or_default()),
            next_offset,
            ..self
        })
    }

    fn merge_strategy_for(
        conf: odf::MergeStrategy,
        vocab: &odf::DatasetVocabularyResolved<'_>,
    ) -> Arc<dyn MergeStrategy> {
        use crate::merge_strategies::*;

        match conf {
            odf::MergeStrategy::Append => Arc::new(MergeStrategyAppend),
            odf::MergeStrategy::Ledger(conf) => {
                Arc::new(MergeStrategyLedger::new(conf.primary_key.clone()))
            }
            odf::MergeStrategy::Snapshot(cfg) => Arc::new(MergeStrategySnapshot::new(
                vocab.offset_column.to_string(),
                cfg.clone(),
            )),
        }
    }
}
