use crate::{Backend, BackendCreationError, BuildEvent};
use async_trait::async_trait;
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
        // todo: check required options
        Ok(Self {
            bucket: "fsfd".into(),
            path_prefix: "test".into(),
            cloudfront_distribution: None,
        })
    }

    async fn run(
        &self,
        cache_dir: &Path,
        mut rx: Receiver<BuildEvent>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Sync existing artifacts
        // TODO

        // Handle build events
        while let Some(event) = rx.recv().await {
            println!("{:?}", event);
        }
        Ok(())
    }
}
