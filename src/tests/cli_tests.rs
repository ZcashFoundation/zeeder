use crate::commands::{Commands, SeederApp};
use clap::Parser;

type TestResult = color_eyre::Result<()>;

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
    let app =
        SeederApp::try_parse_from(["zebra-seeder", "--config", "/path/to/config.toml", "start"])?;
    assert_eq!(
        app.config.as_deref().and_then(std::path::Path::to_str),
        Some("/path/to/config.toml")
    );
    Ok(())
}

#[test]
fn parses_global_config_after_subcommand() -> TestResult {
    let app =
        SeederApp::try_parse_from(["zebra-seeder", "start", "--config", "/path/to/config.toml"])?;
    assert_eq!(
        app.config.as_deref().and_then(std::path::Path::to_str),
        Some("/path/to/config.toml")
    );
    Ok(())
}
