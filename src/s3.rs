use crate::{Backend, BackendCreationError, BuildEvent};
use async_trait::async_trait;
use aws_sdk_cloudfront::model::{InvalidationBatch, Paths};
use aws_sdk_s3::ByteStream;
use log::{debug, info};
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::mpsc::Receiver;

#[derive(Debug)]
pub struct S3 {
    bucket: String,
    path_prefix: String,
    cloudfront_distribution: Option<String>,
}

#[async_trait]
impl Backend for S3 {
    fn get_options() -> &'static [&'static str] {
        &["bucket", "path-prefix", "cloudfront-distribution"]
    }

    fn get_required_options() -> &'static [&'static str] {
        &["bucket"]
    }

    fn new(options: HashMap<&str, &str>) -> Result<Self, BackendCreationError> {
        let bucket_name = match options.get("bucket") {
            Some(bucket_name) => bucket_name,
            None => return Err(BackendCreationError),
        };
        let backend = Self {
            bucket: bucket_name.to_string(),
            path_prefix: options.get("path_prefix").unwrap_or(&"").to_string(),
            cloudfront_distribution: options
                .get("cloudfront-distribution")
                .map(|v| v.to_string()),
        };
        Ok(backend)
    }

    async fn run(
        &self,
        cache_dir: &Path,
        mut rx: Receiver<BuildEvent>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Create S3 and CloudFront clients
        let shared_config = aws_config::from_env().load().await;
        let s3_client = aws_sdk_s3::Client::new(&shared_config);
        let cf_client = match self.cloudfront_distribution {
            Some(_) => Some(aws_sdk_cloudfront::Client::new(&shared_config)),
            None => None,
        };

        // Check that we have sufficient IAM permissions. Better to do this up-front than to
        // unexpectedly fail at runtime.
        info!("Checking IAM permissions");
        self.test_permissions(&s3_client, &cf_client).await?;

        // Sync existing artifacts
        info!("Syncing current artifact cache to S3");
        self.synchronize(&cache_dir, &s3_client, &cf_client).await?;

        // Handle build events
        info!("Watching for changes. Press CTRL-C to exit.");
        while let Some(event) = rx.recv().await {
            match event {
                BuildEvent::Update(build) => {
                    info!("Updating objects for build {}", &build);
                    self.delete_bucket_dir(&build, &s3_client).await?;
                    self.upload_cache_dir(&cache_dir, &build, &s3_client)
                        .await?;
                    if (&cf_client).is_some() {
                        self.create_invalidation(&build, &cf_client.as_ref().unwrap())
                            .await?;
                    }
                }
                BuildEvent::Delete(build) => {
                    info!("Removing objects for build {}", &build);
                    self.delete_bucket_dir(&build, &s3_client).await?;
                    if (&cf_client).is_some() {
                        self.create_invalidation(&build, &cf_client.as_ref().unwrap())
                            .await?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl S3 {
    /// Test that the current IAM user has all necessary permissions.
    async fn test_permissions(
        &self,
        s3_client: &aws_sdk_s3::Client,
        cloudfront_client: &Option<aws_sdk_cloudfront::Client>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        debug!("Testing ListObjectsV2");
        s3_client
            .list_objects_v2()
            .bucket(&self.bucket)
            .send()
            .await?;

        debug!("Testing PutObject");
        const TEST_BODY: &[u8] = "test contents".as_bytes();
        let body = ByteStream::from_static(TEST_BODY);
        let test_filename = format!("{}{}", &self.path_prefix, "iam_test");
        s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(&test_filename)
            .body(body)
            .send()
            .await?;

        debug!("Testing HeadObject");
        s3_client
            .head_object()
            .bucket(&self.bucket)
            .key(&test_filename)
            .send()
            .await?;

        debug!("Testing GetObject");
        let resp = s3_client
            .get_object()
            .bucket(&self.bucket)
            .key(&test_filename)
            .send()
            .await?;
        let data = resp.body.collect().await;
        assert_eq!(TEST_BODY, data.unwrap().into_bytes());

        debug!("Testing DeleteObject");
        s3_client
            .delete_object()
            .bucket(&self.bucket)
            .key(&test_filename)
            .send()
            .await?;

        if let Some(cf_client) = cloudfront_client {
            debug!("Testing CreateInvalidation");
            let batch = InvalidationBatch::builder()
                .paths(Paths::builder().items(&test_filename).quantity(1).build())
                .caller_reference(chrono::Utc::now().to_string())
                .build();
            cf_client
                .create_invalidation()
                .distribution_id(self.cloudfront_distribution.as_ref().unwrap())
                .invalidation_batch(batch)
                .send()
                .await?;
        }

        Ok(())
    }

    /// Uploads the specified build's cache directory to the S3 bucket.
    async fn upload_cache_dir(
        &self,
        cache_dir: &Path,
        build: &str,
        s3_client: &aws_sdk_s3::Client,
    ) -> Result<(), Box<dyn std::error::Error>> {
        todo!()
    }

    /// Deletes the specified build's artifact directory from the S3 bucket.
    async fn delete_bucket_dir(
        &self,
        build: &str,
        s3_client: &aws_sdk_s3::Client,
    ) -> Result<(), Box<dyn std::error::Error>> {
        todo!()
    }

    /// Invalidates the specified build's artifact directory path from the CloudFront distribution.
    async fn create_invalidation(
        &self,
        build: &str,
        cloudfront_client: &aws_sdk_cloudfront::Client,
    ) -> Result<(), Box<dyn std::error::Error>> {
        todo!()
    }

    /// Perform a full synchronization of the cache directory to the S3 bucket.
    async fn synchronize(
        &self,
        cache_dir: &Path,
        s3_client: &aws_sdk_s3::Client,
        cloudfront_client: &Option<aws_sdk_cloudfront::Client>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        todo!()
    }
}
