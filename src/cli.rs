use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::logging::{LogFormat, LogLevel};

/// Outbound proxy from a private network to Parts Relay.
#[derive(Debug, Parser)]
#[command(name = "partsrelay-tunnel", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("TARGET"), ")"))]
pub struct Cli {
    /// Configuration file, instead of the default one
    #[arg(
        long,
        global = true,
        env = "PARTSRELAY_TUNNEL_CONFIG",
        value_name = "FILE"
    )]
    pub config: Option<PathBuf>,

    #[arg(long, global = true, value_enum, default_value = "info")]
    pub log_level: LogLevel,

    #[arg(long, global = true, value_enum, default_value = "text")]
    pub log_format: LogFormat,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Connect and relay until stopped (the default)
    Run,
    /// Validate the configuration and try every connection
    Check,
    /// Install the latest release now
    Update {
        /// Only tell whether a newer release exists
        #[arg(long)]
        check: bool,
    },
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn runs_by_default_and_reads_the_global_options_anywhere() {
        let plain = Cli::try_parse_from(["partsrelay-tunnel"]).unwrap();
        let update = Cli::try_parse_from([
            "partsrelay-tunnel",
            "update",
            "--check",
            "--config",
            "/tmp/tunnel.toml",
            "--log-format",
            "json",
        ])
        .unwrap();

        assert_eq!(plain.command, None);
        assert_eq!(update.command, Some(Command::Update { check: true }));
        assert_eq!(update.config, Some(PathBuf::from("/tmp/tunnel.toml")));
        assert_eq!(update.log_format, LogFormat::Json);
        Cli::command().debug_assert();
    }
}
