use crate::{BuildId, get_cache_dir_checksum};

use super::{BuildEvent, CHECKSUM_FILENAME};
use blake2::{Blake2b512, Digest};
use flate2::read::GzDecoder;
use hex::ToHex;
use log::{debug, info, trace};
use notify_debouncer_full::Debouncer;
use notify_debouncer_full::notify::{self, EventKind, RecommendedWatcher};
use std::collections::HashMap;
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
fn extract_to(cache_dir: &Path, tarball: &Path) -> Result<(), std::io::Error> {
    maybe_remove_dir(cache_dir)?;
    fs::create_dir_all(cache_dir)?;
    let mut tarball_file = fs::File::open(tarball)?;
    tarball_file.rewind()?;
    let tar = GzDecoder::new(tarball_file);
    let mut archive = Archive::new(tar);
    archive.unpack(cache_dir)?;
    let mut checksum_path = PathBuf::from(cache_dir);
    checksum_path.push(CHECKSUM_FILENAME);
    fs::write(checksum_path, get_tarball_checksum(tarball)?)?;
    Ok(())
}

/// Converts a PathBuf to a filename string slice.
/// Panics if the conversion fails.
fn to_filename_str(path: &Path) -> &str {
    path.file_name()
        .unwrap_or_else(|| panic!("Failed to get filename for path {:?}", &path))
        .to_str()
        .unwrap_or_else(|| panic!("Failed to convert path {:?} to utf-8", &path))
}

/// Performs a full synchronization of the cache and artifact directories.
///
/// Any new or modified (based on a computed checksum) artifact tarballs will be extracted to the
/// cache. Any cache subdirectories no longer corresponding to an artifact tarball will be deleted.
pub(crate) fn sync_cache(
    artifact_dir: &Path,
    cache_dir: &Path,
    digest_salt: Option<&str>,
) -> Result<(), std::io::Error> {
    // Collect build IDs and paths of all existing artifact tarballs
    let mut tarballs: HashMap<String, PathBuf> = HashMap::new();
    for dir_entry in fs::read_dir(artifact_dir)? {
        let path_buf = dir_entry?.path();
        if let Some(build_id) = is_artifact_tarball(&path_buf, digest_salt) {
            tarballs.insert(build_id, path_buf);
        }
    }
    debug!("Found {} artifact tarballs", tarballs.len());

    // Collect build IDs and paths of all existing cache dirs
    let mut cache_dirs: HashMap<String, PathBuf> = HashMap::new();
    for dir_entry in fs::read_dir(cache_dir)? {
        let path_buf = dir_entry?.path();
        if path_buf.is_dir() {
            let dir_name = to_filename_str(&path_buf);
            cache_dirs.insert(dir_name.into(), path_buf);
        } else {
            // There shouldn't be any individual files in the cache directory
            debug!("Removing unrecognized cache file {}", path_buf.display());
            fs::remove_file(path_buf)?;
        }
    }
    debug!("Found {} cache directories", cache_dirs.len());

    // Ensure that the cache dir for each tarball is up to date
    for (build_id, tarball_path) in &tarballs {
        let mut reason = "missing";
        if let Some(cache_dir) = cache_dirs.get(build_id) {
            reason = "outdated";
            if get_tarball_checksum(tarball_path)? == get_cache_dir_checksum(cache_dir)? {
                continue;
            }
        }
        debug!("Cache for build {} is {}, recreating", build_id, reason);
        let mut build_cache_dir = PathBuf::from(cache_dir);
        build_cache_dir.push(build_id);
        extract_to(&build_cache_dir, tarball_path)?;
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
            watcher
                .watch(&artifact_dir, notify::RecursiveMode::NonRecursive)
                .expect("Failed to start file watcher");
            loop {
                match watcher_rx.recv() {
                    Ok(event_result) => match event_result {
                        Ok(events) => {
                            for event in events {
                                trace!("Detected file event: {:?}", event);
                                match event.kind {
                                    EventKind::Create(_) => {
                                        for path in &event.paths {
                                            if let Some(build_id) =
                                                is_artifact_tarball(path, digest_salt.as_deref())
                                            {
                                                info!(
                                                    "Creating artifact cache for build {}",
                                                    build_id
                                                );
                                                let mut cache_dir = PathBuf::from(&cache_dir);
                                                cache_dir.push(&build_id);
                                                extract_to(&cache_dir, path).unwrap_or_else(|_| {
                                                    panic!(
                                                        "Failed to extract artifact tarball {}",
                                                        path.display()
                                                    )
                                                });
                                                tx.blocking_send(BuildEvent::Create(build_id))
                                                    .expect("Failed to send build event");
                                            }
                                        }
                                    }
                                    EventKind::Modify(_) => {
                                        for path in &event.paths {
                                            if let Some(build_id) =
                                                is_artifact_tarball(path, digest_salt.as_deref())
                                            {
                                                info!(
                                                    "Updating artifact cache for build {}",
                                                    build_id
                                                );
                                                let mut cache_dir = PathBuf::from(&cache_dir);
                                                cache_dir.push(&build_id);
                                                extract_to(&cache_dir, path).unwrap_or_else(|_| {
                                                    panic!(
                                                        "Failed to extract artifact tarball {}",
                                                        path.display()
                                                    )
                                                });
                                                tx.blocking_send(BuildEvent::Update(build_id))
                                                    .expect("Failed to send build event");
                                            }
                                        }
                                    }
                                    EventKind::Remove(_) => {
                                        for path in &event.paths {
                                            if let Some(build_id) =
                                                is_artifact_tarball(path, digest_salt.as_deref())
                                            {
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

/// Determines whether a path is a cmgr artifact tarball. If so, returns the build ID.
fn is_artifact_tarball(path: &Path, digest_salt: Option<&str>) -> Option<BuildId> {
    let filename = to_filename_str(path);
    if !filename.ends_with(".tar.gz") {
        return None;
    }
    let build_id = filename.trim_end_matches(".tar.gz");
    let build_id = match digest_salt {
        Some(ref salt) => {
            let digest = sha2::Sha256::digest(format!("{build_id}:{salt}")).encode_hex();
            debug!("digested build ID {build_id} -> {digest}");
            digest
        }
        None => build_id.to_owned(),
    };
    Some(build_id)
}
