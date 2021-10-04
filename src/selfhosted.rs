use crate::{Backend, BackendCreationError};
use async_trait::async_trait;
use blake2::{Blake2b, Digest};
use flate2::read::GzDecoder;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use log::{debug, info};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::error::Error;
use std::fmt::Debug;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tar::Archive;

#[derive(Debug)]
pub struct Selfhosted {
    address: String,
}

impl Selfhosted {
    async fn handle_request<B>(
        req: Request<B>,
        artifact_dir: PathBuf,
    ) -> Result<Response<Body>, std::io::Error> {
        let res = if req.uri().path() == "/health" {
            http::Response::builder()
                .status(http::StatusCode::OK)
                .body(hyper::Body::empty())
                .expect("Unable to build response")
        } else {
            if let Some((build, _file)) = req.uri().path()[1..].split_once("/") {
                Self::check_cache(build, &artifact_dir).expect("Error updating artifact cache");
            }
            let result = hyper_staticfile::resolve(&artifact_dir, &req)
                .await
                .unwrap();
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

    /// Ensures that the unzipped cache directory for the specified build is up to date.
    fn check_cache(build: &str, artifact_dir: &Path) -> Result<(), std::io::Error> {
        debug!("Checking cache for build {}", build);
        let mut tarball_path = artifact_dir.parent().unwrap().to_path_buf();
        tarball_path.push(format!("{}.tar.gz", build));

        let mut cache_dir_path = PathBuf::new();
        cache_dir_path.push(artifact_dir);
        cache_dir_path.push(build);

        let mut checksum_path = cache_dir_path.clone();
        checksum_path.push(".__checksum");

        // If a corresponding ID.tar.gz does not exist, maybe the build ID is no longer valid and we
        // should remove any existing cache directory.
        if !tarball_path.is_file() {
            debug!("No tarball exists for build {}, removing cache", build);
            Selfhosted::maybe_remove_dir(&cache_dir_path)?;
            return Ok(());
        }

        // Check whether the tarball checksum matches the cache directory.
        // If not, delete the cache directory and recreate it.
        let mut hasher = Blake2b::new();
        let mut tarball = File::open(tarball_path)?;
        let mut buf = [0; 4096];
        loop {
            // Avoid reading all of tarball into memory at once
            match tarball.read(&mut buf) {
                Ok(n) if n > 0 => {
                    hasher.update(&buf[..n]);
                }
                Ok(_) => break,
                Err(e) => return Err(e),
            }
        }
        let tarball_hash = hasher.finalize();
        if let Ok(recorded_hash) = fs::read(&checksum_path) {
            if recorded_hash == tarball_hash.as_slice() {
                // Current cache dir matches tarball
                debug!("Cache OK for build {}", build);
                return Ok(());
            }
        }
        debug!("Recreating cache for build {}", build);
        Selfhosted::maybe_remove_dir(&cache_dir_path)?;
        fs::create_dir_all(&cache_dir_path)?;
        fs::write(&checksum_path, tarball_hash.as_slice())?;
        tarball.seek(SeekFrom::Start(0))?;
        let tar = GzDecoder::new(tarball);
        let mut archive = Archive::new(tar);
        archive.unpack(&cache_dir_path)?;
        Ok(())
    }

    /// Removes a directory, ignoring errors if it does not exist.
    fn maybe_remove_dir(path: &Path) -> Result<(), std::io::Error> {
        match fs::remove_dir_all(path) {
            Ok(_) => Ok(()),
            Err(e) => match e.kind() {
                // TODO: ErrorKind::NotADirectory would be better but is unstable
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            },
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
        let backend = Selfhosted {
            address: options
                .get("address")
                .unwrap_or(&"0.0.0.0:4201")
                .to_string(),
        };
        debug!("Created backend: {:?}", backend);
        Ok(backend)
    }

    async fn run(&self, path: &Path) -> Result<(), Box<dyn Error>> {
        let addr: SocketAddr = self.address.parse()?;
        const CACHE_SUBDIR: &str = ".artifact_server_cache";
        let mut path = PathBuf::from(path);
        path.push(CACHE_SUBDIR);

        let make_service = make_service_fn(|_| {
            let path = path.clone();
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
