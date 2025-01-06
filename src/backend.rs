mod s3;
mod selfhosted;

pub(crate) use s3::S3Backend;
pub(crate) use selfhosted::SelfhostedBackend;

use std::{collections::HashMap, future::Future, path::Path};
use tokio::sync::mpsc::Receiver;

use crate::watcher::BuildEvent;

pub trait Backend: Sized {
    /// Create an instance of the backend if all required options are provided.
    fn new(options: HashMap<String, String>) -> Result<Self, anyhow::Error>;

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
    fn run(
        &self,
        cache_dir: &Path,
        rx: Receiver<BuildEvent>,
    ) -> impl Future<Output = Result<(), anyhow::Error>> + Send;
}
