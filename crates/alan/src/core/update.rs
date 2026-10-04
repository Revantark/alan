//! Lazily checks GitHub Releases for a newer published Alan version.
use anyhow::Context as _;
use serde::Deserialize;
use std::time::Duration;

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/Revantark/alan/releases/latest";
const INSTALL_COMMAND: &str = "curl -fsSL https://github.com/Revantark/alan/releases/latest/download/alan-installer.sh | bash";

/// Fetch the latest published release version (leading `v` stripped).
pub async fn fetch_latest_version() -> anyhow::Result<String> {
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        reqwest::Client::new()
            .get(LATEST_RELEASE_URL)
            .header("User-Agent", format!("alan/{}", current_version()))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .context("requesting latest release")?
            .error_for_status()
            .context("latest release lookup failed")?
            .json::<LatestRelease>()
            .await
            .context("decoding latest release")
    })
    .await
    .context("timed out checking for updates")??;

    let tag = response.tag_name.trim().trim_start_matches('v');
    if tag.is_empty() {
        anyhow::bail!("empty tag name in latest release");
    }
    Ok(tag.to_owned())
}

/// True when `latest` is strictly newer than the compiled version.
fn is_newer(latest: &str, current: &str) -> bool {
    let (Some(latest), Some(current)) = (parse_version(latest), parse_version(current)) else {
        return false;
    };
    latest > current
}

pub fn update_notice(latest: &str) -> Option<String> {
    if !is_newer(latest, current_version()) {
        return None;
    }
    Some(format!(
        "alan v{latest} is available (current: {}) — update with: {INSTALL_COMMAND}",
        current_version()
    ))
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// Parse a version into three numeric components; `None` when malformed.
fn parse_version(version: &str) -> Option<[u64; 3]> {
    let version = version.trim().trim_start_matches('v');
    let mut parts = [0u64; 3];
    for (index, part) in version.split('.').enumerate() {
        let number = part.parse::<u64>().ok()?;
        parts[index] = number;
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_handles_v_prefix_and_missing_parts() {
        assert_eq!(parse_version("0.1.3"), Some([0, 1, 3]));
        assert_eq!(parse_version("v1.2"), Some([1, 2, 0]));
        assert_eq!(parse_version(" 2.0.0 "), Some([2, 0, 0]));
        assert_eq!(parse_version("0.1"), Some([0, 1, 0]));
    }

    #[test]
    fn parse_version_rejects_non_numeric() {
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("0.1.x"), None);
        assert_eq!(parse_version("0.2.0-rc1"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn is_newer_compares_numerically() {
        assert!(is_newer("0.1.4", "0.1.3"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.3", "0.1.3"));
        assert!(!is_newer("0.1.2", "0.1.3"));
        assert!(is_newer("0.2", "0.1.3"));
        assert!(!is_newer("0.2.0-rc1", "0.1.3"));
        assert!(!is_newer("garbage", "0.1.3"));
    }
}
