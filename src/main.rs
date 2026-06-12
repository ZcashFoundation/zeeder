//! A DNS seeder for the Zcash network.
//!
//! Crawls the network with `zebra-network` and serves the addresses of
//! recently-live, version-current peers over DNS (via Hickory DNS) so new
//! nodes can bootstrap.

use color_eyre::eyre::{Context, Result};
use std::path::Path;

const DOTENV_PATH: &str = ".env";

mod build_info;
mod commands;
mod config;
mod crawl;
mod dns;
mod metrics;
mod seeder;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    load_dotenv_from_path(Path::new(DOTENV_PATH))?;
    commands::SeederApp::run().await
}

fn load_dotenv_from_path(path: &Path) -> Result<()> {
    match dotenvy::from_path(path) {
        Ok(()) => Ok(()),
        Err(error) if error.not_found() => Ok(()),
        Err(error) => Err(error).wrap_err_with(|| format!("failed to load {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    type TestResult<T = ()> = color_eyre::Result<T>;

    fn dotenv_path(name: &str) -> TestResult<PathBuf> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(std::env::temp_dir().join(format!(
            "zebra-seeder-{name}-{}-{timestamp}.env",
            std::process::id()
        )))
    }

    #[test]
    fn missing_dotenv_file_is_allowed() -> TestResult {
        let path = dotenv_path("missing")?;

        load_dotenv_from_path(&path)
    }

    #[test]
    fn malformed_dotenv_file_is_rejected() -> TestResult {
        let path = dotenv_path("malformed")?;
        fs::write(&path, "<><><>")?;

        let result = load_dotenv_from_path(&path);
        fs::remove_file(path)?;

        assert!(result.is_err(), "malformed .env should fail startup");
        Ok(())
    }
}
