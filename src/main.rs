use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use partsrelay_tunnel::cli::{Cli, Command};
use partsrelay_tunnel::config::Config;
use partsrelay_tunnel::update::Updater;
use partsrelay_tunnel::{Exit, VERSION, check, logging, shutdown};
use tracing::{error, info, warn};

// EX_TEMPFAIL: the supervisor starts the new version
const RESTART_FAILED: u8 = 75;

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Before any update replaces the file
    let exe = std::env::current_exe().ok();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();

    logging::init(cli.log_level, cli.log_format);
    // The update client needs a process wide TLS provider
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = match Config::load(cli.config.as_deref()) {
        Ok(cfg) => Arc::new(cfg),
        Err(error) => {
            error!(%error, "configuration");
            return ExitCode::from(2);
        }
    };

    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(cfg, exe, &args),
        Command::Check => {
            let healthy = runtime().block_on(check::run(&cfg));
            if healthy {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Command::Update { check } => update(&cfg, check),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the async runtime starts")
}

fn run(cfg: Arc<Config>, exe: Option<PathBuf>, args: &[OsString]) -> ExitCode {
    if cfg.allow.is_empty() {
        warn!("the allow list is empty: every target will be refused");
    }
    info!(version = VERSION, "starting");

    let runtime = runtime();
    let outcome = runtime.block_on(async {
        let shutdown = shutdown::on_signal();
        partsrelay_tunnel::run(cfg, shutdown).await
    });
    drop(runtime);

    match outcome {
        Ok(Exit::Shutdown) => ExitCode::SUCCESS,
        Ok(Exit::Restart) => restart(exe, args),
        Err(error) => {
            error!(error = format!("{error:#}"), "stopped");
            ExitCode::FAILURE
        }
    }
}

#[cfg(unix)]
fn restart(exe: Option<PathBuf>, args: &[OsString]) -> ExitCode {
    use partsrelay_tunnel::update::install;

    if let Some(exe) = exe {
        let error = install::restart(&exe, args);
        error!(%error, "cannot start the new version");
    }

    ExitCode::from(RESTART_FAILED)
}

// The scheduled task starts the new version within a minute
#[cfg(not(unix))]
fn restart(_exe: Option<PathBuf>, _args: &[OsString]) -> ExitCode {
    let _ = RESTART_FAILED;
    info!("updated, start the tunnel again if nothing restarts it");

    ExitCode::SUCCESS
}

fn update(cfg: &Config, only_check: bool) -> ExitCode {
    let outcome = Updater::embedded(cfg).and_then(|updater| {
        if only_check {
            return Ok(updater.check()?.map(|manifest| (manifest.version, false)));
        }
        Ok(updater.update()?.map(|version| (version, true)))
    });

    match outcome {
        Ok(Some((version, true))) => println!("updated to {version}, restart the tunnel"),
        Ok(Some((version, false))) => println!("version {version} is available"),
        Ok(None) => println!("version {VERSION} is the latest"),
        Err(error) => {
            eprintln!("update failed: {error:#}");
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}
