use crate::commands::SeederApp;
use clap::Parser;

#[test]
fn test_cli_parsing_default() {
    let args = vec!["zebra-seeder", "start"];
    let app = SeederApp::try_parse_from(args).expect("should parse");
    match app.command {
        crate::commands::Commands::Start => {}
    }
    assert!(app.config.is_none());
}

#[test]
fn test_cli_parsing_with_config() {
    let args = vec!["zebra-seeder", "--config", "/path/to/config.toml", "start"];
    let app = SeederApp::try_parse_from(args).expect("should parse");
    assert_eq!(
        app.config.unwrap().to_str().unwrap(),
        "/path/to/config.toml"
    );
}

#[test]
fn test_cli_parsing_global_arg_after_subcommand() {
    // `--config` is global, so it parses whether it comes before or after the
    // subcommand.
    let args = vec!["zebra-seeder", "start", "--config", "/path/to/config.toml"];
    let app = SeederApp::try_parse_from(args).expect("should parse");
    assert_eq!(
        app.config.unwrap().to_str().unwrap(),
        "/path/to/config.toml"
    );
}
