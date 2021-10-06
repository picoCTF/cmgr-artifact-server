use crate::{Backend, BackendCreationError, BuildEvent};
use async_trait::async_trait;
use log::info;
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::mpsc::Receiver;

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

    fn new(options: HashMap<&str, &str>) -> Result<Self, BackendCreationError> {
        let bucket_name = match options.get("bucket") {
            Some(bucket_name) => bucket_name,
            None => return Err(BackendCreationError),
        };
        let backend = Self {
            bucket: bucket_name.to_string(),
            path_prefix: options.get("path_prefix").unwrap_or(&&"/").to_string(),
            cloudfront_distribution: options
                .get("cloudfront_distribution")
                .map(|v| v.to_string()),
        };
        Ok(backend)
    }

    async fn run(
        &self,
        cache_dir: &Path,
        mut rx: Receiver<BuildEvent>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Sync existing artifacts
        info!("Syncing current artifact cache to S3");

        // Handle build events
        info!("Watching for changes. Press CTRL-C to exit.");
        while let Some(event) = rx.recv().await {
            println!("{:?}", event);
        }
        Ok(())
    }
}
