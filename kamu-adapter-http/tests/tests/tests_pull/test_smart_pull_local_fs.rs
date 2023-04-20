// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use crate::{harness::ServerSideLocalFsHarness, tests::tests_pull::test_smart_pull_shared};

/////////////////////////////////////////////////////////////////////////////////////////

#[test_log::test(tokio::test)]
async fn test_smart_pull_local_fs_new_dataset() {
    let server_harness = ServerSideLocalFsHarness::new().await;
    test_smart_pull_shared::test_smart_pull_new_dataset(server_harness).await;
}

/////////////////////////////////////////////////////////////////////////////////////////

#[test_log::test(tokio::test)]
async fn test_smart_pull_local_fs_existing_up_to_date_dataset() {
    let server_harness = ServerSideLocalFsHarness::new().await;
    test_smart_pull_shared::test_smart_pull_existing_up_to_date_dataset(server_harness).await;
}

/////////////////////////////////////////////////////////////////////////////////////////

#[test_log::test(tokio::test)]
async fn test_smart_pull_local_fs_existing_evolved_dataset() {
    let server_harness = ServerSideLocalFsHarness::new().await;
    test_smart_pull_shared::test_smart_pull_existing_evolved_dataset(server_harness).await;
}

/////////////////////////////////////////////////////////////////////////////////////////

#[test_log::test(tokio::test)]
async fn test_smart_pull_local_fs_existing_advanced_dataset_fails() {
    let server_harness = ServerSideLocalFsHarness::new().await;
    test_smart_pull_shared::test_smart_pull_existing_advanced_dataset_fails(server_harness).await;
}

/////////////////////////////////////////////////////////////////////////////////////////
