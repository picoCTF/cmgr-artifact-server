use crate::{
    get_cache_dir_checksum, to_filename_str, Backend, BackendCreationError, BuildEvent,
    CHECKSUM_FILENAME,
};
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_cloudfront::types::{InvalidationBatch, Paths};
use aws_sdk_s3::primitives::ByteStream;
use log::{debug, info};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
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

    fn new(options: HashMap<String, String>) -> Result<Self, BackendCreationError> {
        let bucket = match options.get("bucket") {
            Some(bucket_name) => bucket_name.to_string(),
            None => return Err(BackendCreationError),
        };
        // If non-empty, path prefixes must include a trailing slash, but not a leading slash.
        // A root path prefix ("/") must be replaced with an empty string to avoid duplicate leading
        // slashes when used in S3 object keys. Normalize the prefix:
        let path_prefix = options
            .get("path-prefix")
            .unwrap_or(&String::from(""))
            .to_string();
        let mut path_prefix = path_prefix.trim_start_matches('/').to_string();
        if !path_prefix.is_empty() && !path_prefix.ends_with('/') {
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
        let shared_config = aws_config::defaults(BehaviorVersion::v2023_11_09())
            .load()
            .await;
        let s3_client = aws_sdk_s3::Client::new(&shared_config);
        let cf_client = self
            .cloudfront_distribution
            .as_ref()
            .map(|_| aws_sdk_cloudfront::Client::new(&shared_config));

        // Check that we have sufficient IAM permissions. Better to do this up-front than to
        // unexpectedly fail at runtime.
        info!("Checking IAM permissions");
        self.test_permissions(&s3_client, &cf_client).await?;

        // Sync existing artifacts
        info!("Syncing current artifact cache to S3");
        self.synchronize(cache_dir, &s3_client, &cf_client).await?;

        // Handle build events
        info!("Watching for changes. Press CTRL-C to exit.");
        while let Some(event) = rx.recv().await {
            match event {
                BuildEvent::Create(build) => {
                    info!("Uploading artifacts for build {}", &build);
                    self.upload_cache_dir(cache_dir, &build, &s3_client).await?;
                }
                BuildEvent::Update(build) => {
                    info!("Updating artifacts for build {}", &build);
                    self.delete_bucket_dir(&build, &s3_client).await?;
                    self.upload_cache_dir(cache_dir, &build, &s3_client).await?;
                    if (cf_client).is_some() {
                        self.create_invalidation(&build, cf_client.as_ref().unwrap())
                            .await?;
                    }
                }
                BuildEvent::Delete(build) => {
                    info!("Removing artifacts for build {}", &build);
                    self.delete_bucket_dir(&build, &s3_client).await?;
                    if (cf_client).is_some() {
                        self.create_invalidation(&build, cf_client.as_ref().unwrap())
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
                .paths(Paths::builder().items(path).quantity(1).build()?)
                .caller_reference(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("System time went backwards")
                        .as_millis()
                        .to_string(),
                )
                .build()?;
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
            let body = ByteStream::read_from().file(file).build().await?;
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
        if obj_keys.is_empty() {
            // DeleteObjects calls fail if made with an empty object array, so return early
            return Ok(());
        }
        for key in &obj_keys {
            debug!("Deleting object: {}", &key);
        }
        let delete_body = aws_sdk_s3::types::Delete::builder()
            .set_objects(Some(
                obj_keys
                    .into_iter()
                    .map(|k| {
                        aws_sdk_s3::types::ObjectIdentifier::builder()
                            .key(k)
                            .build()
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ))
            .build()?;
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
        debug!("Creating invalidation for path: {}", &path);
        let paths = aws_sdk_cloudfront::types::Paths::builder()
            .items(path)
            .quantity(1)
            .build()?;
        let invalidation_batch = aws_sdk_cloudfront::types::InvalidationBatch::builder()
            .paths(paths)
            .caller_reference(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("System time went backwards")
                    .as_millis()
                    .to_string(),
            )
            .build()?;
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
    ) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> {
        let checksum_path = format!("{}{}/{}", &self.path_prefix, build, CHECKSUM_FILENAME);
        let resp = s3_client
            .get_object()
            .bucket(&self.bucket)
            .key(&checksum_path)
            .send()
            .await;
        match resp {
            Ok(get_object_output) => {
                let data = get_object_output.body.collect().await?;
                Ok(Some(data.into_bytes().to_vec()))
            }
            Err(_) => Ok(None),
        }
    }

    /// Perform a full synchronization of the cache directory to the S3 bucket.
    async fn synchronize(
        &self,
        cache_dir: &Path,
        s3_client: &aws_sdk_s3::Client,
        cloudfront_client: &Option<aws_sdk_cloudfront::Client>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Get build IDs and paths of all local cache directories
        let mut cache_dirs: HashMap<String, PathBuf> = HashMap::new();
        for dir_entry in fs::read_dir(cache_dir)? {
            let path_buf = dir_entry?.path();
            if path_buf.is_dir() {
                let dir_name = to_filename_str(&path_buf);
                cache_dirs.insert(dir_name.into(), path_buf);
            }
        }

        // Get all build IDs with directories in bucket
        let mut bucket_build_ids: HashSet<String> = HashSet::new();
        let mut resp = s3_client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(&self.path_prefix)
            .delimiter('/')
            .send()
            .await?;
        if let Some(prefixes) = resp.common_prefixes {
            bucket_build_ids.extend(&mut prefixes.into_iter().map(|p| {
                p.prefix
                    .unwrap()
                    .strip_prefix(&self.path_prefix)
                    .unwrap()
                    .trim_end_matches('/')
                    .to_string()
            }));
        }
        while resp.is_truncated.is_some_and(|t| t) {
            resp = s3_client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&self.path_prefix)
                .delimiter('/')
                .continuation_token(resp.next_continuation_token.unwrap())
                .send()
                .await?;
            if let Some(prefixes) = resp.common_prefixes {
                bucket_build_ids.extend(&mut prefixes.into_iter().map(|p| {
                    p.prefix
                        .unwrap()
                        .strip_prefix(&self.path_prefix)
                        .unwrap()
                        .trim_end_matches('/')
                        .to_string()
                }));
            }
        }

        // Ensure that all bucket directories are up to date
        for (build_id, build_cache_dir) in &cache_dirs {
            if bucket_build_ids.contains(build_id) {
                let bucket_checksum = self.get_bucket_dir_checksum(build_id, s3_client).await?;
                if let Some(bucket_checksum) = bucket_checksum {
                    if bucket_checksum == get_cache_dir_checksum(build_cache_dir)? {
                        continue;
                    }
                }
                info!(
                    "Artifacts for build {} are outdated, reuploading",
                    &build_id
                );
                self.delete_bucket_dir(build_id, s3_client).await?;
                self.upload_cache_dir(cache_dir, build_id, s3_client)
                    .await?;
                if cloudfront_client.is_some() {
                    self.create_invalidation(build_id, cloudfront_client.as_ref().unwrap())
                        .await?;
                }
            } else {
                info!(
                    "Artifacts for build {} not found in bucket, uploading",
                    &build_id
                );
                self.upload_cache_dir(cache_dir, build_id, s3_client)
                    .await?;
            }
        }

        // Remove any bucket directories without a corresponding local cache
        for build_id in &bucket_build_ids {
            if !&cache_dirs.contains_key(build_id) {
                info!(
                    "Artifacts found in bucket for deleted build {}, removing",
                    &build_id
                );
                self.delete_bucket_dir(build_id, s3_client).await?;
                if cloudfront_client.is_some() {
                    self.create_invalidation(build_id, cloudfront_client.as_ref().unwrap())
                        .await?;
                }
            }
        }

        Ok(())
    }
}
