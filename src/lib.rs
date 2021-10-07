mod s3;
mod selfhosted;

use async_trait::async_trait;
use blake2::{Blake2b, Digest};
use flate2::read::GzDecoder;
use log::{debug, info, trace};
use notify::{DebouncedEvent, RecommendedWatcher, Watcher};
pub use s3::S3;
pub use selfhosted::Selfhosted;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use tar::Archive;
use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::Receiver;

#[derive(Debug)]
pub struct OptionParsingError;

impl Error for OptionParsingError {}

impl Display for OptionParsingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Encountered an invalid option. Backend options must be specified in key=value format."
        )
    }
}

#[derive(Debug)]
pub struct BackendCreationError;

impl Error for BackendCreationError {}

impl Display for BackendCreationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Unable to initialize backend. Some required options were not provided."
        )
    }
}

/// Represents detected changes to artifact tarballs.
/// The included string is the build ID.
#[derive(Debug)]
pub enum BuildEvent {
    Update(String),
    Delete(String),
}

#[async_trait]
pub trait Backend: Sized {
    // TODO: currently the get_options() methods are not actually called anywhere. It would be nice
    // if they were used in the CLI help output or BackendCreationErrors.

    /// Return a list of option keys supported by this backend.
    fn get_options() -> &'static [&'static str];

    /// Return a list of option keys required by this backend.
    fn get_required_options() -> &'static [&'static str];

    /// Create an instance of the backend if all required options are provided.
    fn new(options: HashMap<&str, &str>) -> Result<Self, BackendCreationError>;

    /// Run the backend.
    ///
    /// The backend is not provided with the artifact directory (i.e. CMGR_ARTIFACT_DIR) itself, but
    /// rather with a cache directory containing extracted artifact tarballs as subdirectories named
    /// with the associated build ID. This cache directory is automatically kept up to date by a
    /// background thread when the server is run as a binary.
    ///
    /// When a backend runs, it should first perform any synchronization necessary in order to
    /// reflect the current contents of the cache directory. For example, if the backend syncs files
    /// to remote storage, any directories without matching .__checksum files should be re-uploaded,
    /// and any remote directories which no longer exist in the cache should be removed.
    ///
    /// After completing this initial synchronization, the backend should listen on the provided
    /// channel for build events and take action accordingly. These events are produced when a build
    /// with artifacts is (re-)created (BuildEvent::Update) or deleted (BuildEvent::Delete), and
    /// contain the ID of the build. For example, a build's artifacts might be re-uploaded when an
    /// Update event occurs, or deleted from remote storage when a Delete event occurs.
    ///
    /// As there is the potential for race conditions when handling build events, backends must
    /// process any events with the same build ID serially in the order of their arrival.
    async fn run(
        &self,
        cache_dir: &Path,
        mut rx: Receiver<BuildEvent>,
    ) -> Result<(), Box<dyn std::error::Error>>;
}

/// Returns the checksum of an artifact tarball.
fn get_tarball_checksum(tarball: &Path) -> Result<Vec<u8>, std::io::Error> {
    let mut hasher = Blake2b::new();
    let mut tarball = fs::File::open(tarball)?;
    let mut buf = [0; 4096];
    loop {
        // Avoid reading all of tarball into memory at once
        match tarball.read(&mut buf) {
            Ok(n) if n > 0 => {
                hasher.update(&buf[..n]);
            }
            Ok(_) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(hasher.finalize().as_slice().into())
}

pub const CHECKSUM_FILENAME: &str = ".__checksum";

/// Returns the tarball checksum stored inside a cache directory.
fn get_cache_dir_checksum(cache_dir: &Path) -> Result<Vec<u8>, std::io::Error> {
    let mut checksum_path = PathBuf::from(cache_dir);
    checksum_path.push(CHECKSUM_FILENAME);
    fs::read(checksum_path)
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

/// Recreates the specified chache directory and extracts a tarball there.
/// Also writes the tarball's checksum to a file named .__checksum.
fn extract_to(cache_dir: &Path, tarball: &Path) -> Result<(), std::io::Error> {
    maybe_remove_dir(cache_dir)?;
    fs::create_dir_all(cache_dir)?;
    let mut tarball_file = fs::File::open(tarball)?;
    tarball_file.seek(SeekFrom::Start(0))?;
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
pub fn sync_cache(artifact_dir: &Path, cache_dir: &Path) -> Result<(), std::io::Error> {
    // Collect build IDs and paths of all existing artifact tarballs
    let mut tarballs: HashMap<String, PathBuf> = HashMap::new();
    for dir_entry in fs::read_dir(&artifact_dir)? {
        let path_buf = dir_entry?.path();
        let filename = to_filename_str(&path_buf);
        if filename.ends_with(".tar.gz") {
            let build_id = filename.trim_end_matches(".tar.gz");
            tarballs.insert(build_id.into(), path_buf);
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
    for (build_id, tarball) in &tarballs {
        let mut reason = "missing";
        if let Some(cache_dir) = cache_dirs.get(build_id) {
            reason = "outdated";
            if get_tarball_checksum(tarball)? == get_cache_dir_checksum(cache_dir)? {
                continue;
            }
        }
        debug!("Cache for build {} is {}, recreating", build_id, reason);
        let mut build_cache_dir = PathBuf::from(cache_dir);
        build_cache_dir.push(build_id);
        extract_to(&build_cache_dir, tarball)?;
    }

    // Remove any cache dirs without a matching tarball
    for (build_id, cache_dir) in &cache_dirs {
        if tarballs.get(build_id).is_none() {
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
pub fn watch_dir(artifact_dir: &Path, cache_dir: &Path) -> Receiver<BuildEvent> {
    let (tx, rx) = channel(32);
    thread::spawn({
        let artifact_dir = PathBuf::from(artifact_dir);
        let cache_dir = PathBuf::from(cache_dir);
        move || {
            let (watcher_tx, watcher_rx) = std::sync::mpsc::channel();
            let mut watcher: RecommendedWatcher = Watcher::new(watcher_tx, Duration::from_secs(2))
                .expect("Failed to create file watcher");
            watcher
                .watch(&artifact_dir, notify::RecursiveMode::NonRecursive)
                .expect("Failed to start file watcher");
            loop {
                match watcher_rx.recv() {
                    Ok(event) => {
                        trace!("Detected file event: {:?}", event);
                        match event {
                            DebouncedEvent::Create(p) => {
                                let filename = to_filename_str(&p);
                                if filename.ends_with(".tar.gz") {
                                    // Artifact tarball creation detected
                                    let build_id = filename.trim_end_matches(".tar.gz");
                                    info!("Creating artifact cache for build {}", build_id);
                                    let mut cache_dir = PathBuf::from(&cache_dir);
                                    cache_dir.push(build_id);
                                    extract_to(&cache_dir, &p).unwrap_or_else(|_| {
                                        panic!("Failed to extract artifact tarball {}", p.display())
                                    });
                                    tx.blocking_send(BuildEvent::Update(build_id.into()))
                                        .expect("Failed to send build event");
                                }
                            }
                            DebouncedEvent::Write(p) => {
                                let filename = to_filename_str(&p);
                                if filename.ends_with(".tar.gz") {
                                    // Artifact tarball update detected
                                    let build_id = filename.trim_end_matches(".tar.gz");
                                    info!("Updating artifact cache for build {}", build_id);
                                    let mut cache_dir = PathBuf::from(&cache_dir);
                                    cache_dir.push(build_id);
                                    extract_to(&cache_dir, &p).unwrap_or_else(|_| {
                                        panic!("Failed to extract artifact tarball {}", p.display())
                                    });
                                    tx.blocking_send(BuildEvent::Update(build_id.into()))
                                        .expect("Failed to send build event");
                                }
                            }
                            DebouncedEvent::Remove(p) => {
                                let filename = to_filename_str(&p);
                                if filename.ends_with(".tar.gz") {
                                    // Artifact tarball removal detected
                                    let build_id = filename.trim_end_matches(".tar.gz");
                                    info!("Deleting artifact cache for build {}", build_id);
                                    let mut cache_dir = PathBuf::from(&cache_dir);
                                    cache_dir.push(build_id);
                                    maybe_remove_dir(&cache_dir).unwrap_or_else(|_| {
                                        panic!(
                                            "Failed to remove cache directory {}",
                                            cache_dir.display()
                                        )
                                    });
                                    tx.blocking_send(BuildEvent::Delete(build_id.into()))
                                        .expect("Failed to send build event");
                                }
                            }
                            _ => (),
                        }
                    }
                    Err(e) => panic!("File watcher error: {:?}", e),
                }
            }
        }
    });
    rx
}
