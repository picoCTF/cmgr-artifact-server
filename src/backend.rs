mod s3;
mod selfhosted;

pub(crate) use s3::S3Backend;
pub(crate) use selfhosted::SelfhostedBackend;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use tokio::sync::mpsc::Receiver;

use crate::{BuildEvent, BuildId};

pub(crate) trait Backend: Sized {
    /// Whether this backend needs each tarball unpacked into the cache.
    ///
    /// A backend that serves files off local disk does: the cache *is* what
    /// it serves. One that copies bytes elsewhere does not, and unpacking for
    /// it costs a second copy of every artifact on the machine that builds
    /// them -- for a corpus of any size, most of a disk.
    ///
    /// So this is stated per backend rather than decided by whoever wires up
    /// the watcher, and a new backend has to answer it. When it is false the
    /// cache holds a checksum per build and nothing else, and the backend is
    /// given the tarballs instead (see `run`).
    const NEEDS_EXTRACTED_FILES: bool;

    /// Create an instance of the backend if all required options are provided.
    async fn new(options: HashMap<String, String>) -> Result<Self, anyhow::Error>;

    /// Run the backend.
    ///
    /// The backend is not given the artifact directory (i.e. CMGR_ARTIFACT_DIR) itself. It is
    /// given the cache directory, whose subdirectories are named with the associated build key --
    /// a build ID, digested if a salt is in use, under the namespace directory its tarball was
    /// found in, if any. That directory always holds the build's .__checksum, and holds the
    /// unpacked tarball as well when NEEDS_EXTRACTED_FILES is true. It is kept up to date by a
    /// background thread when the server is run as a binary.
    ///
    /// It is also given `tarballs`, the source tarball of every build the cache knows about, by
    /// the same key. A backend that does not need the files reads them from there; one that does
    /// can ignore it.
    ///
    /// When a backend runs, it should first perform any synchronization necessary in order to
    /// reflect the current contents of the cache directory. For example, if the backend syncs files
    /// to remote storage, any directories without matching .__checksum files should be re-uploaded,
    /// and any remote directories which no longer exist in the cache should be removed.
    ///
    /// After completing this initial synchronization, the backend should listen on the provided
    /// channel for build events and take action accordingly. These events are produced when a build
    /// with artifacts is (re-)created (BuildEvent::Update) or deleted (BuildEvent::Delete), and
    /// carry the build's key -- and, where there is one, the tarball it came from.
    ///
    /// As there is the potential for race conditions when handling build events, backends must
    /// process any events with the same build ID serially in the order of their arrival.
    async fn run(
        &self,
        cache_dir: &Path,
        namespaces: &HashSet<String>,
        tarballs: &HashMap<BuildId, PathBuf>,
        rx: Receiver<BuildEvent>,
    ) -> Result<(), anyhow::Error>;
}
