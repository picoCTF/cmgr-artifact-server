use crate::backend::Backend;
use crate::watcher::BuildEvent;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_staticfile::{Body, Static};
use hyper_util::rt::TokioIo;
use log::{debug, info};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::path::Path;
use tokio::net::TcpListener;
use tokio::sync::mpsc::Receiver;

#[derive(Debug)]
pub struct SelfhostedBackend {
    address: String,
}

async fn handle_request<B>(
    req: Request<B>,
    static_: Static,
) -> Result<Response<Body>, std::io::Error> {
    let res = if req.uri().path() == "/health" {
        http::Response::builder()
            .status(http::StatusCode::OK)
            .body(Body::Empty)
            .expect("Unable to build response")
    } else if req.uri().path().ends_with(".__checksum") {
        http::Response::builder()
            .status(http::StatusCode::NOT_FOUND)
            .body(Body::Empty)
            .expect("Unable to build response")
    } else {
        let result = static_.resolver.resolve_request(&req).await?;
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

impl Backend for SelfhostedBackend {
    fn new(options: HashMap<String, String>) -> Result<Self, anyhow::Error> {
        let backend = SelfhostedBackend {
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
    ) -> Result<(), anyhow::Error> {
        let static_ = Static::new(cache_dir);

        let addr: SocketAddr = self.address.parse()?;
        let listener = TcpListener::bind(addr).await?;
        info!("Starting server ({}). Press CTRL-C to exit.", &self.address);
        loop {
            let (stream, _) = listener.accept().await?;
            let static_ = static_.clone();
            tokio::spawn(async move {
                if let Err(err) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req| handle_request(req, static_.clone())),
                    )
                    .await
                {
                    eprintln!("Error serving connection: {:?}", err);
                }
            });
        }
    }
}
