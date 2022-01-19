use clap::{App, Arg};
use cmgr_artifact_server::{sync_cache, watch_dir, Backend, OptionParsingError, Selfhosted, S3};
use log::{debug, info};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;

#[tokio::main]
async fn main() {
    if let Err(e) = run_app().await {
        eprintln!("{}", e);
        process::exit(1);
    }
}

async fn run_app() -> Result<(), Box<dyn std::error::Error>> {
    let matches = App::new(clap::crate_name!())
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
        .takes_value(true)
        .possible_values(&["selfhosted", "s3"])
        .ignore_case(true)
        .required(true)
    )
    .arg(Arg::new("log-level")
        .short('l')
        .long("log-level")
        .help("Log level")
        .takes_value(true)
        .possible_values(&["error", "warn", "info", "debug", "trace"])
        .ignore_case(true)
        .default_value("info")
    )
    .arg(Arg::new("backend-option")
        .short('o')
        .long("backend-option")
        .help("Backend-specific option in key=value format.\nMay be specified multiple times.")
        .takes_value(true)
        .multiple_occurrences(true)
        .number_of_values(1)
    )
    .get_matches();

    // Initialize logger
    env_logger::builder()
        .parse_filters(&format!(
            "cmgr_artifact_server={}",
            matches.value_of("log-level").unwrap()
        ))
        .init();

    // Collect supplied backend options
    let options = if let Some(v) = matches.values_of("backend-option") {
        v.collect::<Vec<&str>>()
    } else {
        vec![]
    };
    let options = parse_options(options)?;
    debug!("Supplied backend options: {:?}", options);

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
    match matches.value_of("backend").unwrap().to_lowercase().as_str() {
        "selfhosted" => Selfhosted::new(options)?.run(&cache_dir, rx).await,
        "s3" => S3::new(options)?.run(&cache_dir, rx).await,
        _ => panic!("Unreachable - invalid backend"), // TODO: use enum instead
    }?;
    Ok(())
}

fn parse_options(options: Vec<&str>) -> Result<HashMap<&str, &str>, OptionParsingError> {
    let mut map = HashMap::new();
    for option in options {
        if let Some((key, value)) = option.split_once("=") {
            map.insert(key, value);
        } else {
            return Err(OptionParsingError);
        }
    }
    Ok(map)
}
