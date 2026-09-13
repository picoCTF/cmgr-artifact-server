use crate::{BuildId, get_cache_dir_checksum};

use super::{BuildEvent, CHECKSUM_FILENAME, NAMESPACE_MARKER_FILENAME};
use blake2::{Blake2b512, Digest};
use flate2::read::GzDecoder;
use hex::ToHex;
use log::{debug, info, trace};
use notify_debouncer_full::Debouncer;
use notify_debouncer_full::notify::{self, EventKind, RecommendedWatcher};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek};
use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use tar::Archive;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::channel;

/// Returns the checksum of an artifact tarball.
fn get_tarball_checksum(tarball: &Path) -> Result<Vec<u8>, std::io::Error> {
    let mut hasher = Blake2b512::new();
    let mut tarball = fs::File::open(tarball)?;
    let mut buf = [0; 4096];
    loop {
        // Avoid reading all of tarball into memory at once
        match tarball.read(&mut buf) {
            Ok(n @ 1..) => {
                hasher.update(&buf[..n]);
            }
            Ok(0) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(hasher.finalize().as_slice().into())
}

/// Attempts to remove a directory, suppressing a returned Error if the directory has already
/// been deleted.
fn maybe_remove_dir(path: &Path) -> Result<(), std::io::Error> {
    if let Err(e) = fs::remove_dir_all(path) {
        return match e.kind() {
            std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(e),
        };
    }
    Ok(())
}

/// Recreates the specified cache directory and extracts a tarball there.
/// Also writes the tarball's checksum to a file named .__checksum.
fn extract_to(cache_dir: &Path, tarball: &Path, extract: bool) -> Result<(), std::io::Error> {
    maybe_remove_dir(cache_dir)?;
    fs::create_dir_all(cache_dir)?;
    if extract {
        let mut tarball_file = fs::File::open(tarball)?;
        tarball_file.rewind()?;
        let tar = GzDecoder::new(tarball_file);
        let mut archive = Archive::new(tar);
        archive.unpack(cache_dir)?;
    }
    // Last, and always: it is what says the rest of this directory is current
    // for this tarball, so a crash before it leaves the build looking
    // uncached rather than cached and wrong. With extraction off it is the
    // only thing here, and the directory exists to hold it.
    let mut checksum_path = PathBuf::from(cache_dir);
    checksum_path.push(CHECKSUM_FILENAME);
    fs::write(checksum_path, get_tarball_checksum(tarball)?)?;
    Ok(())
}

/// Whether a build's cache directory holds anything besides the checksum --
/// which is to say, whether the tarball was unpacked into it.
///
/// Compared as an OsStr, not through to_filename_str: what is listed here is
/// artifact filenames, which come from whoever wrote the challenge rather
/// than from cmgr, and to_filename_str panics on a name that is not UTF-8.
/// Everywhere else it is applied to a build ID, a namespace an operator
/// named, or a tarball this server matched by extension.
fn holds_more_than_its_checksum(cache_dir: &Path) -> Result<bool, std::io::Error> {
    let checksum = std::ffi::OsStr::new(CHECKSUM_FILENAME);
    for dir_entry in fs::read_dir(cache_dir)? {
        if dir_entry?.file_name() != checksum {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Converts a PathBuf to a filename string slice.
/// Panics if the conversion fails.
fn to_filename_str(path: &Path) -> &str {
    path.file_name()
        .unwrap_or_else(|| panic!("Failed to get filename for path {:?}", path))
        .to_str()
        .unwrap_or_else(|| panic!("Failed to convert path {:?} to utf-8", path))
}

/// Performs a full synchronization of the cache and artifact directories.
///
/// Any new or modified (based on a computed checksum) artifact tarballs will be extracted to the
/// cache. Any cache subdirectories no longer corresponding to an artifact tarball will be deleted.
pub(crate) fn sync_cache(
    artifact_dir: &Path,
    cache_dir: &Path,
    digest_salt: Option<&str>,
    extract: bool,
) -> Result<(), std::io::Error> {
    // The per-destination directories cork sorts a build plane's bundles
    // into, which both listings below are keyed by.
    let namespaces = artifact_namespaces(artifact_dir)?;

    // Collect build keys and paths of all existing artifact tarballs
    let tarballs = artifact_tarballs_by_build(artifact_dir, &namespaces, digest_salt)?;
    debug!("Found {} artifact tarballs", tarballs.len());

    // Collect build keys and paths of all existing cache dirs
    let cache_dirs = cache_build_ids(cache_dir, &namespaces)?;
    debug!("Found {} cache directories", cache_dirs.len());

    // Ensure that the cache dir for each tarball is up to date
    for (build_id, tarball_path) in &tarballs {
        let mut reason = "missing";
        if let Some(cache_dir) = cache_dirs.get(build_id) {
            reason = "outdated";
            if get_tarball_checksum(tarball_path)? == get_cache_dir_checksum(cache_dir)? {
                // The checksum says which tarball this directory was made
                // from. It does not say the directory holds what this run
                // needs of it, and the two backends need different things --
                // so a host that has changed backends, or been upgraded or
                // downgraded across the version that made unpacking
                // optional, has a cache that is current and wrong. Nothing
                // else would ever revisit it, because the tarball has not
                // changed.
                //
                // Both directions matter, and differently. Left unpacked for
                // a backend that reads the tarball, it is a second copy of
                // every artifact kept for the life of the cache by a server
                // that never opens it. Left bare for one that serves files
                // off this directory, every build 404s.
                if holds_more_than_its_checksum(cache_dir)? != extract {
                    info!(
                        "Cache for build {} does not hold what this backend needs, recreating",
                        build_id
                    );
                    extract_to(cache_dir, tarball_path, extract)?;
                }
                continue;
            }
        }
        debug!("Cache for build {} is {}, recreating", build_id, reason);
        let mut build_cache_dir = PathBuf::from(cache_dir);
        build_cache_dir.push(build_id);
        extract_to(&build_cache_dir, tarball_path, extract)?;
    }

    // Remove any cache dirs without a matching tarball
    for (build_id, cache_dir) in &cache_dirs {
        if !tarballs.contains_key(build_id) {
            debug!("No tarball found for build {}, removing cache", build_id);
            maybe_remove_dir(cache_dir)?;
        }
    }
    Ok(())
}

/// Spawns a thread watching for changes to tarballs in the artifact directory.
///
/// If an artifact tarball is modified or deleted, its corresponding cache subdirectory is recreated
/// or deleted before sending a BuildEvent on the returned channel.
pub(crate) fn watch_dir(
    artifact_dir: &Path,
    cache_dir: &Path,
    digest_salt: Option<&str>,
    extract: bool,
) -> Receiver<BuildEvent> {
    let (tx, rx) = channel(32);
    thread::spawn({
        let artifact_dir = PathBuf::from(artifact_dir);
        let cache_dir = PathBuf::from(cache_dir);
        let digest_salt = digest_salt.map(|s| s.to_owned());
        move || {
            let (watcher_tx, watcher_rx) = std::sync::mpsc::channel();
            let notify_config =
                notify::Config::default().with_poll_interval(Duration::from_secs(2));
            let mut watcher: Debouncer<RecommendedWatcher, _> =
                notify_debouncer_full::new_debouncer_opt(
                    Duration::from_secs(2),
                    None,
                    watcher_tx,
                    notify_debouncer_full::RecommendedCache::new(),
                    notify_config,
                )
                .expect("Failed to create file watcher");
            // The artifact directory and each of cork's namespaces under it,
            // each on its own and none of them recursively. Not one recursive
            // watch of the whole tree: the extraction cache lives inside the
            // artifact directory, so a recursive watch would report every
            // file this process itself unpacks -- thousands of events per
            // event's worth of builds, all of them discarded.
            watcher
                .watch(&artifact_dir, notify::RecursiveMode::NonRecursive)
                .expect("Failed to start file watcher");
            for namespace in
                artifact_namespaces(&artifact_dir).expect("Failed to list artifact namespaces")
            {
                watcher
                    .watch(
                        artifact_dir.join(&namespace),
                        notify::RecursiveMode::NonRecursive,
                    )
                    .expect("Failed to watch artifact namespace");
            }
            loop {
                match watcher_rx.recv() {
                    Ok(event_result) => match event_result {
                        Ok(events) => {
                            for event in events {
                                trace!("Detected file event: {:?}", event);
                                match event.kind {
                                    EventKind::Create(_) => {
                                        for path in &event.paths {
                                            // A namespace cork made after this
                                            // started -- a destination built for
                                            // the first time -- needs a watch of
                                            // its own, since nothing here watches
                                            // recursively.
                                            if path.is_dir()
                                                && path.parent() == Some(artifact_dir.as_path())
                                                && is_namespace_dir(path)
                                            {
                                                info!(
                                                    "Watching new artifact namespace {}",
                                                    path.display()
                                                );
                                                watcher
                                                    .watch(
                                                        path,
                                                        notify::RecursiveMode::NonRecursive,
                                                    )
                                                    .unwrap_or_else(|e| {
                                                        panic!(
                                                            "Failed to watch artifact namespace {}: {e}",
                                                            path.display()
                                                        )
                                                    });
                                                // Anything cork wrote into it
                                                // before the watch existed.
                                                if let Err(e) = resync_namespace(
                                                    &artifact_dir,
                                                    &cache_dir,
                                                    path,
                                                    digest_salt.as_deref(),
                                                    extract,
                                                    &tx,
                                                ) {
                                                    panic!(
                                                        "Failed to synchronize new artifact namespace {}: {e}",
                                                        path.display()
                                                    );
                                                }
                                                continue;
                                            }
                                            if let Some(build_id) = is_artifact_tarball(
                                                path,
                                                &artifact_dir,
                                                digest_salt.as_deref(),
                                            ) {
                                                info!(
                                                    "Creating artifact cache for build {}",
                                                    build_id
                                                );
                                                let mut cache_dir = PathBuf::from(&cache_dir);
                                                cache_dir.push(&build_id);
                                                extract_to(&cache_dir, path, extract)
                                                    .unwrap_or_else(|_| {
                                                        panic!(
                                                            "Failed to extract artifact tarball {}",
                                                            path.display()
                                                        )
                                                    });
                                                tx.blocking_send(BuildEvent::Create(
                                                    build_id,
                                                    path.clone(),
                                                ))
                                                .expect("Failed to send build event");
                                            }
                                        }
                                    }
                                    EventKind::Modify(_) => {
                                        for path in &event.paths {
                                            if let Some(build_id) = is_artifact_tarball(
                                                path,
                                                &artifact_dir,
                                                digest_salt.as_deref(),
                                            ) {
                                                info!(
                                                    "Updating artifact cache for build {}",
                                                    build_id
                                                );
                                                let mut cache_dir = PathBuf::from(&cache_dir);
                                                cache_dir.push(&build_id);
                                                extract_to(&cache_dir, path, extract)
                                                    .unwrap_or_else(|_| {
                                                        panic!(
                                                            "Failed to extract artifact tarball {}",
                                                            path.display()
                                                        )
                                                    });
                                                tx.blocking_send(BuildEvent::Update(
                                                    build_id,
                                                    path.clone(),
                                                ))
                                                .expect("Failed to send build event");
                                            }
                                        }
                                    }
                                    EventKind::Remove(_) => {
                                        for path in &event.paths {
                                            if let Some(build_id) = is_artifact_tarball(
                                                path,
                                                &artifact_dir,
                                                digest_salt.as_deref(),
                                            ) {
                                                info!(
                                                    "Deleting artifact cache for build {}",
                                                    build_id
                                                );
                                                let mut cache_dir = PathBuf::from(&cache_dir);
                                                cache_dir.push(&build_id);
                                                maybe_remove_dir(&cache_dir).unwrap_or_else(|_| {
                                                    panic!(
                                                        "Failed to remove cache directory {}",
                                                        cache_dir.display()
                                                    )
                                                });
                                                tx.blocking_send(BuildEvent::Delete(build_id))
                                                    .expect("Failed to send build event");
                                            }
                                        }
                                    }
                                    _ => (),
                                }
                            }
                        }
                        Err(errors) => panic!("file watcher errors: {errors:?}"),
                    },
                    Err(e) => panic!("watcher channel receive error: {e:?}"),
                }
            }
        }
    });
    rx
}

/// Determines whether a path is a cmgr artifact tarball. If so, returns its
/// build key: the build ID (digested, if a salt is in use) prefixed with the
/// namespace directory it was found in, if any.
///
/// cork sorts a build plane's bundles into a directory per destination, so
/// that one artifact server can publish several orchestrators' artifacts,
/// each under its own prefix. A tarball in the artifact directory itself --
/// a single-host deployment, or a schema with no destination -- keeps a bare
/// build ID as its key, exactly as before.
fn is_artifact_tarball(
    path: &Path,
    artifact_dir: &Path,
    digest_salt: Option<&str>,
) -> Option<BuildId> {
    let filename = to_filename_str(path);
    if !filename.ends_with(".tar.gz") {
        return None;
    }
    // Where it sits relative to the artifact directory. Anything else -- a
    // deeper path, or a subdirectory cork did not mark -- is not ours: the
    // extraction cache lives under the artifact directory, and so does the
    // challenge tree in cmgr's own ansible defaults.
    let parent = path.parent()?;
    let namespace = if parent == artifact_dir {
        None
    } else if parent.parent() == Some(artifact_dir) && is_namespace_dir(parent) {
        Some(to_filename_str(parent).to_owned())
    } else {
        return None;
    };
    let build_id = filename.trim_end_matches(".tar.gz");
    let build_id = match digest_salt {
        Some(ref salt) => {
            let digest =
                <sha2::Sha256 as sha2::Digest>::digest(format!("{build_id}:{salt}")).encode_hex();
            debug!("digested build ID {build_id} -> {digest}");
            digest
        }
        None => build_id.to_owned(),
    };
    Some(match namespace {
        Some(namespace) => format!("{namespace}/{build_id}"),
        None => build_id,
    })
}

/// Whether a directory is one of cork's per-destination artifact
/// directories. cork writes an empty marker file into each one it makes, so
/// that a subdirectory of CMGR_ARTIFACT_DIR which is something else is never
/// taken for one -- the extraction cache is such a subdirectory, and so is
/// every challenge when the artifact directory and the challenge tree are the
/// same path, which is cmgr's ansible role's default.
fn is_namespace_dir(path: &Path) -> bool {
    path.join(NAMESPACE_MARKER_FILENAME).is_file()
}

/// Extracts and announces every tarball already in a namespace directory.
///
/// A namespace is watched from the moment it appears, but cork creates the
/// directory and writes into it, so the tarballs of the first build may land
/// between the two. Nothing else would notice them until a restart.
fn resync_namespace(
    artifact_dir: &Path,
    cache_dir: &Path,
    namespace_dir: &Path,
    digest_salt: Option<&str>,
    extract: bool,
    tx: &tokio::sync::mpsc::Sender<BuildEvent>,
) -> Result<(), std::io::Error> {
    for dir_entry in fs::read_dir(namespace_dir)? {
        let path_buf = dir_entry?.path();
        let Some(build_id) = is_artifact_tarball(&path_buf, artifact_dir, digest_salt) else {
            continue;
        };
        let mut build_cache_dir = PathBuf::from(cache_dir);
        build_cache_dir.push(&build_id);
        // Only what is not already cached from this same tarball: a create
        // event for the file itself may well arrive too, and extracting twice
        // would republish it for nothing.
        if let Ok(cached) = get_cache_dir_checksum(&build_cache_dir)
            && cached == get_tarball_checksum(&path_buf)?
        {
            continue;
        }
        info!("Creating artifact cache for build {}", build_id);
        extract_to(&build_cache_dir, &path_buf, extract)?;
        tx.blocking_send(BuildEvent::Create(build_id, path_buf))
            .expect("Failed to send build event");
    }
    Ok(())
}

/// The namespaces cork has made under the artifact directory, by name.
pub(crate) fn artifact_namespaces(artifact_dir: &Path) -> Result<HashSet<String>, std::io::Error> {
    let mut namespaces = HashSet::new();
    for dir_entry in fs::read_dir(artifact_dir)? {
        let path_buf = dir_entry?.path();
        if path_buf.is_dir() && is_namespace_dir(&path_buf) {
            debug!("Found artifact namespace {}", path_buf.display());
            namespaces.insert(to_filename_str(&path_buf).to_owned());
        }
    }
    Ok(namespaces)
}

/// Every artifact tarball under the artifact directory, by build key: the
/// directory itself, and one level down into each namespace cork marked.
pub(crate) fn artifact_tarballs_by_build(
    artifact_dir: &Path,
    namespaces: &HashSet<String>,
    digest_salt: Option<&str>,
) -> Result<HashMap<BuildId, PathBuf>, std::io::Error> {
    let mut search = vec![PathBuf::from(artifact_dir)];
    search.extend(namespaces.iter().map(|name| artifact_dir.join(name)));
    let mut tarballs: HashMap<BuildId, PathBuf> = HashMap::new();
    for dir in search {
        for dir_entry in fs::read_dir(&dir)? {
            let path_buf = dir_entry?.path();
            if let Some(build_id) = is_artifact_tarball(&path_buf, artifact_dir, digest_salt) {
                tarballs.insert(build_id, path_buf);
            }
        }
    }
    Ok(tarballs)
}

/// Whether a cache directory is a build's.
///
/// It is one when it holds the checksum that says which tarball it was made
/// from, and only then. Three things turn up in the cache that are not
/// builds: a namespace, which holds builds rather than being one; a directory
/// a crash left between unpacking and writing the checksum; and a namespace
/// whose marker has gone from the artifact directory. Counting any of them as
/// a build publishes it whole, under its own name, and -- because the caller
/// goes straight on to read the checksum that is not there -- fails the
/// startup synchronization outright.
fn is_build_cache_dir(path: &Path) -> bool {
    if path.join(CHECKSUM_FILENAME).is_file() {
        return true;
    }
    debug!(
        "Ignoring cache directory {} with no checksum file",
        path.display()
    );
    false
}

/// Every build with a cache directory, keyed as the tarballs are.
///
/// A subdirectory is a namespace only when the artifact directory says it is,
/// never when it merely looks like one, and what is inside either is a build
/// only when is_build_cache_dir says so.
pub(crate) fn cache_build_ids(
    cache_dir: &Path,
    namespaces: &HashSet<String>,
) -> Result<HashMap<BuildId, PathBuf>, std::io::Error> {
    let mut cache_dirs: HashMap<BuildId, PathBuf> = HashMap::new();
    for dir_entry in fs::read_dir(cache_dir)? {
        let path_buf = dir_entry?.path();
        if !path_buf.is_dir() {
            // There shouldn't be any individual files in the cache directory
            debug!("Removing unrecognized cache file {}", path_buf.display());
            fs::remove_file(path_buf)?;
            continue;
        }
        let dir_name = to_filename_str(&path_buf).to_owned();
        if !namespaces.contains(&dir_name) {
            if is_build_cache_dir(&path_buf) {
                cache_dirs.insert(dir_name, path_buf);
            }
            continue;
        }
        for dir_entry in fs::read_dir(&path_buf)? {
            let build_cache_dir = dir_entry?.path();
            if build_cache_dir.is_dir() && is_build_cache_dir(&build_cache_dir) {
                let build = to_filename_str(&build_cache_dir);
                cache_dirs.insert(format!("{dir_name}/{build}"), build_cache_dir);
            }
        }
    }
    Ok(cache_dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TempDir, dir_entries, namespace_dir, touch, write_tarball};

    /// With extraction off the cache holds one checksum per build and nothing
    /// else. That is the whole point of it: unpacking every tarball on the
    /// machine that builds them is a second copy of the entire corpus, and a
    /// backend that copies the bytes elsewhere never reads that copy.
    #[test]
    fn extraction_can_be_skipped() {
        let root = TempDir::new("noextract");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(
            &tarball,
            &[("BinEx101", b"binary"), ("BinEx101.c", b"source")],
        );
        let cache = root.path().join("cache").join("7");

        extract_to(&cache, &tarball, false).unwrap();
        assert_eq!(dir_entries(&cache), vec![CHECKSUM_FILENAME.to_string()]);
    }

    /// With extraction on the files are there, because the selfhosted backend
    /// serves them straight off this directory.
    #[test]
    fn extraction_writes_the_files() {
        let root = TempDir::new("extract");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(
            &tarball,
            &[("BinEx101", b"binary"), ("BinEx101.c", b"source")],
        );
        let cache = root.path().join("cache").join("7");

        extract_to(&cache, &tarball, true).unwrap();
        assert_eq!(
            dir_entries(&cache),
            vec![
                CHECKSUM_FILENAME.to_string(),
                "BinEx101".to_string(),
                "BinEx101.c".to_string()
            ]
        );
        assert_eq!(fs::read(cache.join("BinEx101.c")).unwrap(), b"source");
    }

    /// The checksum is of the tarball, so it does not depend on whether the
    /// tarball was unpacked. It has to not: it is what the bucket is compared
    /// against to decide whether a build needs uploading, and turning
    /// extraction off must not make every build look changed.
    #[test]
    fn the_checksum_does_not_depend_on_extraction() {
        let root = TempDir::new("checksum");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(&tarball, &[("BinEx101.c", b"source")]);

        let unpacked = root.path().join("cache-a").join("7");
        let bare = root.path().join("cache-b").join("7");
        extract_to(&unpacked, &tarball, true).unwrap();
        extract_to(&bare, &tarball, false).unwrap();

        assert_eq!(
            get_cache_dir_checksum(&unpacked).unwrap(),
            get_cache_dir_checksum(&bare).unwrap()
        );
    }

    /// Re-running over a directory that was extracted leaves only the
    /// checksum. A host that switches backends, or upgrades into one that no
    /// longer unpacks, would otherwise keep serving a stale unpacked copy
    /// and keep paying for it.
    #[test]
    fn skipping_extraction_clears_a_previously_extracted_cache() {
        let root = TempDir::new("switch");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(&tarball, &[("BinEx101.c", b"source")]);
        let cache = root.path().join("cache").join("7");

        extract_to(&cache, &tarball, true).unwrap();
        assert!(cache.join("BinEx101.c").is_file());

        extract_to(&cache, &tarball, false).unwrap();
        assert_eq!(dir_entries(&cache), vec![CHECKSUM_FILENAME.to_string()]);
    }

    /// A tarball in the artifact directory itself keeps a bare build ID, and
    /// one in a namespace is keyed under it. That key is the cache
    /// directory's name and the path the files are published under, so it is
    /// the whole of what makes one build plane's artifacts reach several
    /// orchestrators' players under their own prefixes.
    #[test]
    fn build_key_carries_the_namespace() {
        let root = TempDir::new("key");
        let flat = root.path().join("7.tar.gz");
        touch(&flat);
        let namespaced = namespace_dir(root.path(), "library").join("7.tar.gz");
        touch(&namespaced);

        assert_eq!(
            is_artifact_tarball(&flat, root.path(), None),
            Some("7".to_string())
        );
        assert_eq!(
            is_artifact_tarball(&namespaced, root.path(), None),
            Some("library/7".to_string())
        );
    }

    /// With a salt it is the digest that is namespaced, not the other way
    /// round: the namespace is a directory an operator named, and the digest
    /// is what keeps builds of one challenge from being enumerated.
    #[test]
    fn build_key_digests_under_the_namespace() {
        let root = TempDir::new("salt");
        let namespaced = namespace_dir(root.path(), "library").join("7.tar.gz");
        touch(&namespaced);

        let key = is_artifact_tarball(&namespaced, root.path(), Some("pepper")).unwrap();
        let (namespace, digest) = key.split_once('/').expect("key carries its namespace");
        assert_eq!(namespace, "library");
        assert_eq!(digest.len(), 64, "a sha-256 hex digest");
        assert_ne!(digest, "7");
    }

    /// A subdirectory cork did not mark holds no artifacts of ours, whatever
    /// is in it. The extraction cache is such a subdirectory; so is every
    /// challenge where the artifact directory and the challenge tree are the
    /// same path -- cmgr's own ansible role's default -- and there a
    /// challenge shipping a numbered archive would otherwise be published as
    /// somebody's build.
    #[test]
    fn an_unmarked_subdirectory_holds_no_artifacts() {
        let root = TempDir::new("unmarked");
        let challenge = root.path().join("binex101").join("7.tar.gz");
        touch(&challenge);
        let cache = root.path().join(".artifact_server_cache").join("7.tar.gz");
        touch(&cache);

        assert_eq!(is_artifact_tarball(&challenge, root.path(), None), None);
        assert_eq!(is_artifact_tarball(&cache, root.path(), None), None);
    }

    /// Two levels down is not a namespace either, marked or not: cork writes
    /// one directory per destination and nothing deeper, and the cache's own
    /// build directories are two levels down.
    #[test]
    fn a_deeper_path_holds_no_artifacts() {
        let root = TempDir::new("deep");
        let deep = namespace_dir(root.path(), "library")
            .join("deeper")
            .join("7.tar.gz");
        touch(&deep);
        assert_eq!(is_artifact_tarball(&deep, root.path(), None), None);
    }

    /// Anything that is not a tarball is not one.
    #[test]
    fn only_tarballs_are_artifacts() {
        let root = TempDir::new("not-tar");
        for name in ["7.tar", "7.gz", "notes.txt", NAMESPACE_MARKER_FILENAME] {
            let path = root.path().join(name);
            touch(&path);
            assert_eq!(
                is_artifact_tarball(&path, root.path(), None),
                None,
                "{name} was taken for an artifact tarball"
            );
        }
    }

    /// Only the directories cork marked are namespaces.
    #[test]
    fn namespaces_are_the_marked_directories() {
        let root = TempDir::new("namespaces");
        namespace_dir(root.path(), "library");
        namespace_dir(root.path(), "event");
        fs::create_dir_all(root.path().join("binex101")).unwrap();
        fs::create_dir_all(root.path().join(".artifact_server_cache")).unwrap();
        touch(&root.path().join("7.tar.gz"));

        let found = artifact_namespaces(root.path()).unwrap();
        assert_eq!(
            found,
            HashSet::from(["library".to_string(), "event".to_string()])
        );
    }

    /// Every tarball, in the artifact directory and one level down into each
    /// namespace, and nothing from anywhere else.
    #[test]
    fn tarballs_are_collected_from_every_namespace() {
        let root = TempDir::new("collect");
        touch(&root.path().join("1.tar.gz"));
        touch(&namespace_dir(root.path(), "library").join("2.tar.gz"));
        touch(&namespace_dir(root.path(), "event").join("3.tar.gz"));
        touch(&root.path().join("binex101").join("4.tar.gz"));

        let namespaces = artifact_namespaces(root.path()).unwrap();
        let tarballs = artifact_tarballs_by_build(root.path(), &namespaces, None).unwrap();
        let mut keys: Vec<_> = tarballs.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["1", "event/3", "library/2"]);
    }

    /// The cache is keyed the way the tarballs are, and a subdirectory is
    /// descended into only when the artifact directory says it is a namespace
    /// -- never because it looks like one.
    #[test]
    fn cache_keys_follow_the_namespaces() {
        let root = TempDir::new("cache");
        let cache = root.path().join(".artifact_server_cache");
        touch(&cache.join("1").join(CHECKSUM_FILENAME));
        touch(&cache.join("library").join("2").join(CHECKSUM_FILENAME));

        let namespaces = HashSet::from(["library".to_string()]);
        let found = cache_build_ids(&cache, &namespaces).unwrap();
        let mut keys: Vec<_> = found.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["1", "library/2"]);
    }

    /// A directory holding no checksum file is not a build. Two things land
    /// there: an extraction interrupted between unpacking and writing the
    /// checksum, which is re-extracted rather than read as a build whose
    /// every subdirectory is one; and a namespace whose marker has gone,
    /// which would otherwise be published whole, every build inside it, under
    /// the namespace's own name.
    #[test]
    fn a_cache_directory_without_a_checksum_is_not_a_build() {
        let root = TempDir::new("halfcache");
        let cache = root.path().join(".artifact_server_cache");
        touch(&cache.join("3").join("subdir").join("payload"));
        touch(&cache.join("unmarked").join("4").join(CHECKSUM_FILENAME));

        let found = cache_build_ids(&cache, &HashSet::new()).unwrap();
        assert!(
            found.is_empty(),
            "took a directory with no checksum for a build: {found:?}"
        );
    }

    /// And the same inside a namespace, which is where it is worst: the
    /// caller reads the checksum of everything this returns, so a directory
    /// an interrupted extraction left behind does not merely get published
    /// wrongly -- it fails the startup synchronization, on this start and
    /// every later one, until somebody deletes it by hand.
    #[test]
    fn a_namespaced_cache_directory_without_a_checksum_is_not_a_build() {
        let root = TempDir::new("halfcache-ns");
        let cache = root.path().join(".artifact_server_cache");
        touch(&cache.join("library").join("7").join(CHECKSUM_FILENAME));
        touch(&cache.join("library").join("8").join("BinEx101.c"));

        let namespaces = HashSet::from(["library".to_string()]);
        let found = cache_build_ids(&cache, &namespaces).unwrap();
        let mut keys: Vec<_> = found.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["library/7"]);

        // What the caller does with them, which is what makes it fatal.
        for cache_dir in found.values() {
            get_cache_dir_checksum(cache_dir).expect("a build with no readable checksum");
        }
    }

    /// Upgrading a host in place reclaims its disk. The tarball has not
    /// changed, so the build is current and nothing else would ever look at
    /// its directory again -- and a backend that no longer serves files from
    /// there would keep a copy of every artifact for the life of the cache.
    /// Reclaiming that disk is the whole of why extraction became optional,
    /// and the release notes promise it.
    #[test]
    fn an_unpacked_cache_is_cleared_when_extraction_is_off() {
        let root = TempDir::new("upgrade");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(&tarball, &[("BinEx101.c", b"source")]);
        let cache = root.path().join(".artifact_server_cache");
        fs::create_dir_all(&cache).unwrap();

        // As a version that unpacked for this backend left it.
        extract_to(&cache.join("7"), &tarball, true).unwrap();
        assert!(cache.join("7").join("BinEx101.c").is_file());

        sync_cache(root.path(), &cache, None, false).unwrap();
        assert_eq!(
            dir_entries(&cache.join("7")),
            vec![CHECKSUM_FILENAME.to_string()],
            "the unpacked copy survived the upgrade"
        );
    }

    /// The same sync leaves an unpacked cache alone when the backend serves
    /// files off it. Clearing there would empty the directory the selfhosted
    /// backend answers requests from.
    #[test]
    fn an_unpacked_cache_is_kept_when_extraction_is_on() {
        let root = TempDir::new("no-upgrade");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(&tarball, &[("BinEx101.c", b"source")]);
        let cache = root.path().join(".artifact_server_cache");
        fs::create_dir_all(&cache).unwrap();
        extract_to(&cache.join("7"), &tarball, true).unwrap();

        sync_cache(root.path(), &cache, None, true).unwrap();
        assert!(cache.join("7").join("BinEx101.c").is_file());
    }

    /// And the other direction, which is the one that serves 404s: a cache
    /// left bare by a backend that reads tarballs is unpacked again for one
    /// that serves files off disk. The checksum matches either way -- it
    /// says which tarball the directory came from, not whether it holds what
    /// is about to be asked of it -- so nothing else would ever revisit it.
    /// This is the path a host takes when it changes backends, and the one a
    /// downgrade takes across the version that made unpacking optional.
    #[test]
    fn a_bare_cache_is_unpacked_when_extraction_is_on() {
        let root = TempDir::new("downgrade");
        let tarball = root.path().join("7.tar.gz");
        write_tarball(&tarball, &[("BinEx101.c", b"source")]);
        let cache = root.path().join(".artifact_server_cache");
        fs::create_dir_all(&cache).unwrap();

        // As a backend that publishes from the tarball left it.
        extract_to(&cache.join("7"), &tarball, false).unwrap();
        assert_eq!(
            dir_entries(&cache.join("7")),
            vec![CHECKSUM_FILENAME.to_string()]
        );

        sync_cache(root.path(), &cache, None, true).unwrap();
        assert_eq!(
            fs::read(cache.join("7").join("BinEx101.c")).unwrap(),
            b"source",
            "every build would have 404'd"
        );
    }

    /// An artifact whose name is not UTF-8 does not bring the server down.
    /// Artifact names come from whoever wrote the challenge, and the check
    /// for an unpacked cache reads all of them -- on exactly the upgrade
    /// path it exists for, and only for whichever name read_dir happens to
    /// return first.
    #[cfg(unix)]
    #[test]
    fn an_artifact_name_that_is_not_utf8_is_survivable() {
        use std::os::unix::ffi::OsStrExt;

        let root = TempDir::new("notutf8");
        let cache = root.path().join("7");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join(CHECKSUM_FILENAME), b"sum").unwrap();
        fs::write(
            cache.join(std::ffi::OsStr::from_bytes(b"BinEx\xff101.c")),
            b"source",
        )
        .unwrap();

        assert!(holds_more_than_its_checksum(&cache).unwrap());
    }
}
