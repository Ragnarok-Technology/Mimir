//! Cross-platform GitHub Releases semantic-version update checker.

use serde::Deserialize;

pub const GITHUB_REPO: &str = "Ragnarok-Technology/Mimir";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl SemVer {
    pub fn parse(raw: &str) -> Self {
        let clean = raw.trim().trim_start_matches(['v', 'V']);
        let mut parts = clean.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        Self {
            major: parts.next().unwrap_or(0),
            minor: parts.next().unwrap_or(0),
            patch: parts.next().unwrap_or(0),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReleaseInfo {
    tag_name: String,
    html_url: String,
}

#[derive(Debug, Clone)]
pub enum UpdateStatus {
    UpToDate(String),
    Available { version: String, url: String },
    Error(String),
}

pub fn check_for_updates(current_version: &str) -> UpdateStatus {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let resp = ureq::get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .header("User-Agent", &format!("Mimir-Updater/{current_version}"))
        .call();

    match resp {
        Ok(mut r) => match r.body_mut().read_to_string() {
            Ok(raw) => match serde_json::from_str::<ReleaseInfo>(&raw) {
                Ok(info) => {
                    if SemVer::parse(&info.tag_name) > SemVer::parse(current_version) {
                        UpdateStatus::Available {
                            version: info.tag_name,
                            url: info.html_url,
                        }
                    } else {
                        UpdateStatus::UpToDate(current_version.to_string())
                    }
                }
                Err(e) => UpdateStatus::Error(format!("invalid release payload: {e}")),
            },
            Err(e) => UpdateStatus::Error(format!("failed to read release body: {e}")),
        },
        Err(e) => UpdateStatus::Error(format!("no published releases found ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compares_correctly() {
        assert!(SemVer::parse("v0.2.0") > SemVer::parse("0.1.0"));
        assert!(SemVer::parse("1.0.0") > SemVer::parse("0.9.9"));
        assert_eq!(SemVer::parse("v0.1.0"), SemVer::parse("0.1.0"));
    }
}
