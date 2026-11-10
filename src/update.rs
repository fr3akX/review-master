//! Updates for a review-master installed by its release installer
//! (`review-master-installer.sh`). The installer leaves a receipt with the
//! installed version; at most every few hours startup asks GitHub Releases for a
//! newer one and says so, and `review-master --update` installs it. Other builds
//! (cargo, a checkout) are left alone.

use axoupdater::{AxoUpdater, Version};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs};

pub const INSTALLER: &str =
    "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/fr3akX/review-master/releases/latest/download/review-master-installer.sh | sh";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const CHECK_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Serialize, Deserialize, Default)]
struct State {
    checked_at: u64,
    /// Newest release seen at the last check.
    latest: Option<String>,
}

fn state_file() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|d| d.join("review-master").join("update.json"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn current() -> Version {
    env!("CARGO_PKG_VERSION").parse().expect("package version is semver")
}

fn notice_for(latest: &str) -> Option<String> {
    let newer = latest.parse::<Version>().ok().filter(|v| *v > current())?;
    Some(format!("review-master {newer} is available (you have {}): run `review-master --update`", current()))
}

fn updater() -> Option<AxoUpdater> {
    let mut up = AxoUpdater::new_for("review-master");
    up.load_receipt().ok()?;
    Some(up)
}

/// A one-line notice when a newer release exists. Uses the last answer for a few
/// hours; returns `None` for builds not installed by the release installer.
pub async fn notice() -> Option<String> {
    if env::var_os("REVIEW_MASTER_NO_UPDATE").is_some_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
    let mut up = updater()?;
    let file = state_file()?;
    let state: State = fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if now().saturating_sub(state.checked_at) < CHECK_EVERY.as_secs() {
        return state.latest.as_deref().and_then(notice_for);
    }
    // Only builds the receipt belongs to are eligible; others stop before any request.
    let needed = tokio::time::timeout(CHECK_TIMEOUT, up.is_update_needed()).await.ok()?.ok()?;
    let latest = if needed { up.query_new_version().await.ok().flatten().map(Version::to_string) } else { None };
    let _ = fs::create_dir_all(file.parent()?);
    let _ = fs::write(&file, serde_json::to_vec(&State { checked_at: now(), latest: latest.clone() }).ok()?);
    latest.as_deref().and_then(notice_for)
}

/// `review-master --update`: install the latest release with its installer.
pub async fn update_now() -> Result<String, String> {
    let mut up = updater().ok_or_else(|| {
        format!("this review-master was not installed by the release installer; install it with:\n  {INSTALLER}")
    })?;
    up.enable_installer_output();
    let result = up.run().await.map_err(|e| format!("update failed: {e}"))?;
    if let Some(file) = state_file() {
        let _ = fs::remove_file(file); // the next start checks again
    }
    Ok(match result {
        Some(r) => format!(
            "updated review-master {} → {}",
            r.old_version.map_or_else(|| "?".to_owned(), |v| v.to_string()),
            r.new_version
        ),
        None => format!("review-master {} is up to date", current()),
    })
}

#[cfg(test)]
mod tests {
    use super::notice_for;

    #[test]
    fn only_newer_releases_are_announced() {
        assert!(notice_for("99.0.0").is_some_and(|m| m.contains("99.0.0") && m.contains("review-master --update")));
        assert_eq!(notice_for(env!("CARGO_PKG_VERSION")), None);
        assert_eq!(notice_for("0.0.1"), None);
        assert_eq!(notice_for("not-a-version"), None);
    }
}
