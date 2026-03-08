use crate::backend::Backend;
use crate::{BuildEvent, CHECKSUM_FILENAME, get_cache_dir_checksum};
use aws_config::BehaviorVersion;
use aws_config::retry::RetryConfig;
use aws_sdk_cloudfront::types::{InvalidationBatch, Paths};
use aws_sdk_s3::primitives::ByteStream;
use log::{debug, info};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Receiver;
use walkdir::WalkDir;

/// Maximum number of wildcard invalidation paths allowed per CloudFront invalidation request.
const CLOUDFRONT_MAX_WILDCARD_PATHS: usize = 15;

#[derive(Debug)]
pub(crate) struct S3Backend {
    bucket: String,
    path_prefix: String,
    cloudfront_distribution: Option<String>,
    s3_client: aws_sdk_s3::Client,
    cloudfront_client: Option<aws_sdk_cloudfront::Client>,
    invalidation_counter: AtomicU64,
}

impl Backend for S3Backend {
    async fn new(options: HashMap<String, String>) -> Result<Self, anyhow::Error> {
        let bucket = match options.get("bucket") {
            Some(bucket_name) => bucket_name.to_string(),
            None => anyhow::bail!("required backend option \"bucket\" not provided"),
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

        // Create S3 and CloudFront clients with adaptive retry to handle rate limiting
        let retry_config = RetryConfig::adaptive().with_max_attempts(10);
        let shared_config = aws_config::defaults(BehaviorVersion::latest())
            .retry_config(retry_config)
            .load()
            .await;
        let s3_client = aws_sdk_s3::Client::new(&shared_config);
        let cloudfront_client = options
            .get("cloudfront-distribution")
            .map(|_| aws_sdk_cloudfront::Client::new(&shared_config));

        let backend = Self {
            bucket,
            path_prefix,
            cloudfront_distribution: options
                .get("cloudfront-distribution")
                .map(|v| v.to_string()),
            s3_client,
            cloudfront_client,
            invalidation_counter: AtomicU64::new(0),
        };
        Ok(backend)
    }

    async fn run(
        &self,
        cache_dir: &Path,
        mut rx: Receiver<BuildEvent>,
    ) -> Result<(), anyhow::Error> {
        // Check that we have sufficient IAM permissions. Better to do this up-front than to
        // unexpectedly fail at runtime.
        info!("Checking IAM permissions");
        self.test_permissions().await?;

        // Sync existing artifacts
        info!("Syncing current artifact cache to S3");
        self.synchronize(cache_dir).await?;

        // Handle build events
        info!("Watching for changes. Press CTRL-C to exit.");
        while let Some(event) = rx.recv().await {
            let mut events = vec![event];
            // Drain any additional pending events for batching, but cap per-iteration drain
            let mut drained = 0usize;
            while drained < 1024 {
                match rx.try_recv() {
                    Ok(event) => {
                        events.push(event);
                        drained += 1;
                    }
                    Err(_) => {
                        break;
                    }
                }
            }

            let mut invalidation_builds: Vec<String> = Vec::new();
            for event in events {
                match event {
                    BuildEvent::Create(build) => {
                        info!("Uploading artifacts for build {}", &build);
                        self.upload_cache_dir(cache_dir, &build).await?;
                    }
                    BuildEvent::Update(build) => {
                        info!("Updating artifacts for build {}", &build);
                        self.delete_bucket_dir(&build).await?;
                        self.upload_cache_dir(cache_dir, &build).await?;
                        invalidation_builds.push(build);
                    }
                    BuildEvent::Delete(build) => {
                        info!("Removing artifacts for build {}", &build);
                        self.delete_bucket_dir(&build).await?;
                        invalidation_builds.push(build);
                    }
                }
            }
            if self.cloudfront_client.is_some() {
                self.create_invalidation(&invalidation_builds).await?;
            }
        }
        Ok(())
    }
}

impl S3Backend {
    /// Test that the current IAM user has all necessary permissions.
    async fn test_permissions(&self) -> Result<(), anyhow::Error> {
        debug!("Testing ListObjectsV2");
        self.s3_client
            .list_objects_v2()
            .bucket(&self.bucket)
            .send()
            .await?;

        debug!("Testing PutObject");
        const TEST_BODY: &[u8] = "test contents".as_bytes();
        let body = ByteStream::from_static(TEST_BODY);
        let test_filename = format!("{}{}", &self.path_prefix, "iam_test");
        self.s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(&test_filename)
            .body(body)
            .send()
            .await?;

        debug!("Testing GetObject");
        let resp = self
            .s3_client
            .get_object()
            .bucket(&self.bucket)
            .key(&test_filename)
            .send()
            .await?;
        let data = resp.body.collect().await;
        assert_eq!(TEST_BODY, data.unwrap().into_bytes());

        debug!("Testing DeleteObject");
        self.s3_client
            .delete_object()
            .bucket(&self.bucket)
            .key(&test_filename)
            .send()
            .await?;

        if let Some(cloudfront_client) = self.cloudfront_client.as_ref() {
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
            cloudfront_client
                .create_invalidation()
                .distribution_id(self.cloudfront_distribution.as_ref().unwrap())
                .invalidation_batch(batch)
                .send()
                .await?;
        }

        Ok(())
    }

    /// Uploads the specified build's cache directory to the S3 bucket.
    async fn upload_cache_dir(&self, cache_dir: &Path, build: &str) -> Result<(), anyhow::Error> {
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
            self.s3_client
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
    async fn delete_bucket_dir(&self, build: &str) -> Result<(), anyhow::Error> {
        let prefix = format!("{}{}/", self.path_prefix, build);
        let resp = self
            .s3_client
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
        self.s3_client
            .delete_objects()
            .bucket(&self.bucket)
            .delete(delete_body)
            .send()
            .await?;
        Ok(())
    }

    /// Invalidates the specified builds' artifact directory paths from the CloudFront distribution,
    /// if one is configured. If no distribution is configured or the list is empty, does nothing.
    /// Paths are batched into requests of up to CLOUDFRONT_MAX_WILDCARD_PATHS wildcard paths each.
    async fn create_invalidation(&self, builds: &[String]) -> Result<(), anyhow::Error> {
        if builds.is_empty() {
            return Ok(());
        }
        if let Some(cloudfront_client) = self.cloudfront_client.as_ref() {
            // Deduplicate build IDs to avoid wasting CloudFront invalidation paths
            let unique_builds: Vec<&String> =
                builds.iter().collect::<HashSet<_>>().into_iter().collect();
            for chunk in unique_builds.chunks(CLOUDFRONT_MAX_WILDCARD_PATHS) {
                let items: Vec<String> = chunk
                    .iter()
                    .map(|build| format!("/{}{}*", self.path_prefix, build))
                    .collect();
                let quantity = i32::try_from(items.len())?;
                debug!(
                    "Creating invalidation for {} path(s): {:?}",
                    quantity, &items
                );
                let paths = aws_sdk_cloudfront::types::Paths::builder()
                    .set_items(Some(items))
                    .quantity(quantity)
                    .build()?;
                let invalidation_batch = aws_sdk_cloudfront::types::InvalidationBatch::builder()
                    .paths(paths)
                    .caller_reference(
                        self.invalidation_counter
                            .fetch_add(1, Ordering::Relaxed)
                            .to_string(),
                    )
                    .build()?;
                cloudfront_client
                    .create_invalidation()
                    .distribution_id(self.cloudfront_distribution.as_ref().unwrap())
                    .invalidation_batch(invalidation_batch)
                    .send()
                    .await?;
            }
        }
        Ok(())
    }

    /// Retrieves a build's artifact directory checksum from the S3 bucket, if it exists.
    async fn get_bucket_dir_checksum(&self, build: &str) -> Result<Option<Vec<u8>>, anyhow::Error> {
        let checksum_path = format!("{}{}/{}", &self.path_prefix, build, CHECKSUM_FILENAME);
        let resp = self
            .s3_client
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
    async fn synchronize(&self, cache_dir: &Path) -> Result<(), anyhow::Error> {
        // Get build IDs and paths of all local cache directories
        let mut cache_dirs: HashMap<String, PathBuf> = HashMap::new();
        for dir_entry in fs::read_dir(cache_dir)? {
            let path_buf = dir_entry?.path();
            if path_buf.is_dir() {
                let dir_name = path_buf
                    .file_name()
                    .unwrap_or_else(|| panic!("Failed to get filename for path {:?}", &path_buf))
                    .to_str()
                    .unwrap_or_else(|| panic!("Failed to convert path {:?} to utf-8", &path_buf));
                cache_dirs.insert(dir_name.into(), path_buf);
            }
        }

        // Get all build IDs with directories in bucket
        let mut bucket_build_ids: HashSet<String> = HashSet::new();
        let mut resp = self
            .s3_client
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
            resp = self
                .s3_client
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

        // Collect build IDs that need CloudFront invalidation for batching
        let mut invalidation_builds: Vec<String> = Vec::new();

        // Ensure that all bucket directories are up to date
        for (build_id, build_cache_dir) in &cache_dirs {
            if bucket_build_ids.contains(build_id) {
                let bucket_checksum = self.get_bucket_dir_checksum(build_id).await?;
                if let Some(bucket_checksum) = bucket_checksum
                    && bucket_checksum == get_cache_dir_checksum(build_cache_dir)?
                {
                    continue;
                }
                info!(
                    "Artifacts for build {} are outdated, reuploading",
                    &build_id
                );
                self.delete_bucket_dir(build_id).await?;
                self.upload_cache_dir(cache_dir, build_id).await?;
                invalidation_builds.push(build_id.clone());
            } else {
                info!(
                    "Artifacts for build {} not found in bucket, uploading",
                    &build_id
                );
                self.upload_cache_dir(cache_dir, build_id).await?;
            }
        }

        // Remove any bucket directories without a corresponding local cache
        for build_id in &bucket_build_ids {
            if !&cache_dirs.contains_key(build_id) {
                info!(
                    "Artifacts found in bucket for deleted build {}, removing",
                    &build_id
                );
                self.delete_bucket_dir(build_id).await?;
                invalidation_builds.push(build_id.clone());
            }
        }

        // Send batched CloudFront invalidation for all modified/deleted builds
        self.create_invalidation(&invalidation_builds).await?;

        Ok(())
    }
}
