//! Command-line interface and process entry point for the seeder.

use crate::config::SeederConfig;
use clap::{Parser, Subcommand};
use color_eyre::eyre::{Context, Result};
use std::path::PathBuf;
use std::{io::Write, sync::OnceLock};
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Command-line arguments for the seeder.
#[derive(Parser, Debug)]
#[command(author, version = build_version(), about = "Zcash DNS Seeder", long_about = None)]
pub(crate) struct SeederApp {
    /// Path to a TOML configuration file.
    #[arg(short, long, global = true)]
    pub(crate) config: Option<PathBuf>,

    /// The subcommand to run.
    #[command(subcommand)]
    pub(crate) command: Commands,
}

/// Seeder subcommands.
#[derive(Subcommand, Debug)]
pub(crate) enum Commands {
    /// Start the DNS seeder.
    Start,
    /// Print the resolved configuration as TOML and exit.
    PrintConfig,
}

fn build_version() -> &'static str {
    static BUILD_VERSION: OnceLock<String> = OnceLock::new();

    BUILD_VERSION.get_or_init(|| {
        option_env!("VERGEN_GIT_SHA").map_or_else(
            || env!("CARGO_PKG_VERSION").to_string(),
            |sha| {
                let short_sha = &sha[..7.min(sha.len())];
                format!("{} ({short_sha})", env!("CARGO_PKG_VERSION"))
            },
        )
    })
}

impl SeederApp {
    pub(crate) async fn run() -> Result<()> {
        let app = Self::parse();

        // Log verbosity is controlled by RUST_LOG (for example `RUST_LOG=debug`),
        // defaulting to `info`. Logs go to stderr so stdout stays clean for
        // piping `print-config` output.
        tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();

        let config =
            SeederConfig::load_with_env(app.config).wrap_err("failed to load configuration")?;

        match app.command {
            Commands::Start => {
                info!("Starting zebra-seeder with config: {config:?}");

                if let Some(metrics_config) = &config.metrics {
                    crate::metrics::init(metrics_config.endpoint_addr)?;
                }

                crate::seeder::run(config).await?;
            }
            Commands::PrintConfig => {
                let rendered =
                    toml::to_string_pretty(&config).wrap_err("failed to render config as TOML")?;
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(rendered.as_bytes())
                    .wrap_err("failed to write config to stdout")?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    type TestResult = color_eyre::Result<()>;

    #[test]
    fn test_cli_structure() {
        let cmd = SeederApp::command();
        assert_eq!(cmd.get_name(), "zebra-seeder");
    }

    #[test]
    fn test_subcommands_exist() {
        let cmd = SeederApp::command();
        let subcommands: Vec<_> = cmd.get_subcommands().map(clap::Command::get_name).collect();
        assert!(subcommands.contains(&"start"), "should have 'start'");
        assert!(
            subcommands.contains(&"print-config"),
            "should have 'print-config'"
        );
    }

    #[test]
    fn test_config_option_exists() {
        let cmd = SeederApp::command();
        let config_arg = cmd.get_arguments().find(|a| a.get_id() == "config");
        assert!(config_arg.is_some(), "should have --config option");
    }

    #[test]
    fn version_includes_git_sha_when_available() {
        let version = SeederApp::command().render_version();
        assert!(
            version.contains(env!("CARGO_PKG_VERSION")),
            "version should include package version"
        );

        if let Some(sha) = option_env!("VERGEN_GIT_SHA") {
            let short_sha = &sha[..7.min(sha.len())];
            assert!(
                version.contains(short_sha),
                "version should include short git sha"
            );
        }
    }

    #[test]
    fn parses_start_subcommand() -> TestResult {
        let app = SeederApp::try_parse_from(["zebra-seeder", "start"])?;
        assert!(matches!(app.command, Commands::Start));
        assert!(app.config.is_none());
        Ok(())
    }

    #[test]
    fn parses_print_config_subcommand() -> TestResult {
        let app = SeederApp::try_parse_from(["zebra-seeder", "print-config"])?;
        assert!(matches!(app.command, Commands::PrintConfig));
        Ok(())
    }

    #[test]
    fn parses_global_config_before_subcommand() -> TestResult {
        let app = SeederApp::try_parse_from([
            "zebra-seeder",
            "--config",
            "/path/to/config.toml",
            "start",
        ])?;
        assert_eq!(
            app.config.as_deref().and_then(std::path::Path::to_str),
            Some("/path/to/config.toml")
        );
        Ok(())
    }

    #[test]
    fn parses_global_config_after_subcommand() -> TestResult {
        let app = SeederApp::try_parse_from([
            "zebra-seeder",
            "start",
            "--config",
            "/path/to/config.toml",
        ])?;
        assert_eq!(
            app.config.as_deref().and_then(std::path::Path::to_str),
            Some("/path/to/config.toml")
        );
        Ok(())
    }
}
