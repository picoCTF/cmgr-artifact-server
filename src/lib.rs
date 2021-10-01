use std::fmt::Display;

pub struct BackendCreationError;

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
    fn new(options: &[&str]) -> Result<Self, BackendCreationError>;

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

    fn new(options: &[&str]) -> Result<Self, BackendCreationError> {
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

    fn new(options: &[&str]) -> Result<Self, BackendCreationError> {
        Err(BackendCreationError)
    }

    fn run(&self) {
        todo!()
    }
}
