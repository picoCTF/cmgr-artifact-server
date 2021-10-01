use clap::{App, Arg};
use cmgr_artifact_server::{Backend, S3, Selfhosted};
fn main() {
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

        let options = if let Some(v) = matches.values_of("backend-option") {
            v.collect::<Vec<&str>>()
        } else {
            vec![]
        };
        let options = options.as_slice();
        match matches.value_of("backend").unwrap() {
            "selfhosted" => run_backend::<Selfhosted>(&options),
            "s3" => run_backend::<S3>(&options),
            _ => panic!("Unimplemented backend")  // TODO: use enum instead
        };
}

fn run_backend<T: Backend>(options: &[&str]) {
    match T::new(&options) {
        Ok(backend) => backend.run(),
        Err(error) => {eprintln!("{}", error);}
    }
}
