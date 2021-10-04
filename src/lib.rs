mod s3;
mod selfhosted;

use async_trait::async_trait;
pub use s3::S3;
pub use selfhosted::Selfhosted;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display};
use std::path::Path;

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

#[async_trait]
pub trait Backend: Sized {
    /// Returns a list of option keys supported by this backend.
    fn get_options() -> &'static [&'static str];

    /// Returns a list of option keys required by this backend.
    fn get_required_options() -> &'static [&'static str];

    /// Create an instance of the backend if all required options are provided.
    fn new(options: HashMap<&str, &str>) -> Result<Self, BackendCreationError>;

    /// Runs the backend.
    async fn run(&self, path: &Path) -> Result<(), Box<dyn std::error::Error>>;
}
