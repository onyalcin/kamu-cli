// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, DurationRound, Utc};
use database_common::DatabaseTransactionRunner;
use dill::*;
use futures::TryStreamExt;
use internal_error::InternalError;
use kamu_core::{
    DatasetChangesService,
    DatasetLifecycleMessage,
    DatasetOwnershipService,
    DependencyGraphService,
    MESSAGE_PRODUCER_KAMU_CORE_DATASET_SERVICE,
};
use kamu_flow_system::*;
use kamu_task_system::*;
use messaging_outbox::{
    MessageConsumer,
    MessageConsumerMeta,
    MessageConsumerT,
    MessageConsumptionDurability,
    Outbox,
    OutboxExt,
};
use opendatafabric::{AccountID, DatasetID};
use time_source::SystemTimeSource;
use tokio_stream::StreamExt;

use super::active_configs_state::ActiveConfigsState;
use super::flow_time_wheel::FlowTimeWheel;
use super::pending_flows_state::PendingFlowsState;
use crate::{
    MESSAGE_CONSUMER_KAMU_FLOW_SERVICE,
    MESSAGE_PRODUCER_KAMU_FLOW_CONFIGURATION_SERVICE,
    MESSAGE_PRODUCER_KAMU_FLOW_SERVICE,
};

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

pub struct FlowServiceImpl {
    catalog: Catalog,
    state: Arc<Mutex<State>>,
    run_config: Arc<FlowServiceRunConfig>,
    flow_event_store: Arc<dyn FlowEventStore>,
    time_source: Arc<dyn SystemTimeSource>,
    task_scheduler: Arc<dyn TaskScheduler>,
    dataset_changes_service: Arc<dyn DatasetChangesService>,
    dependency_graph_service: Arc<dyn DependencyGraphService>,
    dataset_ownership_service: Arc<dyn DatasetOwnershipService>,
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

#[derive(Default)]
struct State {
    active_configs: ActiveConfigsState,
    pending_flows: PendingFlowsState,
    time_wheel: FlowTimeWheel,
    running: bool,
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

#[component(pub)]
#[interface(dyn FlowService)]
#[interface(dyn FlowServiceTestDriver)]
#[interface(dyn MessageConsumer)]
#[interface(dyn MessageConsumerT<TaskProgressMessage>)]
#[interface(dyn MessageConsumerT<DatasetLifecycleMessage>)]
#[interface(dyn MessageConsumerT<FlowConfigurationUpdatedMessage>)]
#[meta(MessageConsumerMeta {
    consumer_name: MESSAGE_CONSUMER_KAMU_FLOW_SERVICE,
    feeding_producers: &[
        MESSAGE_PRODUCER_KAMU_CORE_DATASET_SERVICE,
        MESSAGE_PRODUCER_KAMU_TASK_EXECUTOR,
        MESSAGE_PRODUCER_KAMU_FLOW_CONFIGURATION_SERVICE
    ],
    durability: MessageConsumptionDurability::Durable,
})]
#[scope(Singleton)]
impl FlowServiceImpl {
    pub fn new(
        catalog: Catalog,
        run_config: Arc<FlowServiceRunConfig>,
        flow_event_store: Arc<dyn FlowEventStore>,
        time_source: Arc<dyn SystemTimeSource>,
        task_scheduler: Arc<dyn TaskScheduler>,
        dataset_changes_service: Arc<dyn DatasetChangesService>,
        dependency_graph_service: Arc<dyn DependencyGraphService>,
        dataset_ownership_service: Arc<dyn DatasetOwnershipService>,
    ) -> Self {
        Self {
            catalog,
            state: Arc::new(Mutex::new(State::default())),
            run_config,
            flow_event_store,
            time_source,
            task_scheduler,
            dataset_changes_service,
            dependency_graph_service,
            dataset_ownership_service,
        }
    }

    fn round_time(&self, time: DateTime<Utc>) -> Result<DateTime<Utc>, InternalError> {
        let rounded_time = time
            .duration_round(self.run_config.awaiting_step)
            .int_err()?;
        Ok(rounded_time)
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn run_current_timeslot(
        &self,
        timeslot_time: DateTime<Utc>,
    ) -> Result<(), InternalError> {
        let planned_flow_ids: Vec<_> = {
            let mut state = self.state.lock().unwrap();
            state.time_wheel.take_nearest_planned_flows()
        };

        let mut planned_task_futures = Vec::new();
        for planned_flow_id in planned_flow_ids {
            planned_task_futures.push(async move {
                let mut flow = Flow::load(planned_flow_id, self.flow_event_store.as_ref())
                    .await
                    .int_err()?;
                self.schedule_flow_task(&mut flow, timeslot_time).await?;
                Ok(())
            });
        }

        let results = futures::future::join_all(planned_task_futures).await;
        results
            .into_iter()
            .filter(Result::is_err)
            .map(|e| e.err().unwrap())
            .for_each(|e: InternalError| {
                tracing::error!(error=?e, "Scheduling flow failed");
            });

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn initialize_auto_polling_flows_from_configurations(
        &self,
        flow_configuration_service: &dyn FlowConfigurationService,
        start_time: DateTime<Utc>,
    ) -> Result<(), InternalError> {
        // Query all enabled flow configurations
        let enabled_configurations: Vec<_> = flow_configuration_service
            .list_enabled_configurations()
            .try_collect()
            .await?;

        // Split configs by those which have a schedule or different rules
        let (schedule_configs, non_schedule_configs): (Vec<_>, Vec<_>) = enabled_configurations
            .into_iter()
            .partition(|config| matches!(config.rule, FlowConfigurationRule::Schedule(_)));

        // Activate all configs, ensuring schedule configs precedes non-schedule configs
        // (this i.e. forces all root datasets to be updated earlier than the derived)
        //
        // Thought: maybe we need topological sorting by derived relations as well to
        // optimize the initial execution order, but batching rules may work just fine
        for enabled_config in schedule_configs
            .into_iter()
            .chain(non_schedule_configs.into_iter())
        {
            self.activate_flow_configuration(
                start_time,
                enabled_config.flow_key,
                enabled_config.rule,
            )
            .await?;
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, fields(?flow_key, ?rule))]
    async fn activate_flow_configuration(
        &self,
        start_time: DateTime<Utc>,
        flow_key: FlowKey,
        rule: FlowConfigurationRule,
    ) -> Result<(), InternalError> {
        match &flow_key {
            FlowKey::Dataset(dataset_flow_key) => {
                self.state
                    .lock()
                    .unwrap()
                    .active_configs
                    .add_dataset_flow_config(dataset_flow_key, rule.clone());

                match &rule {
                    FlowConfigurationRule::TransformRule(_) => {
                        self.enqueue_auto_polling_flow_unconditionally(start_time, &flow_key)
                            .await?;
                    }
                    // Such as compaction and reset is very dangerous operation we
                    // skip running it during activation flow configurations.
                    // And schedule will be used only for system flows
                    FlowConfigurationRule::CompactionRule(_)
                    | FlowConfigurationRule::Schedule(_)
                    | FlowConfigurationRule::ResetRule(_) => (),
                    FlowConfigurationRule::IngestRule(ingest_rule) => {
                        self.enqueue_scheduled_auto_polling_flow(
                            start_time,
                            &flow_key,
                            &ingest_rule.schedule_condition,
                        )
                        .await?;
                    }
                }
            }
            FlowKey::System(system_flow_key) => {
                if let FlowConfigurationRule::Schedule(schedule) = &rule {
                    self.state
                        .lock()
                        .unwrap()
                        .active_configs
                        .add_system_flow_config(system_flow_key.flow_type, schedule.clone());

                    self.enqueue_scheduled_auto_polling_flow(start_time, &flow_key, schedule)
                        .await?;
                } else {
                    unimplemented!(
                        "Doubt will ever need to schedule system flows via batching rules"
                    )
                }
            }
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, fields(?flow_key))]
    async fn try_enqueue_scheduled_auto_polling_flow_if_enabled(
        &self,
        start_time: DateTime<Utc>,
        flow_key: &FlowKey,
    ) -> Result<(), InternalError> {
        let maybe_active_schedule = self
            .state
            .lock()
            .unwrap()
            .active_configs
            .try_get_flow_schedule(flow_key);

        if let Some(active_schedule) = maybe_active_schedule {
            self.enqueue_scheduled_auto_polling_flow(start_time, flow_key, &active_schedule)
                .await?;
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, fields(?flow_key, ?schedule))]
    async fn enqueue_scheduled_auto_polling_flow(
        &self,
        start_time: DateTime<Utc>,
        flow_key: &FlowKey,
        schedule: &Schedule,
    ) -> Result<FlowState, InternalError> {
        self.trigger_flow_common(
            flow_key,
            FlowTrigger::AutoPolling(FlowTriggerAutoPolling {
                trigger_time: start_time,
            }),
            FlowTriggerContext::Scheduled(schedule.clone()),
            None,
        )
        .await
    }

    #[tracing::instrument(level = "trace", skip_all, fields(?flow_key))]
    async fn enqueue_auto_polling_flow_unconditionally(
        &self,
        start_time: DateTime<Utc>,
        flow_key: &FlowKey,
    ) -> Result<FlowState, InternalError> {
        // Very similar to manual trigger, but automatic reasons
        self.trigger_flow_common(
            flow_key,
            FlowTrigger::AutoPolling(FlowTriggerAutoPolling {
                trigger_time: start_time,
            }),
            FlowTriggerContext::Unconditional,
            None,
        )
        .await
    }

    #[tracing::instrument(level = "trace", skip_all, fields(?flow.flow_key, %flow.flow_id, ))]
    async fn enqueue_dependent_flows(
        &self,
        input_success_time: DateTime<Utc>,
        flow: &Flow,
        flow_result: &FlowResult,
    ) -> Result<(), InternalError> {
        if let FlowKey::Dataset(fk_dataset) = &flow.flow_key {
            let dependent_dataset_flow_plans = self
                .make_downstream_dependencies_flow_plans(fk_dataset, flow.config_snapshot.as_ref())
                .await?;
            if dependent_dataset_flow_plans.is_empty() {
                return Ok(());
            }
            let trigger = FlowTrigger::InputDatasetFlow(FlowTriggerInputDatasetFlow {
                trigger_time: input_success_time,
                dataset_id: fk_dataset.dataset_id.clone(),
                flow_type: fk_dataset.flow_type,
                flow_id: flow.flow_id,
                flow_result: flow_result.clone(),
            });
            // For each, trigger needed flow
            for dependent_dataset_flow_plan in dependent_dataset_flow_plans {
                self.trigger_flow_common(
                    &dependent_dataset_flow_plan.flow_key,
                    trigger.clone(),
                    dependent_dataset_flow_plan.flow_trigger_context,
                    dependent_dataset_flow_plan.maybe_config_snapshot,
                )
                .await?;
            }

            Ok(())
        } else {
            unreachable!("Not expecting other types of flow keys than dataset");
        }
    }

    async fn trigger_flow_common(
        &self,
        flow_key: &FlowKey,
        trigger: FlowTrigger,
        context: FlowTriggerContext,
        config_snapshot_maybe: Option<FlowConfigurationSnapshot>,
    ) -> Result<FlowState, InternalError> {
        // Query previous runs stats to determine activation time
        let flow_run_stats = self.flow_run_stats(flow_key).await?;

        // Flows may not be attempted more frequent than mandatory throttling period.
        // If flow has never run before, let it go without restriction.
        let trigger_time = trigger.trigger_time();
        let mut throttling_boundary_time =
            flow_run_stats.last_attempt_time.map_or(trigger_time, |t| {
                t + self.run_config.mandatory_throttling_period
            });
        // It's also possible we are waiting for some start condition much longer..
        if throttling_boundary_time < trigger_time {
            throttling_boundary_time = trigger_time;
        }

        // Is a pending flow present for this config?
        match self.find_pending_flow(flow_key) {
            // Already pending flow
            Some(flow_id) => {
                // Load, merge triggers, update activation time
                let mut flow = Flow::load(flow_id, self.flow_event_store.as_ref())
                    .await
                    .int_err()?;

                // Only merge unique triggers, ignore identical
                flow.add_trigger_if_unique(self.time_source.now(), trigger)
                    .int_err()?;

                match context {
                    FlowTriggerContext::Batching(transform_rule) => {
                        // Is this rule still waited?
                        if matches!(flow.start_condition, Some(FlowStartCondition::Batching(_))) {
                            self.evaluate_flow_transform_rule(
                                trigger_time,
                                &mut flow,
                                &transform_rule,
                                throttling_boundary_time,
                            )
                            .await?;
                        } else {
                            // Skip, the flow waits for something else
                        }
                    }
                    FlowTriggerContext::Scheduled(_) | FlowTriggerContext::Unconditional => {
                        // Evaluate throttling condition: is new time earlier than planned?
                        let planned_time = self
                            .find_planned_flow_activation_time(flow.flow_id)
                            .expect("Flow expected to have activation time by now");

                        if throttling_boundary_time < planned_time {
                            // If so, enqueue the flow earlier
                            self.enqueue_flow(flow.flow_id, throttling_boundary_time)?;

                            // Indicate throttling, if applied
                            if throttling_boundary_time > trigger_time {
                                self.indicate_throttling_activity(
                                    &mut flow,
                                    throttling_boundary_time,
                                    trigger_time,
                                )?;
                            }
                        }
                    }
                }

                flow.save(self.flow_event_store.as_ref()).await.int_err()?;
                Ok(flow.into())
            }

            // Otherwise, initiate a new flow, and enqueue it in the time wheel
            None => {
                // Initiate new flow
                let config_snapshot_maybe = if config_snapshot_maybe.is_some() {
                    config_snapshot_maybe
                } else {
                    self.state
                        .lock()
                        .unwrap()
                        .active_configs
                        .try_get_config_snapshot_by_key(flow_key)
                };
                let mut flow = self
                    .make_new_flow(flow_key.clone(), trigger, config_snapshot_maybe)
                    .await?;

                match context {
                    FlowTriggerContext::Batching(transform_rule) => {
                        // Don't activate if batching condition not satisfied
                        self.evaluate_flow_transform_rule(
                            trigger_time,
                            &mut flow,
                            &transform_rule,
                            throttling_boundary_time,
                        )
                        .await?;
                    }
                    FlowTriggerContext::Scheduled(schedule) => {
                        // Next activation time depends on:
                        //  - last success time, if ever launched
                        //  - schedule, if defined
                        let naive_next_activation_time = schedule
                            .next_activation_time(trigger_time, flow_run_stats.last_success_time);

                        // Apply throttling boundary
                        let next_activation_time =
                            std::cmp::max(throttling_boundary_time, naive_next_activation_time);
                        self.enqueue_flow(flow.flow_id, next_activation_time)?;

                        // Set throttling activity as start condition
                        if throttling_boundary_time > naive_next_activation_time {
                            self.indicate_throttling_activity(
                                &mut flow,
                                throttling_boundary_time,
                                naive_next_activation_time,
                            )?;
                        } else if naive_next_activation_time > trigger_time {
                            // Set waiting according to the schedule
                            flow.set_relevant_start_condition(
                                self.time_source.now(),
                                FlowStartCondition::Schedule(FlowStartConditionSchedule {
                                    wake_up_at: naive_next_activation_time,
                                }),
                            )
                            .int_err()?;
                        }
                    }
                    FlowTriggerContext::Unconditional => {
                        // Apply throttling boundary
                        let next_activation_time =
                            std::cmp::max(throttling_boundary_time, trigger_time);
                        self.enqueue_flow(flow.flow_id, next_activation_time)?;

                        // Set throttling activity as start condition
                        if throttling_boundary_time > trigger_time {
                            self.indicate_throttling_activity(
                                &mut flow,
                                throttling_boundary_time,
                                trigger_time,
                            )?;
                        }
                    }
                }

                flow.save(self.flow_event_store.as_ref()).await.int_err()?;
                Ok(flow.into())
            }
        }
    }

    async fn evaluate_flow_transform_rule(
        &self,
        evaluation_time: DateTime<Utc>,
        flow: &mut Flow,
        transform_rule: &TransformRule,
        throttling_boundary_time: DateTime<Utc>,
    ) -> Result<(), InternalError> {
        assert!(matches!(
            flow.flow_key.get_type(),
            AnyFlowType::Dataset(
                DatasetFlowType::ExecuteTransform | DatasetFlowType::HardCompaction
            )
        ));

        // TODO: it's likely assumed the accumulation is per each input separately, but
        // for now count overall number
        let mut accumulated_records_count = 0;
        let mut watermark_modified = false;
        let mut is_compacted = false;

        // Scan each accumulated trigger to decide
        for trigger in &flow.triggers {
            if let FlowTrigger::InputDatasetFlow(trigger) = trigger {
                match &trigger.flow_result {
                    FlowResult::Empty | FlowResult::DatasetReset(_) => {}
                    FlowResult::DatasetCompact(_) => {
                        is_compacted = true;
                    }
                    FlowResult::DatasetUpdate(update) => {
                        // Compute increment since the first trigger by this dataset.
                        // Note: there might have been multiple updates since that time.
                        // We are only recording the first trigger of particular dataset.
                        if let FlowResultDatasetUpdate::Changed(update_result) = update {
                            let increment = self
                                .dataset_changes_service
                                .get_increment_since(
                                    &trigger.dataset_id,
                                    update_result.old_head.as_ref(),
                                )
                                .await
                                .int_err()?;

                            accumulated_records_count += increment.num_records;
                            watermark_modified |= increment.updated_watermark.is_some();
                        }
                    }
                }
            }
        }

        // The timeout for batching will happen at:
        let batching_deadline =
            flow.primary_trigger().trigger_time() + *transform_rule.max_batching_interval();

        // Accumulated something if at least some input changed or watermark was touched
        let accumulated_something = accumulated_records_count > 0 || watermark_modified;

        // The condition is satisfied if
        //   - we crossed the number of new records thresholds
        //   - or waited long enough, assuming
        //      - there is at least some change of the inputs
        //      - watermark got touched
        let satisfied = accumulated_something
            && (accumulated_records_count >= transform_rule.min_records_to_await()
                || evaluation_time >= batching_deadline);

        // Set batching condition data, but only during the first rule evaluation.
        if !matches!(
            flow.start_condition.as_ref(),
            Some(FlowStartCondition::Batching(_))
        ) {
            flow.set_relevant_start_condition(
                self.time_source.now(),
                FlowStartCondition::Batching(FlowStartConditionBatching {
                    active_transform_rule: *transform_rule,
                    batching_deadline,
                }),
            )
            .int_err()?;
        }

        //  If we accumulated at least something (records or watermarks),
        //   the upper bound of potential finish time for batching is known
        if accumulated_something || is_compacted {
            // Finish immediately if satisfied, or not later than the deadline
            let batching_finish_time = if satisfied || is_compacted {
                evaluation_time
            } else {
                batching_deadline
            };

            // Throttling boundary correction
            let corrected_finish_time =
                std::cmp::max(batching_finish_time, throttling_boundary_time);

            let should_activate = match self.find_planned_flow_activation_time(flow.flow_id) {
                Some(activation_time) => activation_time > corrected_finish_time,
                None => true,
            };
            if should_activate {
                self.enqueue_flow(flow.flow_id, corrected_finish_time)?;
            }

            // If batching is over, it's start condition is no longer valid.
            // However, set throttling condition, if it applies
            if (satisfied || is_compacted) && throttling_boundary_time > batching_finish_time {
                self.indicate_throttling_activity(
                    flow,
                    throttling_boundary_time,
                    batching_finish_time,
                )?;
            }
        }

        Ok(())
    }

    fn indicate_throttling_activity(
        &self,
        flow: &mut Flow,
        wake_up_at: DateTime<Utc>,
        shifted_from: DateTime<Utc>,
    ) -> Result<(), InternalError> {
        flow.set_relevant_start_condition(
            self.time_source.now(),
            FlowStartCondition::Throttling(FlowStartConditionThrottling {
                interval: self.run_config.mandatory_throttling_period,
                wake_up_at,
                shifted_from,
            }),
        )
        .int_err()?;
        Ok(())
    }

    fn find_pending_flow(&self, flow_key: &FlowKey) -> Option<FlowID> {
        let state = self.state.lock().unwrap();
        state.pending_flows.try_get_pending_flow(flow_key)
    }

    fn find_planned_flow_activation_time(&self, flow_id: FlowID) -> Option<DateTime<Utc>> {
        self.state
            .lock()
            .unwrap()
            .time_wheel
            .get_planned_flow_activation_time(flow_id)
    }

    #[tracing::instrument(level = "trace", skip_all, fields(?flow_key, ?trigger))]
    async fn make_new_flow(
        &self,
        flow_key: FlowKey,
        trigger: FlowTrigger,
        config_snapshot: Option<FlowConfigurationSnapshot>,
    ) -> Result<Flow, InternalError> {
        let flow = Flow::new(
            self.time_source.now(),
            self.flow_event_store.new_flow_id(),
            flow_key,
            trigger,
            config_snapshot,
        );

        let mut state = self.state.lock().unwrap();
        state
            .pending_flows
            .add_pending_flow(flow.flow_key.clone(), flow.flow_id);

        Ok(flow)
    }

    async fn flow_run_stats(&self, flow_key: &FlowKey) -> Result<FlowRunStats, InternalError> {
        match flow_key {
            FlowKey::Dataset(fk_dataset) => {
                self.flow_event_store
                    .get_dataset_flow_run_stats(&fk_dataset.dataset_id, fk_dataset.flow_type)
                    .await
            }
            FlowKey::System(fk_system) => {
                self.flow_event_store
                    .get_system_flow_run_stats(fk_system.flow_type)
                    .await
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, fields(%flow_id, %activation_time))]
    fn enqueue_flow(
        &self,
        flow_id: FlowID,
        activation_time: DateTime<Utc>,
    ) -> Result<(), InternalError> {
        self.state
            .lock()
            .unwrap()
            .time_wheel
            .activate_at(activation_time, flow_id);
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, fields(flow_id = %flow.flow_id))]
    async fn schedule_flow_task(
        &self,
        flow: &mut Flow,
        schedule_time: DateTime<Utc>,
    ) -> Result<TaskID, InternalError> {
        let logical_plan =
            self.make_task_logical_plan(&flow.flow_key, flow.config_snapshot.as_ref())?;

        let task = self
            .task_scheduler
            .create_task(logical_plan)
            .await
            .int_err()?;

        flow.set_relevant_start_condition(
            schedule_time,
            FlowStartCondition::Executor(FlowStartConditionExecutor {
                task_id: task.task_id,
            }),
        )
        .int_err()?;

        flow.on_task_scheduled(schedule_time, task.task_id)
            .int_err()?;
        flow.save(self.flow_event_store.as_ref()).await.int_err()?;

        let mut state = self.state.lock().unwrap();
        state
            .pending_flows
            .track_flow_task(flow.flow_id, task.task_id);

        Ok(task.task_id)
    }

    async fn abort_flow(&self, flow_id: FlowID) -> Result<(), InternalError> {
        // Mark flow as aborted
        let mut flow = Flow::load(flow_id, self.flow_event_store.as_ref())
            .await
            .int_err()?;

        self.abort_flow_impl(&mut flow).await
    }

    async fn abort_flow_impl(&self, flow: &mut Flow) -> Result<(), InternalError> {
        // Abort flow itself
        flow.abort(self.time_source.now()).int_err()?;
        flow.save(self.flow_event_store.as_ref()).await.int_err()?;

        // Cancel associated tasks, but first drop task -> flow associations
        {
            let mut state = self.state.lock().unwrap();
            for task_id in &flow.task_ids {
                state.pending_flows.untrack_flow_by_task(*task_id);
            }
        }
        for task_id in &flow.task_ids {
            self.task_scheduler.cancel_task(*task_id).await.int_err()?;
        }

        Ok(())
    }

    /// Creates task logical plan that corresponds to template
    pub fn make_task_logical_plan(
        &self,
        flow_key: &FlowKey,
        maybe_config_snapshot: Option<&FlowConfigurationSnapshot>,
    ) -> Result<LogicalPlan, InternalError> {
        match flow_key {
            FlowKey::Dataset(flow_key) => match flow_key.flow_type {
                DatasetFlowType::Ingest | DatasetFlowType::ExecuteTransform => {
                    let mut fetch_uncacheable = false;
                    if let Some(config_snapshot) = maybe_config_snapshot
                        && let FlowConfigurationSnapshot::Ingest(ingest_rule) = config_snapshot
                    {
                        fetch_uncacheable = ingest_rule.fetch_uncacheable;
                    }
                    Ok(LogicalPlan::UpdateDataset(UpdateDataset {
                        dataset_id: flow_key.dataset_id.clone(),
                        fetch_uncacheable,
                    }))
                }
                DatasetFlowType::HardCompaction => {
                    let mut max_slice_size: Option<u64> = None;
                    let mut max_slice_records: Option<u64> = None;
                    let mut keep_metadata_only = false;

                    if let Some(config_snapshot) = maybe_config_snapshot
                        && let FlowConfigurationSnapshot::Compaction(compaction_rule) =
                            config_snapshot
                    {
                        max_slice_size = compaction_rule.max_slice_size();
                        max_slice_records = compaction_rule.max_slice_records();
                        keep_metadata_only =
                            matches!(compaction_rule, CompactionRule::MetadataOnly(_));
                    };

                    Ok(LogicalPlan::HardCompactionDataset(HardCompactionDataset {
                        dataset_id: flow_key.dataset_id.clone(),
                        max_slice_size,
                        max_slice_records,
                        keep_metadata_only,
                    }))
                }
                DatasetFlowType::Reset => {
                    if let Some(config_rule) = maybe_config_snapshot
                        && let FlowConfigurationSnapshot::Reset(reset_rule) = config_rule
                    {
                        return Ok(LogicalPlan::Reset(ResetDataset {
                            dataset_id: flow_key.dataset_id.clone(),
                            new_head_hash: reset_rule.new_head_hash.clone(),
                            old_head_hash: reset_rule.old_head_hash.clone(),
                            recursive: reset_rule.recursive,
                        }));
                    }
                    InternalError::bail("Reset flow cannot be called without configuration")
                }
            },
            FlowKey::System(flow_key) => {
                match flow_key.flow_type {
                    // TODO: replace on correct logical plan
                    SystemFlowType::GC => Ok(LogicalPlan::Probe(Probe {
                        dataset_id: None,
                        busy_time: Some(std::time::Duration::from_secs(20)),
                        end_with_outcome: Some(TaskOutcome::Success(TaskResult::Empty)),
                    })),
                }
            }
        }
    }

    async fn make_downstream_dependencies_flow_plans(
        &self,
        fk_dataset: &FlowKeyDataset,
        maybe_config_snapshot: Option<&FlowConfigurationSnapshot>,
    ) -> Result<Vec<DownstreamDependencyFlowPlan>, InternalError> {
        // ToDo: extend dependency graph with possibility to fetch downstream
        // dependencies by owner
        let dependent_dataset_ids: Vec<_> = self
            .dependency_graph_service
            .get_downstream_dependencies(&fk_dataset.dataset_id)
            .await
            .int_err()?
            .collect()
            .await;

        let mut plans: Vec<DownstreamDependencyFlowPlan> = vec![];
        if dependent_dataset_ids.is_empty() {
            return Ok(plans);
        }

        match self.classify_dependent_trigger_type(fk_dataset.flow_type, maybe_config_snapshot) {
            DownstreamDependencyTriggerType::TriggerAllEnabledExecuteTransform => {
                let guard = self.state.lock().unwrap();
                for dataset_id in dependent_dataset_ids {
                    if let Some(transform_rule) =
                        guard.active_configs.try_get_dataset_transform_rule(
                            &dataset_id,
                            DatasetFlowType::ExecuteTransform,
                        )
                    {
                        plans.push(DownstreamDependencyFlowPlan {
                            flow_key: FlowKeyDataset::new(
                                dataset_id,
                                DatasetFlowType::ExecuteTransform,
                            )
                            .into(),
                            flow_trigger_context: FlowTriggerContext::Batching(transform_rule),
                            maybe_config_snapshot: None,
                        });
                    };
                }
            }

            DownstreamDependencyTriggerType::TriggerOwnHardCompaction => {
                let dataset_owner_account_ids = self
                    .dataset_ownership_service
                    .get_dataset_owners(&fk_dataset.dataset_id)
                    .await?;

                for dependent_dataset_id in dependent_dataset_ids {
                    for owner_account_id in &dataset_owner_account_ids {
                        if self
                            .dataset_ownership_service
                            .is_dataset_owned_by(&dependent_dataset_id, owner_account_id)
                            .await?
                        {
                            plans.push(DownstreamDependencyFlowPlan {
                                flow_key: FlowKeyDataset::new(
                                    dependent_dataset_id.clone(),
                                    DatasetFlowType::HardCompaction,
                                )
                                .into(),
                                flow_trigger_context: FlowTriggerContext::Unconditional,
                                // Currently we trigger Hard compaction recursively only in keep
                                // metadata only mode
                                maybe_config_snapshot: Some(FlowConfigurationSnapshot::Compaction(
                                    CompactionRule::MetadataOnly(CompactionRuleMetadataOnly {
                                        recursive: true,
                                    }),
                                )),
                            });
                            break;
                        }
                    }
                }
            }

            DownstreamDependencyTriggerType::Empty => {}
        }

        Ok(plans)
    }

    fn classify_dependent_trigger_type(
        &self,
        dataset_flow_type: DatasetFlowType,
        maybe_config_snapshot: Option<&FlowConfigurationSnapshot>,
    ) -> DownstreamDependencyTriggerType {
        match dataset_flow_type {
            DatasetFlowType::Ingest | DatasetFlowType::ExecuteTransform => {
                DownstreamDependencyTriggerType::TriggerAllEnabledExecuteTransform
            }
            DatasetFlowType::HardCompaction => {
                if let Some(config_snapshot) = &maybe_config_snapshot
                    && let FlowConfigurationSnapshot::Compaction(compaction_rule) = config_snapshot
                {
                    if compaction_rule.recursive() {
                        DownstreamDependencyTriggerType::TriggerOwnHardCompaction
                    } else {
                        DownstreamDependencyTriggerType::Empty
                    }
                } else {
                    DownstreamDependencyTriggerType::TriggerAllEnabledExecuteTransform
                }
            }
            DatasetFlowType::Reset => {
                if let Some(config_snapshot) = &maybe_config_snapshot
                    && let FlowConfigurationSnapshot::Reset(reset_rule) = config_snapshot
                    && reset_rule.recursive
                {
                    DownstreamDependencyTriggerType::TriggerOwnHardCompaction
                } else {
                    DownstreamDependencyTriggerType::Empty
                }
            }
        }
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

#[async_trait::async_trait]
impl FlowService for FlowServiceImpl {
    /// Runs the update main loop
    #[tracing::instrument(level = "info", skip_all)]
    async fn run(&self, planned_start_time: DateTime<Utc>) -> Result<(), InternalError> {
        // Mark running started
        self.state.lock().unwrap().running = true;

        // Initial scheduling
        DatabaseTransactionRunner::new(self.catalog.clone())
            .transactional_with2(
                |flow_configuration_service: Arc<dyn FlowConfigurationService>,
                 outbox: Arc<dyn Outbox>| async move {
                    let start_time = self.round_time(planned_start_time)?;
                    self.initialize_auto_polling_flows_from_configurations(
                        flow_configuration_service.as_ref(),
                        start_time,
                    )
                    .await?;

                    // Publish progress event
                    outbox
                        .post_message(
                            MESSAGE_PRODUCER_KAMU_FLOW_SERVICE,
                            FlowServiceUpdatedMessage {
                                update_time: start_time,
                                update_details: FlowServiceUpdateDetails::Loaded,
                            },
                        )
                        .await?;

                    Ok(())
                },
            )
            .await?;

        // Main scanning loop
        let main_loop_span = tracing::debug_span!("FlowService main loop");
        let _ = main_loop_span.enter();

        loop {
            let current_time = self.time_source.now();

            // Do we have a timeslot scheduled?
            let maybe_nearest_activation_time = {
                let state = self.state.lock().unwrap();
                state.time_wheel.nearest_activation_moment()
            };

            // Is it time to execute it yet?
            if let Some(nearest_activation_time) = maybe_nearest_activation_time
                && nearest_activation_time <= current_time
            {
                // Run scheduling for current time slot. Should not throw any errors
                self.run_current_timeslot(nearest_activation_time).await?;

                // Publish progress event
                DatabaseTransactionRunner::new(self.catalog.clone())
                    .transactional_with(|outbox: Arc<dyn Outbox>| async move {
                        outbox
                            .post_message(
                                MESSAGE_PRODUCER_KAMU_FLOW_SERVICE,
                                FlowServiceUpdatedMessage {
                                    update_time: nearest_activation_time,
                                    update_details: FlowServiceUpdateDetails::ExecutedTimeslot,
                                },
                            )
                            .await
                    })
                    .await?;
            }

            self.time_source.sleep(self.run_config.awaiting_step).await;
        }
    }

    /// Triggers the specified flow manually, unless it's already waiting
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(?flow_key, %initiator_account_id)
    )]
    async fn trigger_manual_flow(
        &self,
        trigger_time: DateTime<Utc>,
        flow_key: FlowKey,
        initiator_account_id: AccountID,
        config_snapshot_maybe: Option<FlowConfigurationSnapshot>,
    ) -> Result<FlowState, RequestFlowError> {
        let activation_time = self.round_time(trigger_time)?;

        self.trigger_flow_common(
            &flow_key,
            FlowTrigger::Manual(FlowTriggerManual {
                trigger_time: activation_time,
                initiator_account_id,
            }),
            FlowTriggerContext::Unconditional,
            config_snapshot_maybe,
        )
        .await
        .map_err(RequestFlowError::Internal)
    }

    /// Returns states of flows associated with a given dataset
    /// ordered by creation time from newest to oldest
    /// Applies specified filters
    #[tracing::instrument(level = "debug", skip_all, fields(%dataset_id, ?filters, ?pagination))]
    async fn list_all_flows_by_dataset(
        &self,
        dataset_id: &DatasetID,
        filters: DatasetFlowFilters,
        pagination: FlowPaginationOpts,
    ) -> Result<FlowStateListing, ListFlowsByDatasetError> {
        let total_count = self
            .flow_event_store
            .get_count_flows_by_dataset(dataset_id, &filters)
            .await?;

        let dataset_id = dataset_id.clone();

        let matched_stream = Box::pin(async_stream::try_stream! {
            let relevant_flow_ids: Vec<_> = self
                .flow_event_store
                .get_all_flow_ids_by_dataset(&dataset_id, filters, pagination)
                .try_collect()
                .await?;

            // TODO: implement batch loading
            for flow_id in relevant_flow_ids {
                let flow = Flow::load(flow_id, self.flow_event_store.as_ref()).await.int_err()?;
                yield flow.into();
            }
        });

        Ok(FlowStateListing {
            matched_stream,
            total_count,
        })
    }

    /// Returns initiators of flows associated with a given dataset
    /// ordered by creation time from newest to oldest
    #[tracing::instrument(level = "debug", skip_all, fields(%dataset_id))]
    async fn list_all_flow_initiators_by_dataset(
        &self,
        dataset_id: &DatasetID,
    ) -> Result<FlowInitiatorListing, ListFlowsByDatasetError> {
        Ok(FlowInitiatorListing {
            matched_stream: self
                .flow_event_store
                .get_unique_flow_initiator_ids_by_dataset(dataset_id),
        })
    }

    /// Returns states of flows associated with a given account
    /// ordered by creation time from newest to oldest
    /// Applies specified filters
    #[tracing::instrument(level = "debug", skip_all, fields(%account_id, ?filters, ?pagination))]
    async fn list_all_flows_by_account(
        &self,
        account_id: &AccountID,
        filters: AccountFlowFilters,
        pagination: FlowPaginationOpts,
    ) -> Result<FlowStateListing, ListFlowsByDatasetError> {
        let owned_dataset_ids = self
            .dataset_ownership_service
            .get_owned_datasets(account_id)
            .await
            .map_err(ListFlowsByDatasetError::Internal)?;

        let filtered_dataset_ids = if !filters.by_dataset_ids.is_empty() {
            owned_dataset_ids
                .into_iter()
                .filter(|dataset_id| filters.by_dataset_ids.contains(dataset_id))
                .collect()
        } else {
            owned_dataset_ids
        };

        let mut total_count = 0;
        let dataset_flow_filters = DatasetFlowFilters {
            by_flow_status: filters.by_flow_status,
            by_flow_type: filters.by_flow_type,
            by_initiator: filters.by_initiator,
        };

        for dataset_id in &filtered_dataset_ids {
            total_count += self
                .flow_event_store
                .get_count_flows_by_dataset(dataset_id, &dataset_flow_filters)
                .await?;
        }

        let account_dataset_ids: HashSet<DatasetID> = HashSet::from_iter(filtered_dataset_ids);

        let matched_stream = Box::pin(async_stream::try_stream! {
            let relevant_flow_ids: Vec<_> = self
                .flow_event_store
                .get_all_flow_ids_by_datasets(account_dataset_ids, &dataset_flow_filters, pagination)
                .try_collect()
                .await
                .int_err()?;

            // TODO: implement batch loading
            for flow_id in relevant_flow_ids {
                let flow = Flow::load(flow_id, self.flow_event_store.as_ref()).await.int_err()?;
                yield flow.into();
            }
        });

        Ok(FlowStateListing {
            matched_stream,
            total_count,
        })
    }

    /// Returns datasets with flows associated with a given account
    /// ordered by creation time from newest to oldest.
    #[tracing::instrument(level = "debug", skip_all, fields(%account_id))]
    async fn list_all_datasets_with_flow_by_account(
        &self,
        account_id: &AccountID,
    ) -> Result<FlowDatasetListing, ListFlowsByDatasetError> {
        let owned_dataset_ids = self
            .dataset_ownership_service
            .get_owned_datasets(account_id)
            .await
            .map_err(ListFlowsByDatasetError::Internal)?;

        let matched_stream = Box::pin(async_stream::try_stream! {
            for dataset_id in &owned_dataset_ids {
                let dataset_flows_count = self
                    .flow_event_store
                    .get_count_flows_by_dataset(dataset_id, &Default::default())
                    .await?;

                if dataset_flows_count > 0 {
                    yield dataset_id.clone();
                }
            }
        });

        Ok(FlowDatasetListing { matched_stream })
    }

    /// Returns states of system flows
    /// ordered by creation time from newest to oldest
    /// Applies specified filters
    #[tracing::instrument(level = "debug", skip_all, fields(?filters, ?pagination))]
    async fn list_all_system_flows(
        &self,
        filters: SystemFlowFilters,
        pagination: FlowPaginationOpts,
    ) -> Result<FlowStateListing, ListSystemFlowsError> {
        let total_count = self
            .flow_event_store
            .get_count_system_flows(&filters)
            .await
            .int_err()?;

        let matched_stream = Box::pin(async_stream::try_stream! {
            let relevant_flow_ids: Vec<_> = self
                .flow_event_store
                .get_all_system_flow_ids(filters, pagination)
                .try_collect()
                .await?;

            // TODO: implement batch loading
            for flow_id in relevant_flow_ids {
                let flow = Flow::load(flow_id, self.flow_event_store.as_ref()).await.int_err()?;
                yield flow.into();
            }
        });

        Ok(FlowStateListing {
            matched_stream,
            total_count,
        })
    }

    /// Returns state of all flows, whether they are system-level or
    /// dataset-bound, ordered by creation time from newest to oldest
    #[tracing::instrument(level = "debug", skip_all, fields(?pagination))]
    async fn list_all_flows(
        &self,
        pagination: FlowPaginationOpts,
    ) -> Result<FlowStateListing, ListFlowsError> {
        let total_count = self.flow_event_store.get_count_all_flows().await?;

        let matched_stream = Box::pin(async_stream::try_stream! {
            let all_flows: Vec<_> = self
                .flow_event_store
                .get_all_flow_ids(pagination)
                .try_collect()
                .await?;

            // TODO: implement batch loading
            for flow_id in all_flows {
                let flow = Flow::load(flow_id, self.flow_event_store.as_ref()).await.int_err()?;
                yield flow.into();
            }
        });

        Ok(FlowStateListing {
            matched_stream,
            total_count,
        })
    }

    /// Returns current state of a given flow
    #[tracing::instrument(level = "debug", skip_all, fields(%flow_id))]
    async fn get_flow(&self, flow_id: FlowID) -> Result<FlowState, GetFlowError> {
        let flow = Flow::load(flow_id, self.flow_event_store.as_ref()).await?;
        Ok(flow.into())
    }

    /// Attempts to cancel the tasks already scheduled for the given flow
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(%flow_id)
    )]
    async fn cancel_scheduled_tasks(
        &self,
        flow_id: FlowID,
    ) -> Result<FlowState, CancelScheduledTasksError> {
        let mut flow = Flow::load(flow_id, self.flow_event_store.as_ref()).await?;

        // Cancel tasks for flows in Waiting/Running state.
        // Ignore in Finished state
        match flow.status() {
            FlowStatus::Waiting | FlowStatus::Running => {
                // Abort current flow and it's scheduled tasks
                self.abort_flow_impl(&mut flow).await?;
            }
            FlowStatus::Finished => { /* Skip, idempotence */ }
        }

        Ok(flow.into())
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

#[async_trait::async_trait]
impl FlowServiceTestDriver for FlowServiceImpl {
    /// Pretends running started
    fn mimic_running_started(&self) {
        let mut state = self.state.lock().unwrap();
        state.running = true;
    }

    /// Pretends it is time to schedule the given flow that was not waiting for
    /// anything else
    async fn mimic_flow_scheduled(
        &self,
        flow_id: FlowID,
        schedule_time: DateTime<Utc>,
    ) -> Result<TaskID, InternalError> {
        {
            let mut state = self.state.lock().unwrap();
            state.time_wheel.cancel_flow_activation(flow_id).int_err()?;
        }

        let mut flow = Flow::load(flow_id, self.flow_event_store.as_ref())
            .await
            .int_err()?;
        let task_id = self.schedule_flow_task(&mut flow, schedule_time).await?;
        Ok(task_id)
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

impl MessageConsumer for FlowServiceImpl {}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

#[async_trait::async_trait]
impl MessageConsumerT<TaskProgressMessage> for FlowServiceImpl {
    #[tracing::instrument(level = "debug", skip_all, fields(?message))]
    async fn consume_message(
        &self,
        target_catalog: &Catalog,
        message: &TaskProgressMessage,
    ) -> Result<(), InternalError> {
        match message {
            TaskProgressMessage::Running(message) => {
                // Is this a task associated with flows?
                let maybe_flow_id = {
                    let state = self.state.lock().unwrap();
                    if !state.running {
                        // Abort if running hasn't started yet
                        return Ok(());
                    }
                    state.pending_flows.try_get_flow_id_by_task(message.task_id)
                };

                if let Some(flow_id) = maybe_flow_id {
                    let mut flow = Flow::load(flow_id, self.flow_event_store.as_ref())
                        .await
                        .int_err()?;
                    flow.on_task_running(message.event_time, message.task_id)
                        .int_err()?;
                    flow.save(self.flow_event_store.as_ref()).await.int_err()?;

                    let outbox = target_catalog.get_one::<dyn Outbox>().unwrap();
                    outbox
                        .post_message(
                            MESSAGE_PRODUCER_KAMU_FLOW_SERVICE,
                            FlowServiceUpdatedMessage {
                                update_time: message.event_time,
                                update_details: FlowServiceUpdateDetails::FlowRunning,
                            },
                        )
                        .await?;
                }
            }
            TaskProgressMessage::Finished(message) => {
                // Is this a task associated with flows?
                let maybe_flow_id = {
                    let state = self.state.lock().unwrap();
                    if !state.running {
                        // Abort if running hasn't started yet
                        return Ok(());
                    }
                    state.pending_flows.try_get_flow_id_by_task(message.task_id)
                };

                let finish_time = self.round_time(message.event_time)?;

                if let Some(flow_id) = maybe_flow_id {
                    let mut flow = Flow::load(flow_id, self.flow_event_store.as_ref())
                        .await
                        .int_err()?;
                    flow.on_task_finished(
                        message.event_time,
                        message.task_id,
                        message.outcome.clone(),
                    )
                    .int_err()?;
                    flow.save(self.flow_event_store.as_ref()).await.int_err()?;

                    {
                        let mut state = self.state.lock().unwrap();
                        state.pending_flows.untrack_flow_by_task(message.task_id);
                        state.pending_flows.drop_pending_flow(&flow.flow_key);
                    }

                    // In case of success:
                    //  - execute followup method
                    if let Some(flow_result) = flow.try_result_as_ref()
                        && !flow_result.is_empty()
                    {
                        match flow.flow_key.get_type().success_followup_method() {
                            FlowSuccessFollowupMethod::Ignore => {}
                            FlowSuccessFollowupMethod::TriggerDependent => {
                                self.enqueue_dependent_flows(finish_time, &flow, flow_result)
                                    .await?;
                            }
                        }
                    }

                    // In case of success:
                    //  - enqueue next auto-polling flow cycle
                    if message.outcome.is_success() {
                        self.try_enqueue_scheduled_auto_polling_flow_if_enabled(
                            finish_time,
                            &flow.flow_key,
                        )
                        .await?;
                    }

                    let outbox = target_catalog.get_one::<dyn Outbox>().unwrap();
                    outbox
                        .post_message(
                            MESSAGE_PRODUCER_KAMU_FLOW_SERVICE,
                            FlowServiceUpdatedMessage {
                                update_time: message.event_time,
                                update_details: FlowServiceUpdateDetails::FlowFinished,
                            },
                        )
                        .await?;

                    // TODO: retry logic in case of failed outcome
                }
            }
        }

        Ok(())
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

#[async_trait::async_trait]
impl MessageConsumerT<FlowConfigurationUpdatedMessage> for FlowServiceImpl {
    #[tracing::instrument(level = "debug", skip_all, fields(?message))]
    async fn consume_message(
        &self,
        _: &Catalog,
        message: &FlowConfigurationUpdatedMessage,
    ) -> Result<(), InternalError> {
        if message.paused {
            let maybe_pending_flow_id = {
                let mut state = self.state.lock().unwrap();
                if !state.running {
                    // Abort if running hasn't started yet
                    return Ok(());
                };

                state.active_configs.drop_flow_config(&message.flow_key);

                let maybe_pending_flow_id =
                    state.pending_flows.drop_pending_flow(&message.flow_key);
                if let Some(flow_id) = &maybe_pending_flow_id {
                    state
                        .time_wheel
                        .cancel_flow_activation(*flow_id)
                        .int_err()?;
                }
                maybe_pending_flow_id
            };

            if let Some(flow_id) = maybe_pending_flow_id {
                self.abort_flow(flow_id).await?;
            }
        } else {
            {
                let state = self.state.lock().unwrap();
                if !state.running {
                    // Abort if running hasn't started yet
                    return Ok(());
                };
            }

            let activation_time = self.round_time(message.event_time)?;
            self.activate_flow_configuration(
                activation_time,
                message.flow_key.clone(),
                message.rule.clone(),
            )
            .await?;
        }

        Ok(())
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

#[async_trait::async_trait]
impl MessageConsumerT<DatasetLifecycleMessage> for FlowServiceImpl {
    #[tracing::instrument(level = "debug", skip_all, fields(?message))]
    async fn consume_message(
        &self,
        _: &Catalog,
        message: &DatasetLifecycleMessage,
    ) -> Result<(), InternalError> {
        match message {
            DatasetLifecycleMessage::Deleted(message) => {
                let flow_ids_2_abort = {
                    let mut state = self.state.lock().unwrap();
                    if !state.running {
                        // Abort if running hasn't started yet
                        return Ok(());
                    };

                    state
                        .active_configs
                        .drop_dataset_configs(&message.dataset_id);

                    // For every possible dataset flow:
                    //  - drop it from pending state
                    //  - drop queued activations
                    //  - collect ID of aborted flow
                    let mut flow_ids_2_abort: Vec<_> =
                        Vec::with_capacity(DatasetFlowType::all().len());
                    for flow_type in DatasetFlowType::all() {
                        if let Some(flow_id) = state
                            .pending_flows
                            .drop_dataset_pending_flow(&message.dataset_id, *flow_type)
                        {
                            flow_ids_2_abort.push(flow_id);
                            state.time_wheel.cancel_flow_activation(flow_id).int_err()?;
                        }
                    }
                    flow_ids_2_abort
                };

                // Abort matched flows
                for flow_id in flow_ids_2_abort {
                    let mut flow = Flow::load(flow_id, self.flow_event_store.as_ref())
                        .await
                        .int_err()?;
                    flow.abort(self.time_source.now()).int_err()?;
                    flow.save(self.flow_event_store.as_ref()).await.int_err()?;
                }

                // Not deleting task->update association, it should be safe.
                // Most of the time the outcome of the task will be "Cancelled".
                // Even if task squeezes to succeed in between cancellations,
                // it's safe:
                //   - we will record a successful update, no consequence
                //   - no further updates will be attempted (schedule
                //     deactivated above)
                //   - no dependent tasks will be launched (dependency graph
                //     erases neighbors)
            }

            DatasetLifecycleMessage::Created(_)
            | DatasetLifecycleMessage::DependenciesUpdated(_) => {
                // No action required
            }
        }

        Ok(())
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Eq, PartialEq)]
pub enum FlowTriggerContext {
    Unconditional,
    Scheduled(Schedule),
    Batching(TransformRule),
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Eq, PartialEq)]
pub struct DownstreamDependencyFlowPlan {
    pub flow_key: FlowKey,
    pub flow_trigger_context: FlowTriggerContext,
    pub maybe_config_snapshot: Option<FlowConfigurationSnapshot>,
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

enum DownstreamDependencyTriggerType {
    TriggerAllEnabledExecuteTransform,
    TriggerOwnHardCompaction,
    Empty,
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////
