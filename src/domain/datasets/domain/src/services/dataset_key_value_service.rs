// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::HashMap;

use internal_error::InternalError;
use thiserror::Error;

use crate::{DatasetEnvVar, DatasetEnvVarNotFoundError, DatasetEnvVarValue};

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

pub trait DatasetKeyValueService: Sync + Send {
    fn find_dataset_env_var_value_by_key(
        &self,
        dataset_env_var_key: &str,
        dataset_env_vars: &HashMap<String, DatasetEnvVar>,
    ) -> Result<DatasetEnvVarValue, FindDatasetEnvVarError>;
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////

#[derive(Error, Debug)]
pub enum FindDatasetEnvVarError {
    #[error(transparent)]
    NotFound(#[from] DatasetEnvVarNotFoundError),

    #[error(transparent)]
    Internal(#[from] InternalError),
}

////////////////////////////////////////////////////////////////////////////////////////////////////////////////////////
