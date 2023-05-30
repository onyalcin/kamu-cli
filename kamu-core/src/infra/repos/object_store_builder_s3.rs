// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::sync::Arc;

use dill::*;
use object_store::aws::AmazonS3Builder;
use url::Url;

use crate::domain::*;
use crate::infra::utils::s3_context::S3Context;

/////////////////////////////////////////////////////////////////////////////////////////

#[component(pub)]
pub struct ObjectStoreBuilderS3 {
    s3_context: S3Context,
    credentials: aws_sdk_s3::config::Credentials,
    allow_http: bool,
}

impl ObjectStoreBuilderS3 {
    pub fn new(
        s3_context: S3Context,
        credentials: aws_sdk_s3::config::Credentials,
        allow_http: bool,
    ) -> Self {
        Self {
            s3_context,
            credentials,
            allow_http,
        }
    }
}

/////////////////////////////////////////////////////////////////////////////////////////

impl ObjectStoreBuilder for ObjectStoreBuilderS3 {
    fn object_store_url(&self) -> Url {
        // TODO: This URL does not account for endpoint and it will collide in case we
        // work with multiple S3-like storages having same buckets names
        Url::parse(format!("s3://{}/", self.s3_context.bucket).as_str()).unwrap()
    }

    fn build_object_store(&self) -> Result<Arc<dyn object_store::ObjectStore>, InternalError> {
        // Endpoint and region are mandatory
        let endpoint = self.s3_context.endpoint.clone().unwrap();
        let region = self.s3_context.region().unwrap();

        let s3_builder = AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_region(region)
            .with_access_key_id(self.credentials.access_key_id())
            .with_secret_access_key(self.credentials.secret_access_key())
            .with_bucket_name(self.s3_context.bucket.clone())
            .with_allow_http(self.allow_http);

        let object_store = s3_builder.build().int_err()?;

        Ok(Arc::new(object_store))
    }
}

/////////////////////////////////////////////////////////////////////////////////////////