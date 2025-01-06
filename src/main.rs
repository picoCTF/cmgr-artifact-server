mod backend;
mod watcher;

use backend::{Backend, S3Backend, SelfhostedBackend};
use clap::{Arg, ArgAction, Command};
use log::{debug, info};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use watcher::{sync_cache, watch_dir};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let matches = Command::new(clap::crate_name!())
    .version(clap::crate_version!())
    .author(clap::crate_authors!())
    .about(clap::crate_description!())
    .after_help(
        "The CMGR_ARTIFACT_DIR environment variable is used to determine which files to serve. \
        \nThe current directory will be used if it is not set.\n\n"
    )
    .arg(Arg::new("backend")
        .short('b')
        .long("backend")
        .help("File hosting backend")
        .value_parser(["selfhosted", "s3"])
        .ignore_case(true)
        .required(true)
    )
    .arg(Arg::new("log-level")
        .short('l')
        .long("log-level")
        .help("Log level")
        .value_parser(["error", "warn", "info", "debug", "trace"])
        .ignore_case(true)
        .default_value("info")
    )
    .arg(Arg::new("backend-option")
        .short('o')
        .long("backend-option")
        .help("Backend-specific option in key=value format.\nMay be specified multiple times.")
        .action(ArgAction::Append)
.value_parser(parse_backend_option)
        .number_of_values(1)
    )
    .get_matches();

    // Initialize logger
    env_logger::builder()
        .parse_filters(&format!(
            "cmgr_artifact_server={}",
            matches.get_one::<String>("log-level").unwrap()
        ))
        .init();

    // Collect supplied backend options
    let backend_options: HashMap<String, String> =
if let Some(options) = matches.get_many::<(String, String)>("backend-option") {
        HashMap::from_iter(options.cloned())
    } else {
        HashMap::new()
    };
        debug!("Supplied backend options: {backend_options:?}");

    // Determine artifact directory
    let artifact_dir = env::var("CMGR_ARTIFACT_DIR").unwrap_or_else(|_| ".".into());
    let artifact_dir = PathBuf::from(&artifact_dir);
    debug!("Determined artifact dir: {}", &artifact_dir.display());
    let mut cache_dir = artifact_dir.clone();
    cache_dir.push(".artifact_server_cache");
    debug!("Determined cache dir: {}", &cache_dir.display());

    // Ensure cache directory exists
    fs::create_dir_all(&cache_dir)?;

    // Synchronize cache directory
    info!("Updating artifact cache");
    sync_cache(&artifact_dir, &cache_dir)?;

    // Watch artifact directory
    let rx = watch_dir(&artifact_dir, &cache_dir);

    // Start backend
    match matches
        .get_one::<String>("backend")
        .unwrap()
        .to_lowercase()
        .as_str()
    {
        "selfhosted" => {
SelfhostedBackend::new(backend_options)?
.run(&cache_dir, rx)
.await
        }
        "s3" => S3Backend::new(backend_options)?.run(&cache_dir, rx).await,
        _ => panic!("Unreachable - invalid backend"), // TODO: use enum instead
    }?;
    Ok(())
}

/// Parses a backend option in `key=value` format.
fn parse_backend_option(option: &str) -> Result<(String, String), anyhow::Error> {
            if let Some((key, value)) = option.split_once('=') {
            Ok((key.to_owned(), value.to_owned()))
        } else {
            anyhow::bail!("Provided backend option \"{option}\" is invalid. Backend options must be specified in key=value format.");
            }
    }
