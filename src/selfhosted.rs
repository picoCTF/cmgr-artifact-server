use crate::{Backend, BackendCreationError, BuildEvent};
use async_trait::async_trait;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use log::{debug, info};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::error::Error;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::Receiver;

#[derive(Debug)]
pub struct Selfhosted {
    address: String,
}

impl Selfhosted {
    async fn handle_request<B>(
        req: Request<B>,
        cache_dir: PathBuf,
    ) -> Result<Response<Body>, std::io::Error> {
        let res = if req.uri().path() == "/health" {
            http::Response::builder()
                .status(http::StatusCode::OK)
                .body(hyper::Body::empty())
                .expect("Unable to build response")
        } else if req.uri().path().ends_with(".__checksum") {
            http::Response::builder()
                .status(http::StatusCode::NOT_FOUND)
                .body(hyper::Body::empty())
                .expect("Unable to build response")
        } else {
            let result = hyper_staticfile::resolve(&cache_dir, &req).await?;
            let mut response = hyper_staticfile::ResponseBuilder::new()
                .request(&req)
                .build(result)
                .unwrap();
            if response.status() == http::StatusCode::OK {
                let headers = response.headers_mut();
                headers.insert(
                    http::header::CONTENT_DISPOSITION,
                    http::HeaderValue::try_from("attachment").unwrap(),
                );
            }
            response
        };
        info!(
            "Serving request: {} ({})",
            req.uri().to_string(),
            res.status()
        );
        Ok(res)
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

    fn new(options: HashMap<String, String>) -> Result<Self, BackendCreationError> {
        let backend = Selfhosted {
            address: options
                .get("address")
                .unwrap_or(&String::from("0.0.0.0:4201"))
                .to_string(),
        };
        debug!("Created backend: {:?}", backend);
        Ok(backend)
    }

    async fn run(
        &self,
        cache_dir: &Path,
        mut _rx: Receiver<BuildEvent>,
    ) -> Result<(), Box<dyn Error>> {
        let addr: SocketAddr = self.address.parse()?;

        let make_service = make_service_fn(move |_| {
            let path = PathBuf::from(cache_dir);
            async {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    Selfhosted::handle_request(req, path.clone())
                }))
            }
        });

        let server = Server::bind(&addr).serve(make_service);
        info!("Starting server ({}). Press CTRL-C to exit.", &self.address);
        server.await?;
        Ok(())
    }
}
