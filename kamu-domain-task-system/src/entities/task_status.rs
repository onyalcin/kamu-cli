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
pub enum TaskStatus {
    /// Task is waiting for capacity to be allocated to it
    Queued,
    /// Task is being executed
    Running,
    /// Task has reached a certain final outcome
    Finished(TaskOutcome),
}

/////////////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    /// Task succeeded
    Success,
    /// Task failed to complete
    Failed,
    /// Task was cancelled by a user
    Cancelled,
    // /// Task was dropped in favor of another task
    // Replaced(TaskID),
}

/////////////////////////////////////////////////////////////////////////////////////////
