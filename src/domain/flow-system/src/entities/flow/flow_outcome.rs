// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

/////////////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowOutcome {
    /// Update succeeded
    Success,
    /// Update failed to complete, even after retry logic
    Failed,
    /// Update was cancelled by a user
    Cancelled,
    /// Update was aborted by system by force
    Aborted,
}

/////////////////////////////////////////////////////////////////////////////////////////