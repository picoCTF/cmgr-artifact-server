use std::fmt::Display;
use std::collections::HashMap;
use std::error::Error;

#[derive(Debug)]
pub struct OptionParsingError;

impl Error for OptionParsingError {}

impl Display for OptionParsingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Encountered an invalid option. Backend options must be specified in key=value format.")
    }
}

#[derive(Debug)]
pub struct BackendCreationError;

impl Error for BackendCreationError {}

impl Display for BackendCreationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Unable to initialize backend. Some required options were not provided.")
    }
}

pub trait Backend: Sized {
    /// Returns a list of option keys supported by this backend.
    fn get_options() -> &'static [&'static str];

    /// Returns a list of option keys required by this backend.
    fn get_required_options() -> &'static [&'static str];

    /// Create an instance of the backend if all required options are provided.
    fn new(options: HashMap<&str, &str>) -> Result<Self, BackendCreationError>;

    /// Runs the backend.
    fn run(&self);
}

pub struct Selfhosted {
    address: String,
}

impl Backend for Selfhosted {

    fn get_options() -> &'static [&'static str] {
        &["address"]
    }

    fn get_required_options() -> &'static [&'static str] {
        &[]
    }

    fn new(options: HashMap<&str, &str>) -> Result<Self, BackendCreationError> {
        todo!()
    }

    fn run(&self) {
        todo!()
    }

}

pub struct S3 {
    bucket: String,
    path_prefix: String,
    cloudfront_distribution: Option<String>,
}

impl Backend for S3 {
    fn get_options() -> &'static [&'static str] {
        &["bucket", "path-prefix", "cloudfront-distribution"]
    }

    fn get_required_options() -> &'static [&'static str] {
        &["bucket"]
    }

    fn new(options: HashMap<&str, &str>) -> Result<Self, BackendCreationError> {
        Err(BackendCreationError)
    }

    fn run(&self) {
        todo!()
    }
}
