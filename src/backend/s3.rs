use crate::backend::Backend;
use crate::{BuildEvent, CHECKSUM_FILENAME, get_cache_dir_checksum};
use aws_config::BehaviorVersion;
use aws_config::retry::RetryConfig;
use aws_sdk_cloudfront::types::{InvalidationBatch, Paths};
use aws_sdk_s3::primitives::ByteStream;
use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Receiver;
use walkdir::WalkDir;

/// Maximum number of wildcard invalidation paths allowed per CloudFront invalidation request.
const CLOUDFRONT_MAX_WILDCARD_PATHS: usize = 15;

/// Whether the startup synchronization should remove bucket directories that
/// have no local artifact, and if not, why not.
#[derive(Debug, PartialEq, Eq)]
enum OrphanSweep {
    Run,
    Skip(&'static str),
}

/// Decides whether to run the sweep.
///
/// It is a reconciliation, not the mechanism by which deletions propagate: a
/// removal seen while the server is running is published as it happens
/// (BuildEvent::Delete). All the sweep adds is catching up on removals that
/// happened while it was not running -- so declining to run it is cheap, and
/// running it wrongly is not.
///
/// It is declined when there is no local artifact at all. An empty artifact
/// directory is not the statement "every build was deleted"; it is a machine
/// that has not built yet -- a fresh disk, a restored host, a build plane
/// brought up on demand -- and sweeping there empties the bucket of an event
/// that is still running. The skipped sweep costs one reconciliation: the
/// builds that follow announce themselves, and the next start with a
/// populated cache catches whatever is genuinely stale.
///
/// A partial cache is not treated as empty, and deliberately so: this only
/// rules out the case that is unambiguous. Where the artifact directory is
/// not the durable record of what exists, prune-orphans=false is the answer.
fn orphan_sweep(prune_orphans: bool, local_builds: usize, bucket_builds: usize) -> OrphanSweep {
    if !prune_orphans {
        return OrphanSweep::Skip("removal is disabled (prune-orphans=false)");
    }
    if local_builds == 0 && bucket_builds > 0 {
        return OrphanSweep::Skip(
            "this host has no local artifacts at all, which means it has not built yet rather \
             than that every build was deleted",
        );
    }
    OrphanSweep::Run
}

#[derive(Debug)]
pub(crate) struct S3Backend {
    bucket: String,
    path_prefix: String,
    /// Whether the startup sweep removes bucket directories with no local
    /// cache. On by default, as it always was. Turned off where the artifact
    /// directory is not the durable record of what exists -- a build plane
    /// that is brought up on demand, or whose disk does not outlive it --
    /// since there the local cache is not evidence that anything was deleted.
    /// Removals seen while running are propagated either way.
    prune_orphans: bool,
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

        let prune_orphans = match options.get("prune-orphans").map(String::as_str) {
            None | Some("true") => true,
            Some("false") => false,
            Some(other) => anyhow::bail!(
                "backend option \"prune-orphans\" must be \"true\" or \"false\", not {other:?}"
            ),
        };
        debug!("Orphan removal on startup: {}", prune_orphans);

        let backend = Self {
            bucket,
            path_prefix,
            prune_orphans,
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
        namespaces: &HashSet<String>,
        mut rx: Receiver<BuildEvent>,
    ) -> Result<(), anyhow::Error> {
        // Check that we have sufficient IAM permissions. Better to do this up-front than to
        // unexpectedly fail at runtime.
        info!("Checking IAM permissions");
        self.test_permissions().await?;

        // Sync existing artifacts
        info!("Syncing current artifact cache to S3");
        self.synchronize(cache_dir, namespaces).await?;

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
            let mut processing_error: Option<anyhow::Error> = None;
            for event in events {
                match event {
                    BuildEvent::Create(build) => {
                        info!("Uploading artifacts for build {}", build);
                        if let Err(e) = self.upload_cache_dir(cache_dir, &build).await {
                            processing_error = Some(e);
                            break;
                        }
                    }
                    BuildEvent::Update(build) => {
                        info!("Updating artifacts for build {}", build);
                        if let Err(e) = self.delete_bucket_dir(&build).await {
                            processing_error = Some(e);
                            break;
                        }
                        // S3 content changed after delete; capture upload result then
                        // record the build for invalidation regardless of upload outcome.
                        let upload_result = self.upload_cache_dir(cache_dir, &build).await;
                        invalidation_builds.push(build);
                        if let Err(e) = upload_result {
                            processing_error = Some(e);
                            break;
                        }
                    }
                    BuildEvent::Delete(build) => {
                        info!("Removing artifacts for build {}", build);
                        if let Err(e) = self.delete_bucket_dir(&build).await {
                            processing_error = Some(e);
                            break;
                        }
                        invalidation_builds.push(build);
                    }
                }
            }

            // Always attempt to flush invalidations for builds whose S3 content
            // changed, even if a later operation failed, to avoid serving stale
            // cached content from CloudFront.
            if !invalidation_builds.is_empty()
                && let Err(inv_err) = self.create_invalidation(&invalidation_builds).await
            {
                if let Some(proc_err) = processing_error {
                    error!(
                        "Additionally failed to flush CloudFront invalidations: {}",
                        inv_err
                    );
                    return Err(proc_err);
                }
                return Err(inv_err);
            }
            if let Some(e) = processing_error {
                return Err(e);
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
        let test_filename = format!("{}{}", self.path_prefix, "iam_test");
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
            let path = format!("/{}", test_filename);
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
            debug!("Uploading object: {}", upload_path.display());
            let file = tokio::fs::File::open(&entry.path()).await?;
            let body = ByteStream::read_from().file(file).build().await?;
            self.s3_client
                .put_object()
                .bucket(&self.bucket)
                .key(
                    upload_path.to_str().unwrap_or_else(|| {
                        panic!("Failed to convert path {:?} to utf-8", upload_path)
                    }),
                )
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
            debug!("Deleting object: {}", key);
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
                    quantity, items
                );
                let paths = aws_sdk_cloudfront::types::Paths::builder()
                    .set_items(Some(items))
                    .quantity(quantity)
                    .build()?;
                let counter = self.invalidation_counter.fetch_add(1, Ordering::Relaxed);
                let now_millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
                let caller_reference = format!("{}-{}", now_millis, counter);
                let invalidation_batch = aws_sdk_cloudfront::types::InvalidationBatch::builder()
                    .paths(paths)
                    .caller_reference(caller_reference)
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
        let checksum_path = format!("{}{}/{}", self.path_prefix, build, CHECKSUM_FILENAME);
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
    async fn synchronize(
        &self,
        cache_dir: &Path,
        namespaces: &HashSet<String>,
    ) -> Result<(), anyhow::Error> {
        // Get build keys and paths of all local cache directories
        let cache_dirs = crate::watcher::cache_build_ids(cache_dir, namespaces)?;

        // Get all build keys with directories in the bucket. One listing of
        // the prefix itself and one more under each namespace: a build's
        // objects are one level below the prefix normally and two below it
        // where a cork build plane has sorted its bundles by destination, and
        // a delimited listing only ever reports the level it is given.
        let mut bucket_build_ids: HashSet<String> = HashSet::new();
        for under in
            std::iter::once(String::new()).chain(namespaces.iter().map(|name| format!("{name}/")))
        {
            let prefix = format!("{}{}", self.path_prefix, under);
            let mut token: Option<String> = None;
            loop {
                let mut request = self
                    .s3_client
                    .list_objects_v2()
                    .bucket(&self.bucket)
                    .prefix(&prefix)
                    .delimiter('/');
                if let Some(token) = token.take() {
                    request = request.continuation_token(token);
                }
                let resp = request.send().await?;
                if let Some(prefixes) = resp.common_prefixes {
                    bucket_build_ids.extend(prefixes.into_iter().filter_map(|p| {
                        let key = p
                            .prefix?
                            .strip_prefix(&self.path_prefix)?
                            .trim_end_matches('/')
                            .to_string();
                        // A namespace is not a build of its own; listing
                        // under it is what finds the builds inside it.
                        if namespaces.contains(&key) {
                            return None;
                        }
                        Some(key)
                    }));
                }
                if !resp.is_truncated.is_some_and(|t| t) {
                    break;
                }
                token = resp.next_continuation_token;
                if token.is_none() {
                    break;
                }
            }
        }

        // Collect build IDs that need CloudFront invalidation for batching
        let mut invalidation_builds: Vec<String> = Vec::new();
        let mut sync_error: Option<anyhow::Error> = None;

        // Ensure that all bucket directories are up to date
        for (build_id, build_cache_dir) in &cache_dirs {
            if bucket_build_ids.contains(build_id) {
                let bucket_checksum = match self.get_bucket_dir_checksum(build_id).await {
                    Ok(c) => c,
                    Err(e) => {
                        sync_error = Some(e);
                        break;
                    }
                };
                let needs_update = match bucket_checksum {
                    Some(bc) => match get_cache_dir_checksum(build_cache_dir) {
                        Ok(local) => bc != local,
                        Err(e) => {
                            sync_error = Some(e.into());
                            break;
                        }
                    },
                    None => true,
                };
                if !needs_update {
                    continue;
                }
                info!("Artifacts for build {} are outdated, reuploading", build_id);
                if let Err(e) = self.delete_bucket_dir(build_id).await {
                    sync_error = Some(e);
                    break;
                }
                invalidation_builds.push(build_id.clone());
                if let Err(e) = self.upload_cache_dir(cache_dir, build_id).await {
                    sync_error = Some(e);
                    break;
                }
            } else {
                info!(
                    "Artifacts for build {} not found in bucket, uploading",
                    build_id
                );
                if let Err(e) = self.upload_cache_dir(cache_dir, build_id).await {
                    sync_error = Some(e);
                    break;
                }
            }
        }

        // Remove any bucket directories without a corresponding local cache,
        // unless something says not to (see OrphanSweep).
        let sweep = match sync_error {
            Some(_) => OrphanSweep::Skip("an earlier step of this synchronization failed"),
            None => orphan_sweep(self.prune_orphans, cache_dirs.len(), bucket_build_ids.len()),
        };
        if let OrphanSweep::Skip(why) = sweep {
            let orphans = bucket_build_ids
                .iter()
                .filter(|id| !cache_dirs.contains_key(*id))
                .count();
            if orphans > 0 {
                warn!(
                    "Not removing {orphans} bucket directory (or directories) with no local \
                     artifact: {why}. Nothing has been removed from the bucket."
                );
            }
        } else {
            for build_id in &bucket_build_ids {
                if !&cache_dirs.contains_key(build_id) {
                    info!(
                        "Artifacts found in bucket for deleted build {}, removing",
                        build_id
                    );
                    if let Err(e) = self.delete_bucket_dir(build_id).await {
                        sync_error = Some(e);
                        break;
                    }
                    invalidation_builds.push(build_id.clone());
                }
            }
        }

        // Always attempt to flush invalidations for builds whose S3 content
        // changed, even if a later operation failed, to avoid serving stale
        // cached content from CloudFront.
        if !invalidation_builds.is_empty()
            && let Err(inv_err) = self.create_invalidation(&invalidation_builds).await
        {
            if let Some(s_err) = sync_error {
                error!(
                    "Additionally failed to flush CloudFront invalidations: {}",
                    inv_err
                );
                return Err(s_err);
            }
            return Err(inv_err);
        }
        if let Some(e) = sync_error {
            return Err(e);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ordinary case: some builds locally, some in the bucket, and the
    /// ones the bucket has that this host does not are stale.
    #[test]
    fn a_populated_host_sweeps() {
        assert_eq!(orphan_sweep(true, 12, 14), OrphanSweep::Run);
    }

    /// The case this guard exists for. A host with nothing local has not
    /// built yet -- a fresh disk, a restored machine, a build plane brought
    /// up on demand -- and sweeping there would empty the bucket of an event
    /// that is still running. Nothing about an empty artifact directory says
    /// the builds were deleted.
    #[test]
    fn a_host_with_nothing_local_does_not_sweep() {
        let OrphanSweep::Skip(why) = orphan_sweep(true, 0, 400) else {
            panic!("a host with no local artifacts swept a bucket holding 400 builds");
        };
        assert!(why.contains("not built yet"), "unhelpful reason: {why}");
    }

    /// Nothing local and nothing in the bucket is not the dangerous case,
    /// and is left to the sweep so that the empty-cache path is only ever
    /// taken when it actually prevents something.
    #[test]
    fn an_empty_bucket_is_not_the_guarded_case() {
        assert_eq!(orphan_sweep(true, 0, 0), OrphanSweep::Run);
    }

    /// A partial cache still sweeps. Only the unambiguous case is ruled out
    /// here; a host whose artifact directory is not the durable record of
    /// what exists turns the pass off instead.
    #[test]
    fn a_partial_cache_still_sweeps() {
        assert_eq!(orphan_sweep(true, 1, 400), OrphanSweep::Run);
    }

    /// Disabled means disabled, whatever is or is not on disk.
    #[test]
    fn prune_orphans_false_never_sweeps() {
        for (local, bucket) in [(12, 14), (0, 400), (0, 0), (5, 5)] {
            let OrphanSweep::Skip(why) = orphan_sweep(false, local, bucket) else {
                panic!("swept with prune-orphans=false ({local} local, {bucket} in bucket)");
            };
            assert!(
                why.contains("prune-orphans=false"),
                "unhelpful reason: {why}"
            );
        }
    }
}
