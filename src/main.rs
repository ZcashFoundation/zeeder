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
            "zeeder-{name}-{}-{timestamp}.env",
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

    #[test]
    fn direct_dependencies_disable_default_features() -> TestResult {
        let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let manifest = fs::read_to_string(manifest_path)?;
        let manifest: toml::Table = manifest.parse()?;

        for section_name in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let dependencies = manifest
                .get(section_name)
                .and_then(toml::Value::as_table)
                .ok_or_else(|| color_eyre::eyre::eyre!("{section_name} must be a table"))?;

            for (dependency_name, dependency) in dependencies {
                let dependency_table = dependency.as_table().ok_or_else(|| {
                    color_eyre::eyre::eyre!(
                        "{section_name}.{dependency_name} must use table form with default-features = false"
                    )
                })?;
                let default_features = dependency_table
                    .get("default-features")
                    .and_then(toml::Value::as_bool)
                    .ok_or_else(|| {
                        color_eyre::eyre::eyre!(
                            "{section_name}.{dependency_name} must set default-features = false"
                        )
                    })?;

                assert!(
                    !default_features,
                    "{section_name}.{dependency_name} must set default-features = false"
                );
            }
        }

        Ok(())
    }

    #[test]
    fn development_docs_do_not_reference_missing_changelog() {
        let development_docs = include_str!("../docs/development.md");
        let changelog_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("CHANGELOG.md");

        assert!(
            !development_docs.contains("CHANGELOG.md") || changelog_path.exists(),
            "docs/development.md should not reference CHANGELOG.md unless it exists"
        );
    }

    #[test]
    fn readme_status_matches_unreleased_project_state() {
        let readme = include_str!("../README.md");

        assert!(
            readme.contains("**Current State**: Pre-release."),
            "README status should describe the project as pre-release"
        );
        assert!(
            !readme.contains("Ready for production testing"),
            "README should not claim production-testing readiness before release"
        );
    }

    #[test]
    fn markdown_local_links_point_to_existing_files() -> TestResult {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let docs = [
            "README.md",
            "CONTEXT.md",
            "docs/README.md",
            "docs/architecture.md",
            "docs/development.md",
            "docs/operations.md",
            "docs/adr/0001-zebra-network.md",
            "docs/adr/0002-hickory-dns.md",
            "docs/adr/0003-rate-limiting.md",
            "docs/adr/0004-peer-servability.md",
        ];

        for doc in docs {
            let doc_path = manifest_dir.join(doc);
            let markdown = fs::read_to_string(&doc_path)?;
            let doc_dir = doc_path
                .parent()
                .ok_or_else(|| color_eyre::eyre::eyre!("{doc} should have a parent directory"))?;

            for link in local_markdown_links(&markdown) {
                let target_path = doc_dir.join(link);
                assert!(
                    target_path.exists(),
                    "{doc} links to missing local file `{link}`"
                );
            }
        }

        Ok(())
    }

    fn local_markdown_links(markdown: &str) -> Vec<&str> {
        markdown
            .match_indices("](")
            .filter_map(|(start, _)| {
                let link_start = start + 2;
                let link_end = markdown[link_start..].find(')')? + link_start;
                let link = &markdown[link_start..link_end];
                let file_target = link.split_once('#').map_or(link, |(file, _)| file);

                if file_target.is_empty()
                    || file_target.starts_with("http://")
                    || file_target.starts_with("https://")
                    || file_target.starts_with("mailto:")
                {
                    None
                } else {
                    Some(file_target)
                }
            })
            .collect()
    }
}
