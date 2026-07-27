//! Command-line interface and process entry point for the seeder.

use crate::{
    config::{SeederConfig, ZcashNetwork},
    crawl::activation,
};
use clap::{Parser, Subcommand};
use color_eyre::eyre::{Context, Result, bail};
use std::{io::Write, path::PathBuf};
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

const DEFAULT_LOG_FILTER: &str = "info";
const RUST_LOG_ENV: &str = "RUST_LOG";

/// Command-line arguments for the seeder.
#[derive(Parser, Debug)]
#[command(author, version = crate::build_info::cli_version(), about = "Zeeder, a Zcash DNS seeder", long_about = None)]
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
    /// Explicitly attest an already-activated compiled network upgrade.
    AttestActivation {
        /// Network whose compiled activation is being attested.
        #[arg(long)]
        network: ZcashNetwork,

        /// Zebra cache root that contains the network peer-cache directory.
        #[arg(long)]
        cache_dir: PathBuf,

        /// Activation height independently verified by the operator.
        #[arg(long)]
        activation_height: u32,

        /// Confirmation height independently verified by the operator.
        #[arg(long)]
        confirmation_height: u32,

        /// Minimum protocol version independently verified by the operator.
        #[arg(long)]
        minimum_protocol_version: u32,
    },
}

fn log_filter_from_env() -> Result<EnvFilter> {
    match std::env::var(RUST_LOG_ENV) {
        Ok(filter) => EnvFilter::try_new(&filter)
            .wrap_err_with(|| format!("failed to parse {RUST_LOG_ENV}={filter:?}")),
        Err(std::env::VarError::NotPresent) => Ok(EnvFilter::new(DEFAULT_LOG_FILTER)),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("{RUST_LOG_ENV} must be valid UTF-8");
        }
    }
}

impl SeederApp {
    pub(crate) async fn run() -> Result<()> {
        let Self { config, command } = Self::parse();

        // Log verbosity is controlled by RUST_LOG (for example `RUST_LOG=debug`),
        // defaulting to `info`. Logs go to stderr so stdout stays clean for
        // piping `print-config` output.
        tracing_subscriber::registry()
            .with(log_filter_from_env()?)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();

        match command {
            Commands::Start => {
                let config =
                    SeederConfig::load_with_env(config).wrap_err("failed to load configuration")?;
                info!("Starting zeeder with config: {config:?}");

                if let Some(metrics_config) = &config.metrics {
                    crate::metrics::init(metrics_config.endpoint_addr)?;
                }

                crate::seeder::run(config).await?;
            }
            Commands::PrintConfig => {
                let config =
                    SeederConfig::load_with_env(config).wrap_err("failed to load configuration")?;
                let rendered =
                    toml::to_string_pretty(&config).wrap_err("failed to render config as TOML")?;
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(rendered.as_bytes())
                    .wrap_err("failed to write config to stdout")?;
            }
            Commands::AttestActivation {
                network,
                cache_dir,
                activation_height,
                confirmation_height,
                minimum_protocol_version,
            } => {
                let zcash_network = network.to_zebra();
                let cache_dir = zebra_network::config::CacheDir::custom_path(cache_dir);
                let path = activation::attest_confirmation(
                    &cache_dir,
                    &zcash_network,
                    activation_height,
                    confirmation_height,
                    minimum_protocol_version,
                )
                .await
                .wrap_err("failed to persist operator activation attestation")?;

                info!(
                    network = network.label(),
                    path = %path.display(),
                    activation_height,
                    confirmation_height,
                    minimum_protocol_version,
                    "persisted explicit operator activation attestation"
                );
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
        assert_eq!(cmd.get_name(), "zeeder");
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
        assert!(
            subcommands.contains(&"attest-activation"),
            "should have 'attest-activation'"
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
    fn log_filter_defaults_when_rust_log_is_missing() -> TestResult {
        temp_env::with_var(RUST_LOG_ENV, None::<&str>, log_filter_from_env)?;

        Ok(())
    }

    #[test]
    fn log_filter_rejects_invalid_rust_log() {
        let filter = temp_env::with_var(RUST_LOG_ENV, Some("["), log_filter_from_env);

        assert!(filter.is_err(), "invalid RUST_LOG should fail");
    }

    #[test]
    fn parses_start_subcommand() -> TestResult {
        let app = SeederApp::try_parse_from(["zeeder", "start"])?;
        assert!(matches!(app.command, Commands::Start));
        assert!(app.config.is_none());
        Ok(())
    }

    #[test]
    fn parses_print_config_subcommand() -> TestResult {
        let app = SeederApp::try_parse_from(["zeeder", "print-config"])?;
        assert!(matches!(app.command, Commands::PrintConfig));
        Ok(())
    }

    #[test]
    fn parses_attest_activation_subcommand() -> TestResult {
        let app = SeederApp::try_parse_from([
            "zeeder",
            "attest-activation",
            "--network",
            "testnet",
            "--cache-dir",
            "/cache/zebra",
            "--activation-height",
            "4134000",
            "--confirmation-height",
            "4135000",
            "--minimum-protocol-version",
            "170160",
        ])?;

        assert!(matches!(
            app.command,
            Commands::AttestActivation {
                network: ZcashNetwork::Testnet,
                cache_dir,
                activation_height: 4_134_000,
                confirmation_height: 4_135_000,
                minimum_protocol_version: 170_160,
            } if cache_dir == std::path::Path::new("/cache/zebra")
        ));
        Ok(())
    }

    #[test]
    fn parses_global_config_before_subcommand() -> TestResult {
        let app =
            SeederApp::try_parse_from(["zeeder", "--config", "/path/to/config.toml", "start"])?;
        assert_eq!(
            app.config.as_deref().and_then(std::path::Path::to_str),
            Some("/path/to/config.toml")
        );
        Ok(())
    }

    #[test]
    fn parses_global_config_after_subcommand() -> TestResult {
        let app =
            SeederApp::try_parse_from(["zeeder", "start", "--config", "/path/to/config.toml"])?;
        assert_eq!(
            app.config.as_deref().and_then(std::path::Path::to_str),
            Some("/path/to/config.toml")
        );
        Ok(())
    }
}
