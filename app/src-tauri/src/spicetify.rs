//! Spicetify colour schemes, read straight from the theme repository.
//!
//! Spicetify is a patcher for the official client: it injects a theme's
//! `user.css` into Spotify's own markup and exposes the theme's palette as
//! `--spice-*` variables. The palette half of that travels perfectly — it is
//! just colours, and Rustify's own variables already describe the same
//! surfaces, because they were taken from the same client. The `user.css`
//! half does not: it is written against Spotify's class names, none of which
//! exist here, so it is deliberately not fetched.
//!
//! Nothing here needs Spicetify to be installed. The themes are read from the
//! same repository its marketplace lists, cached on disk, and parsed into
//! plain colours — no CSS from the network ever reaches the page.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

const TREE: &str =
    "https://api.github.com/repos/spicetify/spicetify-themes/git/trees/master?recursive=1";
const RAW: &str = "https://raw.githubusercontent.com/spicetify/spicetify-themes/master/";
const UA: &str = "Rustify-Themes";
const CACHE_FILE: &str = "spicetify-themes.json";

/// How many colour files to fetch at once.
///
/// The catalogue is around forty small files. Fetching them one at a time
/// takes long enough to feel broken; fetching all of them at once is rude to
/// a host that is doing us a favour.
const FETCH_AT_ONCE: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub schemes: Vec<Scheme>,
    /// Installed on this machine rather than read from the repository.
    #[serde(default)]
    pub local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scheme {
    pub name: String,
    /// Spicetify's colour keys, without the `--spice-` prefix: `main`,
    /// `text`, `button`, `sidebar` and so on. Kept as a map rather than a
    /// struct because themes invent keys, and an unknown one should be
    /// carried through rather than dropped.
    pub colors: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct TreeResponse {
    #[serde(default)]
    tree: Vec<TreeEntry>,
}

#[derive(Deserialize)]
struct TreeEntry {
    #[serde(default)]
    path: String,
}

/// Parse a `color.ini`.
///
/// The format is INI: a section per scheme, one `key = value` per colour.
/// Values are hex with or without the leading `#`, and anything that is not a
/// colour is dropped rather than rejected — these files are written by hand,
/// and one stray line should not cost the reader a whole theme.
fn parse_color_ini(text: &str) -> Vec<Scheme> {
    let mut schemes: Vec<Scheme> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }

        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            schemes.push(Scheme {
                name: name.trim().to_string(),
                colors: BTreeMap::new(),
            });
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(scheme) = schemes.last_mut() else {
            continue;
        };

        // A `;` starts a comment. A `#` does not — it prefixes the colour.
        let value = value.split(';').next().unwrap_or("").trim();
        let value = value.trim_start_matches('#').trim();

        let is_hex = matches!(value.len(), 3 | 6 | 8)
            && value.chars().all(|c| c.is_ascii_hexdigit());
        if is_hex {
            scheme
                .colors
                .insert(key.trim().to_ascii_lowercase(), value.to_ascii_lowercase());
        }
    }

    schemes.retain(|s| !s.colors.is_empty());
    schemes
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .context("building the theme client")
}

/// Every theme directory in the repository that carries a `color.ini`.
async fn theme_names(client: &reqwest::Client) -> Result<Vec<String>> {
    let tree: TreeResponse = client
        .get(TREE)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for the theme list")?
        .error_for_status()
        .context("GitHub refused the theme list")?
        .json()
        .await
        .context("reading the theme list")?;

    let mut names: Vec<String> = tree
        .tree
        .into_iter()
        .filter_map(|entry| {
            // Top level only: `Theme/color.ini`. Deeper matches are a theme's
            // own examples and variants, which the marketplace does not list.
            let (dir, file) = entry.path.split_once('/')?;
            (file == "color.ini" && !dir.contains('/') && !dir.starts_with('.'))
                .then(|| dir.to_string())
        })
        .collect();

    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    Ok(names)
}

async fn fetch_theme(client: &reqwest::Client, name: &str) -> Option<Theme> {
    let url = format!("{RAW}{name}/color.ini");
    let text = client.get(&url).send().await.ok()?.text().await.ok()?;

    let schemes = parse_color_ini(&text);
    if schemes.is_empty() {
        warn!("{name}: no usable colour schemes");
        return None;
    }
    Some(Theme {
        name: name.to_string(),
        schemes,
        local: false,
    })
}

/// Themes belonging to a Spicetify install on this machine.
///
/// Spicetify keeps them under its config directory, one folder per theme,
/// exactly as the repository does — so the same parser reads both. Nothing
/// here requires Spicetify: with no install these directories do not exist
/// and the catalogue is simply the published one.
fn local_themes() -> Vec<Theme> {
    let roots = ["APPDATA", "LOCALAPPDATA", "USERPROFILE"];
    let mut themes = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for root in roots {
        let Ok(base) = std::env::var(root) else {
            continue;
        };
        // `.spicetify` is where newer versions keep the config; older ones
        // use a `spicetify` folder under AppData.
        for dir in ["spicetify/Themes", ".spicetify/Themes"] {
            let Ok(entries) = std::fs::read_dir(PathBuf::from(&base).join(dir)) else {
                continue;
            };

            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if seen.contains(&name) {
                    continue;
                }

                let Ok(text) = std::fs::read_to_string(entry.path().join("color.ini")) else {
                    continue;
                };
                let schemes = parse_color_ini(&text);
                if schemes.is_empty() {
                    continue;
                }

                debug!("local theme: {name}");
                seen.push(name.clone());
                themes.push(Theme {
                    name,
                    schemes,
                    local: true,
                });
            }
        }
    }

    themes.sort_by_key(|t| t.name.to_lowercase());
    themes
}

fn read_cache(dir: &Path) -> Option<Vec<Theme>> {
    let raw = std::fs::read_to_string(dir.join(CACHE_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(dir: &Path, themes: &[Theme]) {
    if let Err(e) = std::fs::create_dir_all(dir)
        .and_then(|_| serde_json::to_vec(themes).map_err(std::io::Error::other))
        .and_then(|bytes| std::fs::write(dir.join(CACHE_FILE), bytes))
    {
        warn!("could not cache the theme catalogue: {e}");
    }
}

/// The theme catalogue, from cache when there is one.
///
/// `refresh` forces a fetch. Without it the cache answers, which keeps the
/// gallery instant and working with no network at all — the colours never go
/// stale in any way that matters.
pub async fn catalogue(cache_dir: PathBuf, refresh: bool) -> Result<Vec<Theme>> {
    // Installed themes are read every time. They are local files that cost
    // nothing to look at, and a theme someone just installed should appear
    // without having to refresh a catalogue it is not part of.
    let local = local_themes();
    let with_local = |mut themes: Vec<Theme>| {
        themes.retain(|t| !local.iter().any(|l| l.name == t.name));
        let mut all = local.clone();
        all.extend(themes);
        all
    };

    if !refresh {
        if let Some(cached) = read_cache(&cache_dir) {
            debug!("theme catalogue: {} themes from cache", cached.len());
            return Ok(with_local(cached));
        }
    }

    let client = client()?;
    let names = match theme_names(&client).await {
        Ok(names) => names,
        Err(e) => {
            // Falling back to a stale cache beats an empty gallery.
            return match read_cache(&cache_dir) {
                Some(cached) => {
                    warn!("using the cached catalogue: the fetch failed");
                    Ok(with_local(cached))
                }
                // Installed themes alone still make a catalogue worth showing.
                None if !local.is_empty() => Ok(local),
                None => Err(e),
            };
        }
    };

    let mut themes: Vec<Theme> = Vec::with_capacity(names.len());
    for batch in names.chunks(FETCH_AT_ONCE) {
        let mut running = Vec::with_capacity(batch.len());
        for name in batch {
            let client = client.clone();
            let name = name.clone();
            running.push(tokio::spawn(
                async move { fetch_theme(&client, &name).await },
            ));
        }
        for task in running {
            if let Ok(Some(theme)) = task.await {
                themes.push(theme);
            }
        }
    }

    if themes.is_empty() {
        return match read_cache(&cache_dir) {
            Some(cached) => Ok(with_local(cached)),
            None if !local.is_empty() => Ok(local),
            None => Err(anyhow!("no themes could be read")),
        };
    }

    debug!("theme catalogue: {} themes fetched", themes.len());
    // Only the fetched half is cached: the local half is read from disk
    // anyway, and caching it would resurrect themes after an uninstall.
    write_cache(&cache_dir, &themes);
    Ok(with_local(themes))
}

#[cfg(test)]
mod tests {
    use super::parse_color_ini;

    #[test]
    fn reads_a_scheme_per_section() {
        let ini = "\
; a comment
[Mocha]
text          = cdd6f4
subtext       = #a6adc8
main          = 1e1e2e
button        = 89b4fa ; trailing note

[Latte]
text = 4c4f69
main = eff1f5
";
        let schemes = parse_color_ini(ini);
        assert_eq!(schemes.len(), 2);
        assert_eq!(schemes[0].name, "Mocha");
        // The leading # and the trailing comment both come off.
        assert_eq!(schemes[0].colors["subtext"], "a6adc8");
        assert_eq!(schemes[0].colors["button"], "89b4fa");
        assert_eq!(schemes[1].name, "Latte");
    }

    #[test]
    fn anything_that_is_not_a_colour_is_dropped() {
        let schemes = parse_color_ini("[X]\ntext = ffffff\nname = Dribbblish\nurl = a.com\n");
        assert_eq!(schemes.len(), 1);
        assert_eq!(schemes[0].colors.len(), 1);
        assert!(schemes[0].colors.contains_key("text"));
    }

    #[test]
    fn a_section_with_no_colours_is_not_a_scheme() {
        assert!(parse_color_ini("[Empty]\nname = nothing\n").is_empty());
    }
}
