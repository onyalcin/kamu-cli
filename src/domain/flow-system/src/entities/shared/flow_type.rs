// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

/////////////////////////////////////////////////////////////////////////////////////////

use crate::{BatchingRule, CompactingRule, Schedule};

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub enum DatasetFlowType {
    Ingest,
    ExecuteTransform,
    HardCompacting,
}

impl DatasetFlowType {
    pub fn all() -> &'static [DatasetFlowType] {
        &[Self::Ingest, Self::ExecuteTransform, Self::HardCompacting]
    }

    pub fn dataset_kind_restriction(&self) -> Option<opendatafabric::DatasetKind> {
        match self {
            DatasetFlowType::Ingest | DatasetFlowType::HardCompacting => {
                Some(opendatafabric::DatasetKind::Root)
            }
            DatasetFlowType::ExecuteTransform => Some(opendatafabric::DatasetKind::Derivative),
        }
    }

    pub fn config_restriction(&self, flow_configuration_type: &'static str) -> bool {
        match self {
            DatasetFlowType::Ingest => flow_configuration_type == std::any::type_name::<Schedule>(),
            DatasetFlowType::ExecuteTransform => {
                flow_configuration_type == std::any::type_name::<BatchingRule>()
            }
            DatasetFlowType::HardCompacting => {
                flow_configuration_type == std::any::type_name::<CompactingRule>()
            }
        }
    }
}

/////////////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum SystemFlowType {
    GC,
}

impl SystemFlowType {
    pub fn all() -> &'static [SystemFlowType] {
        &[Self::GC]
    }
}

/////////////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum AnyFlowType {
    Dataset(DatasetFlowType),
    System(SystemFlowType),
}

impl AnyFlowType {
    /// What should be the reaction on flow success
    pub fn success_followup_method(&self) -> FlowSuccessFollowupMethod {
        match self {
            AnyFlowType::Dataset(
                DatasetFlowType::Ingest
                | DatasetFlowType::ExecuteTransform
                | DatasetFlowType::HardCompacting,
            ) => FlowSuccessFollowupMethod::TriggerDependent,
            _ => FlowSuccessFollowupMethod::Ignore,
        }
    }
}

/////////////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FlowSuccessFollowupMethod {
    /// Nothing should happen if flow succeeds,
    Ignore,

    /// If flow succeeds, it's dependent flows should trigger
    TriggerDependent,
}

/////////////////////////////////////////////////////////////////////////////////////////
