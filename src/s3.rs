use crate::{Backend, BackendCreationError, TarballEvent};
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
        artifact_dir: &Path,
        rx: &mut Receiver<TarballEvent>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        while let Some(event) = rx.recv().await {
            println!("{:?}", event);
        }
        Ok(())
    }
}
