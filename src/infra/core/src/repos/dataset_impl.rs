// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::path::Path;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kamu_core::*;
use opendatafabric::serde::yaml::Manifest;
use opendatafabric::*;

/////////////////////////////////////////////////////////////////////////////////////////

pub struct DatasetImpl<MetaChain, DataRepo, CheckpointRepo, InfoRepo> {
    metadata_chain: MetaChain,
    data_repo: DataRepo,
    checkpoint_repo: CheckpointRepo,
    info_repo: InfoRepo,
}

/////////////////////////////////////////////////////////////////////////////////////////

impl<MetaChain, DataRepo, CheckpointRepo, InfoRepo>
    DatasetImpl<MetaChain, DataRepo, CheckpointRepo, InfoRepo>
where
    MetaChain: MetadataChain + Sync + Send,
    DataRepo: ObjectRepository + Sync + Send,
    CheckpointRepo: ObjectRepository + Sync + Send,
    InfoRepo: NamedObjectRepository + Sync + Send,
{
    pub fn new(
        metadata_chain: MetaChain,
        data_repo: DataRepo,
        checkpoint_repo: CheckpointRepo,
        info_repo: InfoRepo,
    ) -> Self {
        Self {
            metadata_chain,
            data_repo,
            checkpoint_repo,
            info_repo,
        }
    }

    async fn read_summary(&self) -> Result<Option<DatasetSummary>, GetSummaryError> {
        let data = match self.info_repo.get("summary").await {
            Ok(data) => data,
            Err(GetNamedError::NotFound(_)) => return Ok(None),
            Err(GetNamedError::Access(e)) => return Err(GetSummaryError::Access(e)),
            Err(GetNamedError::Internal(e)) => return Err(GetSummaryError::Internal(e)),
        };

        let manifest: Manifest<DatasetSummary> = serde_yaml::from_slice(&data[..]).int_err()?;

        if manifest.kind != "DatasetSummary" {
            return Err(InvalidObjectKind {
                expected: "DatasetSummary".to_owned(),
                actual: manifest.kind,
            }
            .int_err()
            .into());
        }

        Ok(Some(manifest.content))
    }

    async fn write_summary(&self, summary: &DatasetSummary) -> Result<(), GetSummaryError> {
        let manifest = Manifest {
            kind: "DatasetSummary".to_owned(),
            version: 1,
            content: summary.clone(),
        };

        let data = serde_yaml::to_string(&manifest).int_err()?.into_bytes();

        match self.info_repo.set("summary", &data).await {
            Ok(()) => Ok(()),
            Err(SetNamedError::Access(e)) => Err(GetSummaryError::Access(e)),
            Err(SetNamedError::Internal(e)) => Err(GetSummaryError::Internal(e)),
        }?;

        Ok(())
    }

    async fn update_summary(
        &self,
        prev: Option<DatasetSummary>,
    ) -> Result<Option<DatasetSummary>, GetSummaryError> {
        let current_head = match self.metadata_chain.get_ref(&BlockRef::Head).await {
            Ok(h) => h,
            Err(GetRefError::NotFound(_)) => return Ok(prev),
            Err(GetRefError::Access(e)) => return Err(GetSummaryError::Access(e)),
            Err(GetRefError::Internal(e)) => return Err(GetSummaryError::Internal(e)),
        };

        let last_seen = prev.as_ref().map(|s| &s.last_block_hash);
        if last_seen == Some(&current_head) {
            return Ok(prev);
        }

        tracing::debug!(?current_head, ?last_seen, "Updating dataset summary");

        let increment = self
            .compute_summary_increment(&current_head, last_seen)
            .await?;

        let summary = if increment.seen_chain_beginning() {
            // Increment includes the entire chain (most likely due to a history reset)
            increment.into_summary()
        } else {
            // Increment applies to interval [head, prev.last_block_hash)
            increment.apply_to_summary(prev.unwrap())
        };

        self.write_summary(&summary).await?;

        Ok(Some(summary))
    }

    async fn compute_summary_increment(
        &self,
        current_head: &Multihash,
        last_seen: Option<&Multihash>,
    ) -> Result<UpdateSummaryIncrement, GetSummaryError> {
        let mut block_stream =
            self.metadata_chain
                .iter_blocks_interval(&current_head, last_seen, true);

        use tokio_stream::StreamExt;
        let mut increment: UpdateSummaryIncrement = UpdateSummaryIncrement::default();
        increment.seen_head = Some(current_head.clone());

        while let Some((_, block)) = block_stream.try_next().await.int_err()? {
            match block.event {
                MetadataEvent::Seed(seed) => {
                    increment.seen_id.get_or_insert(seed.dataset_id);
                    increment.seen_kind.get_or_insert(seed.dataset_kind);
                }
                MetadataEvent::SetTransform(set_transform) => {
                    increment
                        .seen_dependencies
                        .get_or_insert(set_transform.inputs);
                }
                MetadataEvent::AddData(add_data) => {
                    increment.seen_last_pulled.get_or_insert(block.system_time);

                    if let Some(output_data) = add_data.output_data {
                        let iv = output_data.interval;
                        increment.seen_num_records += (iv.end - iv.start + 1) as u64;

                        increment.seen_data_size += output_data.size as u64;
                    }

                    if let Some(checkpoint) = add_data.output_checkpoint {
                        increment.seen_checkpoints_size += checkpoint.size as u64;
                    }
                }
                MetadataEvent::ExecuteQuery(execute_query) => {
                    increment.seen_last_pulled.get_or_insert(block.system_time);

                    if let Some(output_data) = execute_query.output_data {
                        let iv = output_data.interval;
                        increment.seen_num_records += (iv.end - iv.start + 1) as u64;

                        increment.seen_data_size += output_data.size as u64;
                    }

                    if let Some(checkpoint) = execute_query.output_checkpoint {
                        increment.seen_checkpoints_size += checkpoint.size as u64;
                    }
                }
                MetadataEvent::SetAttachments(_)
                | MetadataEvent::SetInfo(_)
                | MetadataEvent::SetLicense(_)
                | MetadataEvent::SetWatermark(_)
                | MetadataEvent::SetVocab(_)
                | MetadataEvent::SetPollingSource(_) => (),
            }
        }

        Ok(increment)
    }
}

/////////////////////////////////////////////////////////////////////////////////////////

#[derive(Default)]
struct UpdateSummaryIncrement {
    seen_id: Option<DatasetID>,
    seen_kind: Option<DatasetKind>,
    seen_head: Option<Multihash>,
    seen_dependencies: Option<Vec<TransformInput>>,
    seen_last_pulled: Option<DateTime<Utc>>,
    seen_num_records: u64,
    seen_data_size: u64,
    seen_checkpoints_size: u64,
}

impl UpdateSummaryIncrement {
    fn seen_chain_beginning(&self) -> bool {
        // Seed blocks are guaranteed to appear only once in a chain, and only at the
        // very beginning
        return self.seen_id.is_some();
    }

    fn into_summary(self) -> DatasetSummary {
        DatasetSummary {
            id: self.seen_id.unwrap(),
            kind: self.seen_kind.unwrap(),
            last_block_hash: self.seen_head.unwrap(),
            dependencies: self.seen_dependencies.unwrap_or_default(),
            last_pulled: self.seen_last_pulled,
            num_records: self.seen_num_records,
            data_size: self.seen_data_size,
            checkpoints_size: self.seen_checkpoints_size,
        }
    }

    fn apply_to_summary(self, summary: DatasetSummary) -> DatasetSummary {
        DatasetSummary {
            id: self.seen_id.unwrap_or(summary.id),
            kind: self.seen_kind.unwrap_or(summary.kind),
            last_block_hash: self.seen_head.unwrap(),
            dependencies: self.seen_dependencies.unwrap_or(summary.dependencies),
            last_pulled: self.seen_last_pulled.or(summary.last_pulled),
            num_records: summary.num_records + self.seen_num_records,
            data_size: summary.data_size + self.seen_data_size,
            checkpoints_size: summary.checkpoints_size + self.seen_checkpoints_size,
        }
    }
}

/////////////////////////////////////////////////////////////////////////////////////////

#[async_trait]
impl<MetaChain, DataRepo, CheckpointRepo, InfoRepo> Dataset
    for DatasetImpl<MetaChain, DataRepo, CheckpointRepo, InfoRepo>
where
    MetaChain: MetadataChain + Sync + Send,
    DataRepo: ObjectRepository + Sync + Send,
    CheckpointRepo: ObjectRepository + Sync + Send,
    InfoRepo: NamedObjectRepository + Sync + Send,
{
    /// Helper function to append a generic event to metadata chain.
    ///
    /// Warning: Don't use when synchronizing blocks from another dataset.
    async fn commit_event(
        &self,
        event: MetadataEvent,
        opts: CommitOpts<'_>,
    ) -> Result<CommitResult, CommitError> {
        // Validate refferential consistency
        if opts.check_object_refs {
            if let Some(event) = event.as_data_stream_event() {
                if let Some(data_slice) = event.output_data {
                    if !self
                        .as_data_repo()
                        .contains(&data_slice.physical_hash)
                        .await
                        .int_err()?
                    {
                        return Err(ObjectNotFoundError {
                            hash: data_slice.physical_hash.clone(),
                        }
                        .into());
                    }
                }
                if let Some(checkpoint) = event.output_checkpoint {
                    if !self
                        .as_checkpoint_repo()
                        .contains(&checkpoint.physical_hash)
                        .await
                        .int_err()?
                    {
                        return Err(ObjectNotFoundError {
                            hash: checkpoint.physical_hash.clone(),
                        }
                        .into());
                    }
                }
            }
        }

        let chain = self.as_metadata_chain();

        let prev_block_hash = if let Some(prev_block_hash) = opts.prev_block_hash {
            prev_block_hash.cloned()
        } else {
            match chain.get_ref(opts.block_ref).await {
                Ok(h) => Some(h),
                Err(GetRefError::NotFound(_)) => None,
                Err(e) => return Err(e.int_err().into()),
            }
        };

        let sequence_number = if let Some(prev_block_hash) = &prev_block_hash {
            chain
                .get_block(prev_block_hash)
                .await
                .int_err()?
                .sequence_number
                + 1
        } else {
            0
        };

        let block = MetadataBlock {
            prev_block_hash: prev_block_hash.clone(),
            sequence_number,
            system_time: opts.system_time.unwrap_or_else(|| Utc::now()),
            event,
        };

        tracing::info!(?block, "Committing new block");

        let new_head = chain
            .append(
                block,
                AppendOpts {
                    update_ref: Some(opts.block_ref),
                    check_ref_is_prev_block: true,
                    ..AppendOpts::default()
                },
            )
            .await?;

        tracing::info!(%new_head, "Committed new block");

        Ok(CommitResult {
            old_head: prev_block_hash,
            new_head,
        })
    }

    /// Helper function to commit AddData event into a local dataset.
    ///
    /// Will attempt to atomically move data and checkpoint files, so those have
    /// to be on the same file system as the workspace.
    async fn commit_add_data(
        &self,
        input_checkpoint: Option<Multihash>,
        data_interval: Option<OffsetInterval>,
        movable_data_file: Option<&Path>,
        movable_checkpoint_file: Option<&Path>,
        output_watermark: Option<DateTime<Utc>>,
        source_state: Option<SourceState>,
        opts: CommitOpts<'_>,
    ) -> Result<CommitResult, CommitError> {
        // Commit data
        let output_data = match data_interval {
            None => None,
            Some(data_interval) => {
                let from_data_path = movable_data_file.unwrap();

                let output_data = DataSlice {
                    logical_hash: kamu_data_utils::data::hash::get_parquet_logical_hash(
                        from_data_path.as_ref(),
                    )
                    .int_err()?,
                    physical_hash: kamu_data_utils::data::hash::get_file_physical_hash(
                        from_data_path.as_ref(),
                    )
                    .int_err()?,
                    interval: data_interval,
                    size: std::fs::metadata(&from_data_path).int_err()?.len() as i64,
                };

                // Move data to repo
                self.as_data_repo()
                    .insert_file_move(
                        from_data_path.as_ref(),
                        InsertOpts {
                            precomputed_hash: Some(&output_data.physical_hash),
                            expected_hash: None,
                            size_hint: Some(output_data.size as usize),
                        },
                    )
                    .await
                    .int_err()?;

                Some(output_data)
            }
        };

        // Commit checkpoint
        let output_checkpoint = match movable_checkpoint_file {
            None => None,
            Some(checkpoint_file) => {
                let physical_hash =
                    kamu_data_utils::data::hash::get_file_physical_hash(checkpoint_file.as_ref())
                        .int_err()?;

                let size = std::fs::metadata(&checkpoint_file).int_err()?.len() as i64;

                self.as_checkpoint_repo()
                    .insert_file_move(
                        checkpoint_file.as_ref(),
                        InsertOpts {
                            precomputed_hash: Some(&physical_hash),
                            expected_hash: None,
                            size_hint: Some(size as usize),
                        },
                    )
                    .await
                    .int_err()?;

                Some(Checkpoint {
                    physical_hash,
                    size,
                })
            }
        };

        let metadata_event = MetadataEvent::AddData(AddData {
            input_checkpoint,
            output_data,
            output_checkpoint,
            output_watermark,
            source_state,
        });

        self.commit_event(
            metadata_event,
            CommitOpts {
                check_object_refs: false, // We just added all objects
                ..opts
            },
        )
        .await
    }

    async fn get_summary(&self, opts: GetSummaryOpts) -> Result<DatasetSummary, GetSummaryError> {
        let summary = self.read_summary().await?;

        let summary = if opts.update_if_stale {
            self.update_summary(summary).await?
        } else {
            summary
        };

        summary.ok_or_else(|| GetSummaryError::EmptyDataset)
    }

    fn as_metadata_chain(&self) -> &dyn MetadataChain {
        &self.metadata_chain
    }

    fn as_data_repo(&self) -> &dyn ObjectRepository {
        &self.data_repo
    }

    fn as_checkpoint_repo(&self) -> &dyn ObjectRepository {
        &self.checkpoint_repo
    }
}