mod s3;
mod selfhosted;

use async_trait::async_trait;
use notify::{DebouncedEvent, RecommendedWatcher, Watcher};
pub use s3::S3;
pub use selfhosted::Selfhosted;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display};
use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
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
pub enum TarballEvent {
    Update(String),
    Delete(String),
}

#[async_trait]
pub trait Backend: Sized {
    /// Return a list of option keys supported by this backend.
    fn get_options() -> &'static [&'static str];

    /// Return a list of option keys required by this backend.
    fn get_required_options() -> &'static [&'static str];

    /// Create an instance of the backend if all required options are provided.
    fn new(options: HashMap<&str, &str>) -> Result<Self, BackendCreationError>;

    /// Run the backend.
    /// Should perform any necessary initialization based on existing directory state, then listen
    /// on the provided channel for TarballEvents and process them sequentially.
    async fn run(
        &self,
        artifact_dir: &Path,
        rx: &mut Receiver<TarballEvent>,
    ) -> Result<(), Box<dyn std::error::Error>>;
}

/// Spawns a thread watching for changes to artifact tarballs in the specified directory.
pub fn watch_dir(artifact_dir: &Path) -> Receiver<TarballEvent> {
    let (tx, rx) = channel(32);
    thread::spawn({
        let artifact_dir = PathBuf::from(artifact_dir);
        move || {
            let (watcher_tx, watcher_rx) = std::sync::mpsc::channel();
            let mut watcher: RecommendedWatcher =
                Watcher::new(watcher_tx, Duration::from_secs(2)).unwrap();
            watcher
                .watch(&artifact_dir, notify::RecursiveMode::NonRecursive)
                .unwrap();
            loop {
                match watcher_rx.recv() {
                    Ok(event) => {
                        println!("{:?}", event);
                        match event {
                            DebouncedEvent::Create(p) => {
                                tx.blocking_send(TarballEvent::Update(p.to_str().unwrap().into()))
                                    .expect("Failed to send build event");
                            }
                            _ => (),
                        }
                    }
                    Err(e) => println!("watch error: {:?}", e),
                }
            }
        }
    });
    rx
}
