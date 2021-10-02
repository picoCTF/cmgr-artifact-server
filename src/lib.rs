use async_trait::async_trait;
use std::fmt::Display;
use std::collections::HashMap;
use std::error::Error;
use tokio::runtime::Runtime;
use std::convert::Infallible;
use std::net::SocketAddr;
use hyper::{Body, Request, Response, Server};
use hyper::service::{make_service_fn, service_fn};

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

#[async_trait]
pub trait Backend: Sized {
    /// Returns a list of option keys supported by this backend.
    fn get_options() -> &'static [&'static str];

    /// Returns a list of option keys required by this backend.
    fn get_required_options() -> &'static [&'static str];

    /// Create an instance of the backend if all required options are provided.
    fn new(options: HashMap<&str, &str>) -> Result<Self, BackendCreationError>;

    /// Runs the backend.
    async fn run(&self) -> Result<(), Box<dyn std::error::Error>>;
}

pub struct Selfhosted {
    address: String,
}

impl Selfhosted {
    async fn hello_world(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
        Ok(Response::new("Hello, World".into()))
    }
}

#[async_trait]
impl Backend for Selfhosted {

    fn get_options() -> &'static [&'static str] {
        &["address"]
    }

    fn get_required_options() -> &'static [&'static str] {
        &[]
    }

    fn new(options: HashMap<&str, &str>) -> Result<Self, BackendCreationError> {
        Ok(Selfhosted {
            address: options.get("address").unwrap_or(&"0.0.0.0:4201").to_string(),
        })
    }

    async fn run(&self) -> Result<(), Box<dyn Error>> {
        // We'll bind to 127.0.0.1:3000
        let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

        // A `Service` is needed for every connection, so this
        // creates one from our `hello_world` function.
        let make_svc = make_service_fn(|_conn| async {
            // service_fn converts our function into a `Service`
            Ok::<_, Infallible>(service_fn(Selfhosted::hello_world))
        });

        let server = Server::bind(&addr).serve(make_svc);

        // Run this server for... forever!
        if let Err(e) = server.await {
            eprintln!("server error: {}", e);
        }
        Ok(())
    }

}

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
        Err(BackendCreationError)
    }

    async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
}
