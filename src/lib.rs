use async_trait::async_trait;
use hyper_staticfile::Static;
use std::fmt::Display;
use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
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
    async fn handle_request<B>(req: Request<B>, static_: Static) -> Result<Response<Body>, std::io::Error>{
        if req.uri().path() == "/health" {
            let res = http::Response::builder()
                .status(http::StatusCode::OK)
                .body(hyper::Body::empty())
                .expect("Unable to build response");
            Ok(res)
        } else {
            static_.clone().serve(req).await
        }
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
        let addr: SocketAddr = self.address.parse()?;
        let static_ = hyper_staticfile::Static::new(Path::new(""));

        let make_service = make_service_fn(|_| {
            let static_ = static_.clone();
            async {
                Ok::<_, hyper::Error>(service_fn(move |req| Selfhosted::handle_request(req, static_.clone())))
            }
        });

        let server = Server::bind(&addr).serve(make_service);

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
