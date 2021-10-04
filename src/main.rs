use clap::{App, Arg};
use cmgr_artifact_server::{Backend, OptionParsingError, Selfhosted, S3};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::path::Path;
use std::process;

#[tokio::main]
async fn main() {
    if let Err(e) = run_app().await {
        eprintln!("{}", e);
        process::exit(1);
    }
}

async fn run_app() -> Result<(), Box<dyn Error>> {
    let matches = App::new(clap::crate_name!())
    .version(clap::crate_version!())
    .author(clap::crate_authors!())
    .about(clap::crate_description!())
    .after_help(
        "The CMGR_ARTIFACT_DIR environment variable is used to determine which files to serve. \
        \nThe current directory will be used if it is not set.\n\n"
    )
    .arg(Arg::with_name("backend")
        .short("b")
        .long("backend")
        .help("File hosting backend")
        .takes_value(true)
        .possible_values(&["selfhosted", "s3"])
        .required(true)
    )
    .arg(Arg::with_name("daemonize")
        .short("d")
        .long("daemonize")
        .help("Run in the background and do not log to stdout")
    )
    .arg(Arg::with_name("log-level")
        .short("l")
        .long("log-level")
        .help("Log level")
        .takes_value(true)
        .possible_values(&["error", "warn", "info", "debug", "trace"])
        .case_insensitive(true)
        .default_value("info")
    )
    .arg(Arg::with_name("backend-option")
        .short("o")
        .long("backend-option")
        .help("Backend-specific option in key=value format.\nMay be specified multiple times.")
        .takes_value(true)
        .multiple(true)
        .number_of_values(1)
    )
    .get_matches();

    // Initialize logger
    env_logger::builder().parse_filters(matches.value_of("log-level").unwrap()).init();

    // Collect supplied backend options
    let options = if let Some(v) = matches.values_of("backend-option") {
        v.collect::<Vec<&str>>()
    } else {
        vec![]
    };
    let options = parse_options(options)?;

    // Determine artifact directory
    let artifact_dir = env::var("CMGR_ARTIFACT_DIR").unwrap_or_else(|_| ".".into());
    let artifact_dir = Path::new(&artifact_dir);

    // Start backend
    match matches.value_of("backend").unwrap() {
        "selfhosted" => Selfhosted::new(options)?.run(artifact_dir).await,
        "s3" => S3::new(options)?.run(artifact_dir).await,
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
