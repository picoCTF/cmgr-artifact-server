use crate::{Backend, BackendCreationError, BuildEvent, CHECKSUM_FILENAME};
use async_trait::async_trait;
use aws_sdk_cloudfront::model::{InvalidationBatch, Paths};
use aws_sdk_s3::ByteStream;
use log::{debug, info};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::Receiver;
use walkdir::WalkDir;

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
        let bucket = match options.get("bucket") {
            Some(bucket_name) => bucket_name.to_string(),
            None => return Err(BackendCreationError),
        };
        // If non-empty, path prefixes must include a trailing slash, but not a leading slash.
        // A root path prefix ("/") must be replaced with an empty string to avoid duplicate leading
        // slashes when used in S3 object keys. Normalize the prefix:
        let path_prefix = options.get("path-prefix").unwrap_or(&"").to_string();
        let mut path_prefix = path_prefix.trim_start_matches("/").to_string();
        if path_prefix.len() > 0 && path_prefix.chars().last().unwrap() != '/' {
            path_prefix.push('/');
        }
        debug!("Normalized path prefix: \"{}\"", path_prefix);

        let backend = Self {
            bucket,
            path_prefix,
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
            let path = format!("/{}", &test_filename);
            let batch = InvalidationBatch::builder()
                .paths(Paths::builder().items(path).quantity(1).build())
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
        let mut build_cache_dir = PathBuf::from(cache_dir);
        build_cache_dir.push(build);
        for entry in WalkDir::new(&build_cache_dir).min_depth(1) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let relative_path = &entry.path().strip_prefix(&build_cache_dir)?;
            let mut upload_path = PathBuf::from(&self.path_prefix);
            upload_path.push(build);
            upload_path.push(relative_path);
            debug!("Uploading object: {}", &upload_path.display());
            let file = tokio::fs::File::open(&entry.path()).await?;
            let body = ByteStream::from_file(file).await?;
            s3_client
                .put_object()
                .bucket(&self.bucket)
                .key(upload_path.to_str().unwrap_or_else(|| {
                    panic!("Failed to convert path {:?} to utf-8", &upload_path)
                }))
                .body(body)
                .send()
                .await?;
        }
        Ok(())
    }

    /// Deletes the specified build's artifact directory from the S3 bucket.
    async fn delete_bucket_dir(
        &self,
        build: &str,
        s3_client: &aws_sdk_s3::Client,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let prefix = format!("{}{}/", self.path_prefix, build);
        let resp = s3_client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .send()
            .await?;
        // Note: this assumes that a build will never have more than 1000 artifacts (the limit of a
        // single GetObjectsV2 response or DeleteObjects request). To handle over 1000 artifacts per
        // build, it would be necessary to check .is_truncated() and send additional requests using
        // continuation tokens.
        let obj_keys: Vec<String> = resp
            .contents
            .unwrap_or_default()
            .into_iter()
            .map(|o| o.key.unwrap())
            .collect();
        if obj_keys.len() == 0 {
            // DeleteObjects calls fail if made with an empty object array, so return early
            return Ok(());
        }
        for key in &obj_keys {
            debug!("Deleting object: {}", &key);
        }
        let delete_body = aws_sdk_s3::model::Delete::builder()
            .set_objects(Some(
                obj_keys
                    .into_iter()
                    .map(|k| {
                        aws_sdk_s3::model::ObjectIdentifier::builder()
                            .key(k)
                            .build()
                    })
                    .collect(),
            ))
            .build();
        s3_client
            .delete_objects()
            .bucket(&self.bucket)
            .delete(delete_body)
            .send()
            .await?;
        Ok(())
    }

    /// Invalidates the specified build's artifact directory path from the CloudFront distribution.
    async fn create_invalidation(
        &self,
        build: &str,
        cloudfront_client: &aws_sdk_cloudfront::Client,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path = format!("/{}{}*", self.path_prefix, build);
        info!("Creating invalidation for path: {}", &path);
        let paths = aws_sdk_cloudfront::model::Paths::builder()
            .items(path)
            .quantity(1)
            .build();
        let invalidation_batch = aws_sdk_cloudfront::model::InvalidationBatch::builder()
            .paths(paths)
            .caller_reference(chrono::Utc::now().to_string())
            .build();
        cloudfront_client
            .create_invalidation()
            .distribution_id(self.cloudfront_distribution.as_ref().unwrap())
            .invalidation_batch(invalidation_batch)
            .send()
            .await?;
        Ok(())
    }

    /// Retrieves a build's artifact directory checksum from the S3 bucket, if it exists.
    async fn get_bucket_dir_checksum(
        &self,
        build: &str,
        s3_client: &aws_sdk_s3::Client,
    ) -> Option<Vec<u8>> {
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
