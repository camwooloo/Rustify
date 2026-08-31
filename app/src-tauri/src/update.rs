//! Update check against GitHub releases.
//!
//! Rustify ships as two executables (window + daemon), so this deliberately
//! does *not* hot-swap the running binary the way a single-file tool can.
//! It downloads the published installer and hands off to it, which replaces
//! both halves and keeps the installed state consistent.
//!
//! The handoff is silent. Nobody wants a setup wizard as the price of a bug
//! fix, so the installer runs with no window of its own, closes Rustify,
//! swaps the files and starts it again. Because Rustify installs per user
//! there is no elevation prompt either, which is what makes a single click
//! enough.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

const REPO: &str = "camwooloo/Rustify";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const UA: &str = "Rustify-Updater";

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub notes: String,
}

#[derive(Deserialize)]
struct Release {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
}

/// Compare dotted version strings ("0.3.1" > "0.3.0").
fn is_newer(remote: &str, current: &str) -> bool {
    let parse = |s: &str| {
        s.trim_start_matches('v')
            .split('.')
            .filter_map(|p| p.trim().parse::<u32>().ok())
            .collect::<Vec<_>>()
    };
    let (a, b) = (parse(remote), parse(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// Ask GitHub for the latest release, if it is newer than this build.
///
/// Returns `None` for "nothing to do", including the perfectly normal cases
/// of no network and a repo with no releases yet.
pub async fn check() -> Option<UpdateInfo> {
    let client = reqwest::Client::builder().user_agent(UA).build().ok()?;

    let release: Release = client
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    if release.draft || release.prerelease {
        return None;
    }

    let version = release.tag_name.trim_start_matches('v').to_string();
    if !is_newer(&version, CURRENT) {
        debug!("already on the latest version ({CURRENT})");
        return None;
    }

    // Each release carries one download per platform, so pick the one this
    // build can actually use rather than whatever is listed first.
    let asset = release.assets.iter().find(|a| {
        let name = a.name.to_ascii_lowercase();
        if cfg!(target_os = "windows") {
            name.ends_with("-setup.exe") || name.ends_with(".msi")
        } else if cfg!(target_os = "macos") {
            name.ends_with(".dmg")
        } else {
            name.ends_with(".appimage")
        }
    });

    // A release is published before its builds finish uploading, so for a few
    // minutes there is a newer version with nothing to download. Saying
    // "up to date" then is a lie — the version is reported with no url, and
    // whoever asked is told to come back shortly.
    let Some(asset) = asset else {
        info!("v{version} is out, but has no download for this platform yet");
        return Some(UpdateInfo {
            version,
            url: String::new(),
            notes: release.body.clone(),
        });
    };

    info!("update available: v{version}");
    Some(UpdateInfo {
        version,
        url: asset.browser_download_url.clone(),
        notes: release.body.clone(),
    })
}

/// Is this a bare filename we are willing to write into the temp directory?
///
/// The name comes from a URL, so it is checked rather than trusted: letters,
/// digits, dots, dashes and underscores only, which leaves no way to point at
/// a directory or at anything but an installer.
fn is_plain_installer_name(name: &str) -> bool {
    name.ends_with(".exe")
        && name.len() < 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Silent install: no window, no clicks, close Rustify, start it again.
///
/// `/S` is NSIS's own silent switch, and Tauri's installer reads the other
/// two: `/UPDATE` to install over itself rather than as a first install, and
/// `/R` to relaunch the app afterwards.
const INSTALL_SILENTLY: [&str; 3] = ["/S", "/UPDATE", "/R"];

/// Can an update install itself here, or does it need a human?
///
/// Only the Windows installer can: NSIS takes switches for a silent install
/// and knows where the app lives. A dmg has to be opened and dragged, and an
/// AppImage is a file the person chose where to put — neither is something to
/// do behind someone's back.
pub fn installs_itself() -> bool {
    cfg!(target_os = "windows")
}

/// Download the installer and hand off to it.
///
/// `on_progress` is called with a whole percentage as the download runs, and
/// only when that number changes: it is driving a progress bar, and there is
/// no point waking the webview for a fraction of a percent.
///
/// This does not exit the app afterwards. The installer stops Rustify itself
/// as its first step, which is both the moment the window should disappear
/// and the only one that is safe — quitting any earlier would leave nothing
/// on screen while the download's replacement is still being written.
pub async fn apply(url: &str, on_progress: impl Fn(u8)) -> Result<()> {
    if !installs_itself() {
        return Err(anyhow!(
            "this build cannot install its own updates; open the download instead"
        ));
    }

    // Only ever fetch from the project's own release host.
    if !url.starts_with("https://github.com/") && !url.starts_with("https://objects.githubusercontent.com/")
    {
        return Err(anyhow!("refusing to download from an unexpected host"));
    }

    let client = reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .context("building the update client")?;

    let mut response = client
        .get(url)
        .send()
        .await
        .context("downloading the update")?
        .error_for_status()
        .context("the download was rejected")?;

    let total = response.content_length().unwrap_or(0);
    let mut bytes: Vec<u8> = Vec::with_capacity(total as usize);
    let mut shown = 0u8;

    while let Some(chunk) = response.chunk().await.context("reading the update")? {
        bytes.extend_from_slice(&chunk);
        if total > 0 {
            let pct = ((bytes.len() as u64 * 100 / total) as u8).min(100);
            if pct != shown {
                shown = pct;
                on_progress(pct);
            }
        }
    }

    // A real build is well over a megabyte; this catches error pages saved
    // as if they were the installer.
    if bytes.len() < 500_000 {
        return Err(anyhow!("the downloaded file looks too small to be real"));
    }
    // A truncated download would install a broken Rustify, so treat a short
    // read as a failure rather than handing the installer half a file.
    if total > 0 && bytes.len() as u64 != total {
        return Err(anyhow!("the download ended early"));
    }

    // Every download gets the release's own filename rather than one fixed
    // name. A silent installer stays alive for as long as the Rustify it
    // relaunched, and Windows will not let anything overwrite the image of a
    // running process — so a second update in one sitting would otherwise be
    // refused the moment it tried to write.
    let dir = std::env::temp_dir();
    let name = url
        .rsplit('/')
        .next()
        .filter(|n| is_plain_installer_name(n))
        .unwrap_or("Rustify-update-setup.exe");
    let path = dir.join(name);

    // Installers left by previous updates are dead weight once their own
    // install is done. Whatever is still locked simply stays.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let stale = entry.file_name();
            let stale = stale.to_string_lossy();
            if stale != name && stale.starts_with("Rustify") && stale.ends_with("setup.exe") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    std::fs::write(&path, &bytes)
        .with_context(|| format!("writing {}", path.display()))?;

    info!("installing the update silently");
    std::process::Command::new(&path)
        .args(INSTALL_SILENTLY)
        .spawn()
        .with_context(|| format!("launching {}", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_newer, is_plain_installer_name};

    #[test]
    fn only_bare_installer_names_are_written_to_temp() {
        assert!(is_plain_installer_name("Rustify_0.4.1_x64-setup.exe"));
        assert!(!is_plain_installer_name("notes.txt"));
        // Nothing that could climb out of the temp directory.
        assert!(!is_plain_installer_name("../../Windows/System32/evil.exe"));
        assert!(!is_plain_installer_name("C:/Windows/System32/evil.exe"));
    }

    #[test]
    fn version_comparison_handles_the_v_prefix_and_short_forms() {
        assert!(is_newer("0.3.1", "0.3.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.2", "0.1.9"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.9", "0.3.0"));
        // Garbage must never look like an upgrade.
        assert!(!is_newer("", "0.1.0"));
    }
}
