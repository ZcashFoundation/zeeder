//! Runtime build identity derived from Cargo and vergen metadata.

use std::sync::OnceLock;

/// Package version from `Cargo.toml`.
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

const PACKAGE_NAME: &str = env!("CARGO_PKG_NAME");
const UNKNOWN_GIT_SHA: &str = "unknown";

/// Return the version string shown by `--version`.
pub(crate) fn cli_version() -> &'static str {
    static CLI_VERSION: OnceLock<String> = OnceLock::new();

    CLI_VERSION.get_or_init(|| {
        short_git_sha().map_or_else(
            || VERSION.to_string(),
            |short_sha| format!("{VERSION} ({short_sha})"),
        )
    })
}

/// Return the full git SHA used by build-info metrics.
pub(crate) fn git_sha_label() -> &'static str {
    option_env!("VERGEN_GIT_SHA").unwrap_or(UNKNOWN_GIT_SHA)
}

/// Return the user agent sent during zebra-network handshakes.
pub(crate) fn user_agent() -> String {
    short_git_sha().map_or_else(
        || format!("{PACKAGE_NAME}/{VERSION}"),
        |short_sha| format!("{PACKAGE_NAME}/{VERSION} ({short_sha})"),
    )
}

fn short_git_sha() -> Option<&'static str> {
    option_env!("VERGEN_GIT_SHA").map(|sha| sha.get(..7).unwrap_or(sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_version_includes_package_version() {
        assert!(cli_version().contains(VERSION));
    }

    #[test]
    fn cli_version_includes_short_git_sha_when_available() {
        if let Some(sha) = option_env!("VERGEN_GIT_SHA") {
            let short_sha = sha.get(..7).unwrap_or(sha);
            assert!(cli_version().contains(short_sha));
        }
    }

    #[test]
    fn user_agent_identifies_package_version_and_git_sha() {
        let user_agent = user_agent();

        assert!(user_agent.starts_with(&format!("{PACKAGE_NAME}/{VERSION}")));
        if let Some(sha) = option_env!("VERGEN_GIT_SHA") {
            let short_sha = sha.get(..7).unwrap_or(sha);
            assert!(user_agent.contains(short_sha));
        }
    }

    #[test]
    fn git_sha_label_uses_full_git_sha_when_available() {
        assert_eq!(
            git_sha_label(),
            option_env!("VERGEN_GIT_SHA").unwrap_or(UNKNOWN_GIT_SHA)
        );
    }
}
