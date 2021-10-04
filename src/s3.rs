use crate::{Backend, BackendCreationError};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::Path;

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
        todo!()
    }

    async fn run(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        todo!()
    }
}
