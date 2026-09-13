use crate::backend::Backend;
use crate::{BuildEvent, BuildId, CHECKSUM_FILENAME, get_cache_dir_checksum};
use aws_config::BehaviorVersion;
use aws_config::retry::RetryConfig;
use aws_sdk_cloudfront::types::{InvalidationBatch, Paths};
use aws_sdk_s3::primitives::ByteStream;
use flate2::read::GzDecoder;
use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tar::Archive;
use tokio::sync::mpsc::Receiver;

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

/// Name of the file each tarball entry is spooled through on its way to the
/// bucket. It lives in the build's own cache directory, is truncated and
/// refilled per entry, and is removed when the upload ends however it ends.
const SPOOL_FILENAME: &str = ".__spool";

/// A file removed when it goes out of scope, however the scope ends. The
/// upload below returns early on any S3 or I/O error, and a spool left behind
/// would sit in the cache holding the largest file of a build that failed.
struct SpoolFile(PathBuf);

impl Drop for SpoolFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The object key suffix an artifact tarball entry is published under, or
/// None if it must not be published at all.
///
/// Extracting a tarball to disk has a backstop: `Archive::unpack` refuses to
/// write outside the directory it was given. Streaming entries straight into
/// object keys has none, and the names come out of a tarball this server did
/// not build. Two of them are not merely untidy:
///
/// - An **absolute** name. The key is composed by pushing the entry onto the
///   build's prefix, and pushing an absolute path replaces everything before
///   it -- so `/etc/passwd` would be written at the bucket root rather than
///   under the build, outside any prefix a retirement would ever delete.
/// - A name with **parent components**. It cannot traverse an S3 key, which
///   is a flat string, but the object then answers to a path no request
///   resolves to: CloudFront normalizes `a/../b` to `b` before it ever
///   reaches the bucket, so the file is published and permanently
///   unreachable.
///
/// Everything that is not a plain relative path of ordinary components is
/// therefore refused rather than cleaned up, and only regular files are
/// published: a symlink or a device node has no meaning as an object, and
/// guessing at one is how the first two get in by another door.
///
/// cork's cmgr rewrites every artifact archive before publishing it and
/// fails the build outright on any entry that is not a regular file or a
/// directory, so nothing it produces should ever be refused here. That is
/// its guarantee, though, not this one's: the tarballs in the artifact
/// directory are whatever wrote them, which for an older cmgr is an archive
/// that passed through no such pass at all.
fn artifact_object_path(entry: &Path, is_file: bool) -> Option<String> {
    if !is_file {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for component in entry.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str()?),
            // Dropped, not refused. `tar czf x.tar.gz -C dir .` -- which is
            // how a challenge's bundle is commonly made -- names every entry
            // "./file", and Path::components keeps a *leading* CurDir even
            // though it drops interior ones. Refusing it would reject
            // ordinary archives. "." on its own leaves nothing behind and
            // falls out as empty below.
            std::path::Component::CurDir => continue,
            // RootDir and Prefix are the absolute case, ParentDir the
            // traversal one.
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// What this backend was configured with, apart from the clients it talks to.
///
/// Kept separate from the clients because it is the whole of what decides
/// which objects a build is published as, and because a test can then pair it
/// with a client of its own (see S3Backend::from_parts) rather than one built
/// from an environment that would have to hold AWS credentials.
#[derive(Debug)]
struct S3Settings {
    bucket: String,
    path_prefix: String,
    prune_orphans: bool,
    cloudfront_distribution: Option<String>,
}

impl S3Settings {
    fn from_options(options: &HashMap<String, String>) -> Result<Self, anyhow::Error> {
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

        let prune_orphans = match options.get("prune-orphans").map(String::as_str) {
            None | Some("true") => true,
            Some("false") => false,
            Some(other) => anyhow::bail!(
                "backend option \"prune-orphans\" must be \"true\" or \"false\", not {other:?}"
            ),
        };
        debug!("Orphan removal on startup: {}", prune_orphans);

        Ok(Self {
            bucket,
            path_prefix,
            prune_orphans,
            cloudfront_distribution: options
                .get("cloudfront-distribution")
                .map(|v| v.to_string()),
        })
    }
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
        let settings = S3Settings::from_options(&options)?;

        // Create S3 and CloudFront clients with adaptive retry to handle rate limiting
        let retry_config = RetryConfig::adaptive().with_max_attempts(10);
        let shared_config = aws_config::defaults(BehaviorVersion::latest())
            .retry_config(retry_config)
            .load()
            .await;
        let s3_client = aws_sdk_s3::Client::new(&shared_config);
        let cloudfront_client = settings
            .cloudfront_distribution
            .as_ref()
            .map(|_| aws_sdk_cloudfront::Client::new(&shared_config));

        Ok(Self::from_parts(settings, s3_client, cloudfront_client))
    }

    /// Bytes are copied to the bucket, so the cache needs no unpacked copy of
    /// them: this reads each build's tarball instead (upload_build), which on
    /// a machine that builds an event's worth of challenges is a whole second
    /// copy of the corpus not written.
    const NEEDS_EXTRACTED_FILES: bool = false;

    async fn run(
        &self,
        cache_dir: &Path,
        namespaces: &HashSet<String>,
        tarballs: &HashMap<BuildId, PathBuf>,
        mut rx: Receiver<BuildEvent>,
    ) -> Result<(), anyhow::Error> {
        // Check that we have sufficient IAM permissions. Better to do this up-front than to
        // unexpectedly fail at runtime.
        info!("Checking IAM permissions");
        self.test_permissions().await?;

        // Sync existing artifacts
        info!("Syncing current artifact cache to S3");
        self.synchronize(cache_dir, namespaces, tarballs).await?;

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
                    BuildEvent::Create(build, tarball) => {
                        info!("Uploading artifacts for build {}", build);
                        if let Err(e) = self.upload_build(cache_dir, &build, &tarball).await {
                            processing_error = Some(e);
                            break;
                        }
                    }
                    BuildEvent::Update(build, tarball) => {
                        info!("Updating artifacts for build {}", build);
                        if let Err(e) = self.delete_bucket_dir(&build).await {
                            processing_error = Some(e);
                            break;
                        }
                        // S3 content changed after delete; capture upload result then
                        // record the build for invalidation regardless of upload outcome.
                        let upload_result = self.upload_build(cache_dir, &build, &tarball).await;
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
    /// Assembles a backend from settings and the clients it will use.
    fn from_parts(
        settings: S3Settings,
        s3_client: aws_sdk_s3::Client,
        cloudfront_client: Option<aws_sdk_cloudfront::Client>,
    ) -> Self {
        Self {
            bucket: settings.bucket,
            path_prefix: settings.path_prefix,
            prune_orphans: settings.prune_orphans,
            cloudfront_distribution: settings.cloudfront_distribution,
            s3_client,
            cloudfront_client,
            invalidation_counter: AtomicU64::new(0),
        }
    }

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
    /// Publishes a build's artifacts, reading them out of its tarball.
    ///
    /// Nothing is unpacked to disk. The tarball is read once and each entry
    /// spooled to a single reusable temporary file, so what this costs in
    /// local space is the largest file in the archive rather than the whole
    /// corpus -- which on a machine that builds an event's worth of
    /// challenges is the difference between a working disk and a full one.
    ///
    /// The spool is not avoidable: an object body has to be rewindable,
    /// because a signed request that is retried has to be signed again over
    /// the same bytes, and an entry inside a gzip stream can only be read
    /// forwards once.
    ///
    /// The checksum is written last, and separately, because it is not in
    /// the tarball: it is what synchronize compares against the cache to
    /// decide whether a build needs uploading at all, so a build whose
    /// objects are all up but whose checksum is not would be re-uploaded on
    /// every start.
    async fn upload_build(
        &self,
        cache_dir: &Path,
        build: &str,
        tarball: &Path,
    ) -> Result<(), anyhow::Error> {
        let mut spool_path = PathBuf::from(cache_dir);
        spool_path.push(build);
        spool_path.push(SPOOL_FILENAME);
        let spool = SpoolFile(spool_path.clone());

        let file = std::fs::File::open(tarball)?;
        let mut archive = Archive::new(GzDecoder::new(file));
        let mut published = 0usize;
        for entry in archive.entries()? {
            let mut entry = entry?;
            let is_file = entry.header().entry_type().is_file();
            let entry_path = entry.path()?.into_owned();
            let Some(member) = artifact_object_path(&entry_path, is_file) else {
                // A directory entry has never been published and is not worth
                // mentioning. Anything else is a file a player will not get:
                // a symlink or a hard link, which cannot be an object, or a
                // name this refuses. Said out loud, because the alternative
                // is a download that quietly 404s.
                if entry.header().entry_type().is_dir() {
                    debug!("Skipping directory entry {}", entry_path.display());
                } else {
                    warn!(
                        "Not publishing {} from {}: only regular files with plain relative \
                         names are published",
                        entry_path.display(),
                        tarball.display()
                    );
                }
                continue;
            };
            // One path, truncated and refilled per entry, rather than a new
            // temporary file for each of a build's files. Not fsynced: it is
            // read back by this process a line later, and a crash in between
            // costs a re-upload rather than a wrong object -- where an fsync
            // per artifact file would cost one on every file of every build.
            let mut sink = std::fs::File::create(&spool_path)?;
            std::io::copy(&mut entry, &mut sink)?;
            drop(sink);

            let key = format!("{}{}/{}", self.path_prefix, build, member);
            debug!("Uploading object: {key}");
            let body = ByteStream::read_from()
                .file(tokio::fs::File::open(&spool_path).await?)
                .build()
                .await?;
            self.s3_client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(body)
                .send()
                .await?;
            published += 1;
        }
        debug!("Published {published} object(s) for build {build}");
        drop(spool);

        // The checksum, from the cache rather than recomputed: it is what the
        // watcher wrote for this exact tarball.
        let mut checksum_path = PathBuf::from(cache_dir);
        checksum_path.push(build);
        checksum_path.push(CHECKSUM_FILENAME);
        let key = format!("{}{}/{}", self.path_prefix, build, CHECKSUM_FILENAME);
        let body = ByteStream::read_from()
            .file(tokio::fs::File::open(&checksum_path).await?)
            .build()
            .await?;
        self.s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(body)
            .send()
            .await?;
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
        tarballs: &HashMap<BuildId, PathBuf>,
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
            // The tarball this build was cached from, which is what its
            // objects are read out of. A cache directory with no tarball is
            // one sync_cache is about to remove -- it ran before this, so
            // this can only be a tarball deleted in between -- and there is
            // nothing to upload from.
            let Some(tarball) = tarballs.get(build_id) else {
                debug!("No tarball for cached build {build_id}, skipping");
                continue;
            };
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
                if let Err(e) = self.upload_build(cache_dir, build_id, tarball).await {
                    sync_error = Some(e);
                    break;
                }
            } else {
                info!(
                    "Artifacts for build {} not found in bucket, uploading",
                    build_id
                );
                if let Err(e) = self.upload_build(cache_dir, build_id, tarball).await {
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
    use crate::testing::{TempDir, write_tarball, write_tarball_of};
    use aws_sdk_s3::config::{Credentials, Region, StalledStreamProtectionConfig};
    use aws_sdk_s3::primitives::SdkBody;
    use aws_smithy_http_client::test_util::{ReplayEvent, StaticReplayClient};

    /// A backend wired to a fake transport rather than to AWS.
    ///
    /// Retries are off so that a request the fake has no answer for fails the
    /// test at once instead of being retried ten times with backoff, and path
    /// style addressing is on so that a request's URI reads as the object key
    /// it is rather than hiding the bucket in the hostname.
    fn backend(http: &StaticReplayClient, options: &[(&str, &str)]) -> S3Backend {
        let options: HashMap<String, String> = options
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new("ak", "sk", None, None, "test"))
            .force_path_style(true)
            .retry_config(RetryConfig::disabled())
            .stalled_stream_protection(StalledStreamProtectionConfig::disabled())
            .http_client(http.clone())
            .build();
        S3Backend::from_parts(
            S3Settings::from_options(&options).unwrap(),
            aws_sdk_s3::Client::from_conf(config),
            None,
        )
    }

    /// A canned successful response. The request half of a replay event is
    /// unused here: these tests read back what was actually asked
    /// (`requests`) rather than declaring it up front, since what is under
    /// test is which objects the backend decides to touch.
    fn responds(body: &str) -> ReplayEvent {
        ReplayEvent::new(
            http::Request::builder()
                .uri("https://unused.test/")
                .body(SdkBody::empty())
                .unwrap(),
            http::Response::builder()
                .status(200)
                .body(SdkBody::from(body))
                .unwrap(),
        )
    }

    /// A ListObjectsV2 response naming directories, as a delimited listing
    /// returns them.
    fn lists_directories(prefixes: &[&str]) -> ReplayEvent {
        let entries: String = prefixes
            .iter()
            .map(|p| format!("<CommonPrefixes><Prefix>{p}</Prefix></CommonPrefixes>"))
            .collect();
        responds(&format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Name>bucket</Name><IsTruncated>false</IsTruncated>{entries}</ListBucketResult>"
        ))
    }

    /// A ListObjectsV2 response naming objects.
    fn lists_objects(keys: &[&str]) -> ReplayEvent {
        let entries: String = keys
            .iter()
            .map(|k| format!("<Contents><Key>{k}</Key></Contents>"))
            .collect();
        responds(&format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Name>bucket</Name><IsTruncated>false</IsTruncated>{entries}</ListBucketResult>"
        ))
    }

    /// A DeleteObjects response.
    fn deleted() -> ReplayEvent {
        responds(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <DeleteResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></DeleteResult>",
        )
    }

    /// What was asked of S3, in order: the method and the path, which under
    /// path style addressing is the bucket followed by the object key. The
    /// host is dropped, and so is the `x-id` parameter the SDK tags requests
    /// with -- it names the operation, which the method already says, and
    /// says nothing about which object was touched.
    fn requests(http: &StaticReplayClient) -> Vec<String> {
        http.actual_requests()
            .map(|r| {
                let uri: http::Uri = r.uri().parse().expect("request URI");
                let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
                let path = path.split_once("?x-id=").map_or(path, |(head, _)| head);
                format!("{} {}", r.method(), path)
            })
            .collect()
    }

    /// Writes a build's cache directory as the watcher leaves it with
    /// extraction off: the directory, holding a checksum and nothing else.
    fn cached_build(cache_dir: &Path, build: &str, checksum: &[u8]) {
        let dir = cache_dir.join(build);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CHECKSUM_FILENAME), checksum).unwrap();
    }

    /// A build found in one of cork's namespaces is published under a
    /// matching prefix, each artifact at its own name, and the path prefix is
    /// in front of the lot. This is the arrangement that lets one build plane
    /// serve several orchestrators out of one bucket, and lets an event be
    /// retired by deleting a single prefix.
    #[tokio::test]
    async fn a_namespaced_build_is_published_under_its_namespace() {
        let root = TempDir::new("upload");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(
            &tarball,
            &[("BinEx101", b"binary"), ("nested/BinEx101.c", b"source")],
        );
        let cache_dir = root.path().join("cache");
        cached_build(&cache_dir, "library/7", b"sum");

        let http = StaticReplayClient::new(vec![responds(""), responds(""), responds("")]);
        let backend = backend(&http, &[("bucket", "arts"), ("path-prefix", "ctf")]);
        backend
            .upload_build(&cache_dir, "library/7", &tarball)
            .await
            .unwrap();

        assert_eq!(
            requests(&http),
            vec![
                "PUT /arts/ctf/library/7/BinEx101",
                "PUT /arts/ctf/library/7/nested/BinEx101.c",
                // Last, because it is what says the rest of them are up.
                "PUT /arts/ctf/library/7/.__checksum",
            ]
        );
    }

    /// A build in the artifact directory itself is published exactly where it
    /// always was. The `selfhosted` deployments this server was written for
    /// have no namespaces, and nothing about them may move.
    #[tokio::test]
    async fn a_plain_build_is_published_where_it_always_was() {
        let root = TempDir::new("upload-flat");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(&tarball, &[("BinEx101.c", b"source")]);
        let cache_dir = root.path().join("cache");
        cached_build(&cache_dir, "7", b"sum");

        let http = StaticReplayClient::new(vec![responds(""), responds("")]);
        let backend = backend(&http, &[("bucket", "arts")]);
        backend
            .upload_build(&cache_dir, "7", &tarball)
            .await
            .unwrap();

        assert_eq!(
            requests(&http),
            vec!["PUT /arts/7/BinEx101.c", "PUT /arts/7/.__checksum"]
        );
    }

    /// An entry that is not publishable reaches no object. Unpacking a
    /// tarball to disk had `Archive::unpack` to refuse what should not be
    /// written; streaming entries into object keys has only
    /// `artifact_object_path`, so what matters is that it is actually
    /// consulted on the way to the bucket and not merely unit-tested beside
    /// it.
    #[tokio::test]
    async fn an_unpublishable_entry_reaches_no_object() {
        let root = TempDir::new("refuse");
        let tarball = root.path().join("7.tar.gz");
        write_tarball_of(
            &tarball,
            &[
                (tar::EntryType::Directory, "nested/", b""),
                (tar::EntryType::Regular, "../escape.txt", b"nope"),
                (tar::EntryType::Regular, "nested/BinEx101.c", b"source"),
            ],
        );
        // The fixture is only evidence if the refused name really is in the
        // archive. A tar writer that quietly cleaned it -- which is what
        // `Header::set_path` does, and why this one does not use it -- would
        // leave this test passing while testing nothing.
        let names: Vec<String> =
            Archive::new(GzDecoder::new(std::fs::File::open(&tarball).unwrap()))
                .entries()
                .unwrap()
                .map(|e| e.unwrap().path().unwrap().display().to_string())
                .collect();
        assert!(
            names.iter().any(|n| n.contains("..")),
            "the fixture lost its traversing entry: {names:?}"
        );

        let cache_dir = root.path().join("cache");
        cached_build(&cache_dir, "7", b"sum");

        let http = StaticReplayClient::new(vec![responds(""), responds("")]);
        let backend = backend(&http, &[("bucket", "arts")]);
        backend
            .upload_build(&cache_dir, "7", &tarball)
            .await
            .unwrap();

        assert_eq!(
            requests(&http),
            vec!["PUT /arts/7/nested/BinEx101.c", "PUT /arts/7/.__checksum"]
        );
    }

    /// The spool file an upload streams entries through does not survive it.
    /// It sits in the build's own cache directory, and a leftover would be
    /// taken for an artifact by nothing but would hold the largest file of
    /// the build for as long as the cache lived.
    #[tokio::test]
    async fn the_spool_file_does_not_outlive_the_upload() {
        let root = TempDir::new("spool");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(&tarball, &[("BinEx101.c", b"source")]);
        let cache_dir = root.path().join("cache");
        cached_build(&cache_dir, "7", b"sum");

        let http = StaticReplayClient::new(vec![responds(""), responds("")]);
        let backend = backend(&http, &[("bucket", "arts")]);
        backend
            .upload_build(&cache_dir, "7", &tarball)
            .await
            .unwrap();

        assert_eq!(
            crate::testing::dir_entries(&cache_dir.join("7")),
            vec![CHECKSUM_FILENAME.to_string()]
        );
    }

    /// The bucket is listed once for the prefix itself and once under each
    /// namespace. A delimited listing only ever reports the level it is
    /// given, so a build two levels down -- which is every build a cork build
    /// plane produces -- is invisible to the first listing alone. Missing it
    /// would not be quiet: those builds would look absent from the bucket and
    /// be re-uploaded on every start.
    #[tokio::test]
    async fn the_bucket_is_listed_under_every_namespace() {
        let root = TempDir::new("listing");
        let cache_dir = root.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let http = StaticReplayClient::new(vec![
            lists_directories(&["7/", "library/"]),
            lists_directories(&["library/9/"]),
        ]);
        let backend = backend(&http, &[("bucket", "arts")]);
        backend
            .synchronize(
                &cache_dir,
                &HashSet::from(["library".to_string()]),
                &HashMap::new(),
            )
            .await
            .unwrap();

        let requests = requests(&http);
        assert_eq!(requests.len(), 2, "listings made: {requests:?}");
        assert!(
            requests[0].contains("delimiter=%2F"),
            "the first listing was not delimited: {}",
            requests[0]
        );
        assert!(
            requests[1].contains("prefix=library%2F"),
            "nothing listed the inside of the namespace: {}",
            requests[1]
        );
    }

    /// Nothing local means nothing is removed. The same two listings as
    /// above, and then no delete at all: a host with an empty artifact
    /// directory has not built yet rather than had every build deleted, and
    /// the bucket it is pointed at may be serving a running event.
    #[tokio::test]
    async fn a_host_with_nothing_local_deletes_nothing() {
        let root = TempDir::new("no-sweep");
        let cache_dir = root.path().join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let http = StaticReplayClient::new(vec![
            lists_directories(&["7/", "library/"]),
            lists_directories(&["library/9/"]),
        ]);
        let backend = backend(&http, &[("bucket", "arts")]);
        backend
            .synchronize(
                &cache_dir,
                &HashSet::from(["library".to_string()]),
                &HashMap::new(),
            )
            .await
            .unwrap();

        let requests = requests(&http);
        assert!(
            !requests.iter().any(|r| r.starts_with("POST")),
            "something was deleted from the bucket: {requests:?}"
        );
    }

    /// A build the bucket has and this host does not is removed, under the
    /// namespace it lives in. A build whose checksum already matches is left
    /// alone -- not re-uploaded, which for a bucket of any size is the
    /// difference between a start and an outage.
    #[tokio::test]
    async fn a_stale_namespaced_build_is_removed() {
        let root = TempDir::new("sweep");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(&tarball, &[("BinEx101.c", b"source")]);
        let cache_dir = root.path().join("cache");
        cached_build(&cache_dir, "7", b"same");

        let http = StaticReplayClient::new(vec![
            lists_directories(&["7/", "library/"]),
            lists_directories(&["library/9/"]),
            // The bucket's copy of build 7 is current, so it is skipped.
            responds("same"),
            // library/9 has no local artifact: list it, then delete it.
            lists_objects(&["library/9/BinEx101.c", "library/9/.__checksum"]),
            deleted(),
        ]);
        let backend = backend(&http, &[("bucket", "arts")]);
        backend
            .synchronize(
                &cache_dir,
                &HashSet::from(["library".to_string()]),
                &HashMap::from([("7".to_string(), tarball)]),
            )
            .await
            .unwrap();

        let requests = requests(&http);
        assert!(
            requests.iter().any(|r| r == "GET /arts/7/.__checksum"),
            "the bucket's checksum for build 7 was never read: {requests:?}"
        );
        assert!(
            !requests.iter().any(|r| r.starts_with("PUT")),
            "a build whose checksum already matched was re-uploaded: {requests:?}"
        );
        assert!(
            requests.iter().any(|r| r.contains("prefix=library%2F9%2F")),
            "the stale build was not listed under its namespace: {requests:?}"
        );
        assert!(
            requests.iter().any(|r| r.starts_with("POST /arts/?delete")),
            "the stale build was not deleted: {requests:?}"
        );
    }

    /// An ordinary entry, at the root of the archive and nested, keeps its
    /// path as the object key suffix.
    #[test]
    fn an_ordinary_entry_keeps_its_path() {
        assert_eq!(
            artifact_object_path(Path::new("BinEx101.c"), true).as_deref(),
            Some("BinEx101.c")
        );
        assert_eq!(
            artifact_object_path(Path::new("src/nested/file.bin"), true).as_deref(),
            Some("src/nested/file.bin")
        );
    }

    /// An absolute name is refused. This is the one that escapes: the key is
    /// built by pushing the entry onto the build's prefix, and pushing an
    /// absolute path throws away everything before it -- so the object would
    /// land at the bucket root, outside any prefix a retirement deletes.
    #[test]
    fn an_absolute_entry_is_refused() {
        for name in ["/etc/passwd", "/", "//tmp/x"] {
            assert_eq!(
                artifact_object_path(Path::new(name), true),
                None,
                "{name} was accepted as an object key"
            );
        }
    }

    /// A name with parent components is refused. It cannot traverse an S3
    /// key, but CloudFront normalizes `a/../b` to `b` before the request
    /// reaches the bucket, so the object would be published and permanently
    /// unreachable.
    #[test]
    fn a_traversing_entry_is_refused() {
        for name in ["../flag.txt", "a/../../b", "..", "a/.."] {
            assert_eq!(
                artifact_object_path(Path::new(name), true),
                None,
                "{name} was accepted as an object key"
            );
        }
    }

    /// A "./" prefix is dropped rather than refused, and that is not
    /// leniency: `tar czf x.tar.gz -C dir .` -- which is how a challenge's
    /// bundle is commonly made -- names every entry "./file". Refusing those
    /// would reject ordinary archives. Only "." alone has nothing left to
    /// publish once it is dropped.
    #[test]
    fn a_current_directory_prefix_is_dropped() {
        assert_eq!(
            artifact_object_path(Path::new("./BinEx101.c"), true).as_deref(),
            Some("BinEx101.c")
        );
        assert_eq!(
            artifact_object_path(Path::new("a/./b"), true).as_deref(),
            Some("a/b")
        );
        assert_eq!(artifact_object_path(Path::new("."), true), None);
    }

    /// Dropping "." does not extend to "..": a parent component is refused
    /// wherever it appears, including after a name that would seem to cancel
    /// it. Nothing here knows whether that name was a directory or a symlink
    /// to one, so nothing here gets to cancel anything.
    #[test]
    fn dropping_a_dot_does_not_drop_a_dotdot() {
        assert_eq!(artifact_object_path(Path::new("./../x"), true), None);
        assert_eq!(artifact_object_path(Path::new("a/./../x"), true), None);
    }

    /// Nothing but a regular file is published. A symlink or a device node
    /// has no meaning as an object, and the tar crate would have applied its
    /// own rules to those on extraction -- rules that do not exist here.
    #[test]
    fn only_regular_files_are_published() {
        assert_eq!(artifact_object_path(Path::new("link"), false), None);
        assert_eq!(artifact_object_path(Path::new("dir/"), false), None);
    }

    /// An empty name is refused rather than published at the build's own
    /// prefix, which is where an empty suffix would put it.
    #[test]
    fn an_empty_entry_is_refused() {
        assert_eq!(artifact_object_path(Path::new(""), true), None);
    }

    /// A backslash is an ordinary character in a POSIX filename and stays
    /// one: it is not a separator here, and rewriting it would invent a key
    /// the archive never named.
    #[test]
    fn a_backslash_is_an_ordinary_character() {
        assert_eq!(
            artifact_object_path(Path::new(r"weird\name.txt"), true).as_deref(),
            Some(r"weird\name.txt")
        );
    }
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
