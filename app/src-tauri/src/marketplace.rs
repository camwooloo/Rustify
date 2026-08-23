//! The Spicetify Marketplace catalogue, read from the same places it reads.
//!
//! The Marketplace is a Spicetify app that lists what the community has
//! published: extensions, themes and apps found by GitHub topic, plus a file
//! of CSS snippets kept in its own repository. Each repository describes
//! itself in a `manifest.json`. None of that is Spotify-specific — it is
//! GitHub — so Rustify can show the same catalogue.
//!
//! What differs is what can be *used* here. A theme's colours apply to
//! Rustify directly, because Rustify's variables describe the same surfaces.
//! Everything else — the CSS half of a theme, snippets, extensions, apps —
//! is written against the official client's markup and APIs, so it is listed
//! and can be installed into a Spicetify setup, but it is never run here.
//! Fetching JavaScript from a stranger's repository and executing it next to
//! someone's account is not a feature.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

const SEARCH: &str = "https://api.github.com/search/repositories";
const RAW: &str = "https://raw.githubusercontent.com";
const SNIPPETS: &str =
    "https://raw.githubusercontent.com/spicetify/marketplace/main/resources/snippets.json";
const UA: &str = "Rustify-Marketplace";

/// Repositories to read per kind. The Marketplace pages through everything;
/// this takes the most-starred slice, which is what anyone scrolls anyway.
const REPOS: usize = 40;
const FETCH_AT_ONCE: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    /// extension, theme, app or snippet.
    pub kind: String,
    pub name: String,
    pub description: String,
    pub authors: Vec<String>,
    pub stars: u32,
    pub preview: Option<String>,
    /// owner/repo, for the link out and for installing.
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub url: Option<String>,
    pub updated: Option<String>,
    pub tags: Vec<String>,
    /// Path within the repository to the colour scheme file, for themes.
    pub schemes_path: Option<String>,
    /// Path to the file that is the extension or the theme's CSS.
    pub main_path: Option<String>,
    /// Snippets carry their CSS with them rather than pointing at a file.
    pub code: Option<String>,
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .context("building the marketplace client")
}

fn cache_path(dir: &PathBuf, kind: &str) -> PathBuf {
    dir.join(format!("marketplace-{kind}.json"))
}

fn topic(kind: &str) -> &'static str {
    match kind {
        "theme" => "spicetify-themes",
        "app" => "spicetify-apps",
        _ => "spicetify-extensions",
    }
}

/// Resolve a manifest's `preview` against the repository it came from.
fn absolute(repo: &str, branch: &str, path: &str) -> String {
    if path.starts_with("http") {
        path.to_string()
    } else {
        format!("{RAW}/{repo}/{branch}/{}", path.trim_start_matches('/'))
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        // Authors are objects; tags are plain strings.
                        .or_else(|| {
                            item.get("name")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Turn one manifest entry into a catalogue item.
fn item_from_manifest(entry: &Value, kind: &str, repo: &Repo) -> Option<Item> {
    let name = entry.get("name")?.as_str()?.to_string();

    let mut tags = strings(entry.get("tags"));
    // The Marketplace flags entries that pull in code from elsewhere. It is
    // the most useful thing on a card, so it is kept.
    if entry.get("include").is_some() {
        tags.push("external JS".to_string());
    }

    let authors = {
        let listed = strings(entry.get("authors"));
        if listed.is_empty() {
            vec![repo.owner.clone()]
        } else {
            listed
        }
    };

    Some(Item {
        kind: kind.to_string(),
        name,
        description: entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        authors,
        stars: repo.stars,
        preview: entry
            .get("preview")
            .and_then(Value::as_str)
            .map(|p| absolute(&repo.full_name, &repo.branch, p)),
        repo: Some(repo.full_name.clone()),
        branch: Some(repo.branch.clone()),
        url: Some(format!("https://github.com/{}", repo.full_name)),
        updated: repo.pushed.clone(),
        tags,
        schemes_path: entry
            .get("schemes")
            .and_then(Value::as_str)
            .map(str::to_string),
        main_path: entry
            .get("usercss")
            .or_else(|| entry.get("main"))
            .and_then(Value::as_str)
            .map(str::to_string),
        code: None,
    })
}

struct Repo {
    full_name: String,
    owner: String,
    branch: String,
    stars: u32,
    pushed: Option<String>,
}

async fn repos(client: &reqwest::Client, kind: &str) -> Result<Vec<Repo>> {
    let url = format!(
        "{SEARCH}?q=topic:{}&sort=stars&order=desc&per_page={REPOS}",
        topic(kind)
    );

    let body: Value = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for the catalogue")?
        .error_for_status()
        .context("GitHub refused the catalogue")?
        .json()
        .await
        .context("reading the catalogue")?;

    Ok(body
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|r| {
                    Some(Repo {
                        full_name: r.get("full_name")?.as_str()?.to_string(),
                        owner: r
                            .get("owner")
                            .and_then(|o| o.get("login"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                        branch: r
                            .get("default_branch")
                            .and_then(Value::as_str)
                            .unwrap_or("main")
                            .to_string(),
                        stars: r
                            .get("stargazers_count")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                        pushed: r
                            .get("pushed_at")
                            .and_then(Value::as_str)
                            .map(|d| d[..10.min(d.len())].to_string()),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn manifest(client: &reqwest::Client, kind: &str, repo: Repo) -> Vec<Item> {
    let url = format!("{RAW}/{}/{}/manifest.json", repo.full_name, repo.branch);
    let Ok(response) = client.get(&url).send().await else {
        return Vec::new();
    };
    if !response.status().is_success() {
        return Vec::new();
    }
    let Ok(body) = response.json::<Value>().await else {
        warn!("{}: manifest is not JSON", repo.full_name);
        return Vec::new();
    };

    // A manifest is a list of entries, or a single entry on its own.
    match &body {
        Value::Array(entries) => entries
            .iter()
            .filter_map(|e| item_from_manifest(e, kind, &repo))
            .collect(),
        entry => item_from_manifest(entry, kind, &repo)
            .into_iter()
            .collect(),
    }
}

async fn snippets(client: &reqwest::Client) -> Result<Vec<Item>> {
    let body: Vec<Value> = client
        .get(SNIPPETS)
        .send()
        .await
        .context("asking for the snippet list")?
        .error_for_status()
        .context("the snippet list was refused")?
        .json()
        .await
        .context("reading the snippet list")?;

    Ok(body
        .iter()
        .filter_map(|s| {
            Some(Item {
                kind: "snippet".to_string(),
                name: s.get("title")?.as_str()?.to_string(),
                description: s
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                authors: strings(s.get("authors")),
                stars: 0,
                preview: s
                    .get("preview")
                    .and_then(Value::as_str)
                    .map(|p| absolute("spicetify/marketplace", "main", p)),
                repo: Some("spicetify/marketplace".to_string()),
                branch: Some("main".to_string()),
                url: Some("https://github.com/spicetify/marketplace".to_string()),
                updated: None,
                tags: strings(s.get("tags")),
                schemes_path: None,
                main_path: None,
                code: s.get("code").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect())
}

fn read_cache(dir: &PathBuf, kind: &str) -> Option<Vec<Item>> {
    let raw = std::fs::read_to_string(cache_path(dir, kind)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(dir: &PathBuf, kind: &str, items: &[Item]) {
    if let Err(e) = std::fs::create_dir_all(dir)
        .and_then(|_| serde_json::to_vec(items).map_err(std::io::Error::other))
        .and_then(|bytes| std::fs::write(cache_path(dir, kind), bytes))
    {
        warn!("could not cache the {kind} catalogue: {e}");
    }
}

/// One kind of catalogue entry, from cache unless a refresh is asked for.
pub async fn catalogue(cache_dir: PathBuf, kind: String, refresh: bool) -> Result<Vec<Item>> {
    if !refresh {
        if let Some(cached) = read_cache(&cache_dir, &kind) {
            debug!("marketplace: {} {kind}s from cache", cached.len());
            return Ok(cached);
        }
    }

    let client = client()?;

    let mut items = if kind == "snippet" {
        snippets(&client).await?
    } else {
        let found = match repos(&client, &kind).await {
            Ok(found) => found,
            Err(e) => {
                return read_cache(&cache_dir, &kind).ok_or(e);
            }
        };

        let mut items = Vec::new();
        let mut queue = found.into_iter();
        loop {
            let batch: Vec<Repo> = queue.by_ref().take(FETCH_AT_ONCE).collect();
            if batch.is_empty() {
                break;
            }
            let mut running = Vec::with_capacity(batch.len());
            for repo in batch {
                let client = client.clone();
                let kind = kind.clone();
                running.push(tokio::spawn(
                    async move { manifest(&client, &kind, repo).await },
                ));
            }
            for task in running {
                if let Ok(found) = task.await {
                    items.extend(found);
                }
            }
        }
        items
    };

    if items.is_empty() {
        return read_cache(&cache_dir, &kind)
            .ok_or_else(|| anyhow!("nothing could be read from the catalogue"));
    }

    items.sort_by(|a, b| b.stars.cmp(&a.stars).then_with(|| a.name.cmp(&b.name)));
    debug!("marketplace: {} {kind}s fetched", items.len());
    write_cache(&cache_dir, &kind, &items);
    Ok(items)
}

/// A theme's colour schemes, fetched when someone actually opens it.
///
/// Fetching every theme's `color.ini` up front would be another request per
/// card for something most of them will never be asked for.
pub async fn schemes(repo: &str, branch: &str, path: &str) -> Result<Vec<crate::spicetify::Scheme>> {
    let url = absolute(repo, branch, path);
    let text = client()?
        .get(&url)
        .send()
        .await
        .context("fetching the colour scheme")?
        .error_for_status()
        .context("the colour scheme was refused")?
        .text()
        .await
        .context("reading the colour scheme")?;

    let parsed = crate::spicetify::parse_color_ini(&text);
    if parsed.is_empty() {
        return Err(anyhow!("no colours in {path}"));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::{absolute, item_from_manifest, strings, Repo};
    use serde_json::json;

    fn repo() -> Repo {
        Repo {
            full_name: "spicetify/spicetify-themes".into(),
            owner: "spicetify".into(),
            branch: "master".into(),
            stars: 6052,
            pushed: Some("2026-07-15".into()),
        }
    }

    #[test]
    fn relative_paths_resolve_against_the_repository() {
        assert_eq!(
            absolute("a/b", "main", "images/preview.png"),
            "https://raw.githubusercontent.com/a/b/main/images/preview.png"
        );
        // An address that is already absolute is left alone.
        assert_eq!(absolute("a/b", "main", "https://x/y.png"), "https://x/y.png");
    }

    #[test]
    fn authors_are_read_from_objects_and_tags_from_strings() {
        let value = json!([{ "name": "Mr Biscuit", "url": "https://github.com/x" }]);
        assert_eq!(strings(Some(&value)), vec!["Mr Biscuit"]);
        assert_eq!(strings(Some(&json!(["latest"]))), vec!["latest"]);
    }

    #[test]
    fn a_theme_entry_keeps_what_a_card_needs() {
        let entry = json!({
            "name": "SharkBlue",
            "description": "SharkBlue",
            "preview": "SharkBlue/screenshot.png",
            "usercss": "SharkBlue/user.css",
            "schemes": "SharkBlue/color.ini",
            "authors": [{ "name": "Mr Biscuit" }],
            "tags": ["latest"],
        });

        let item = item_from_manifest(&entry, "theme", &repo()).expect("an item");
        assert_eq!(item.name, "SharkBlue");
        assert_eq!(item.authors, vec!["Mr Biscuit"]);
        assert_eq!(item.stars, 6052);
        assert_eq!(item.schemes_path.as_deref(), Some("SharkBlue/color.ini"));
        assert_eq!(
            item.preview.as_deref(),
            Some("https://raw.githubusercontent.com/spicetify/spicetify-themes/master/SharkBlue/screenshot.png")
        );
    }

    /// Against the live catalogue, so what this parses is what is published.
    ///
    /// Ignored by default: a missing network should not fail the suite. Run
    /// with `cargo test -p spotify-rust-app -- --ignored`.
    #[tokio::test]
    #[ignore = "hits GitHub"]
    async fn the_theme_catalogue_reads_end_to_end() {
        let dir = std::env::temp_dir().join("rustify-market-test");
        let _ = std::fs::remove_dir_all(&dir);

        let items = super::catalogue(dir.clone(), "theme".into(), true)
            .await
            .expect("a catalogue");

        assert!(items.len() > 20, "got {} themes", items.len());

        // The most-starred repository is the official theme collection, and
        // its entries are the ones whose colours Rustify can use.
        let with_colours = items.iter().filter(|i| i.schemes_path.is_some()).count();
        assert!(with_colours > 10, "only {with_colours} themes carry colours");

        let previewed = items.iter().filter(|i| i.preview.is_some()).count();
        assert!(previewed > 10, "only {previewed} themes carry a preview");

        // And a real colour file parses into schemes.
        let theme = items
            .iter()
            .find(|i| i.schemes_path.is_some())
            .expect("a theme with colours");
        let schemes = super::schemes(
            theme.repo.as_deref().unwrap(),
            theme.branch.as_deref().unwrap(),
            theme.schemes_path.as_deref().unwrap(),
        )
        .await
        .expect("schemes");
        assert!(!schemes.is_empty());
        assert!(!schemes[0].colors.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[ignore = "hits GitHub"]
    async fn snippets_read_end_to_end() {
        let dir = std::env::temp_dir().join("rustify-market-test-snippets");
        let _ = std::fs::remove_dir_all(&dir);

        let items = super::catalogue(dir.clone(), "snippet".into(), true)
            .await
            .expect("snippets");

        assert!(items.len() > 50, "got {} snippets", items.len());
        assert!(items.iter().all(|i| i.code.is_some()), "a snippet is its CSS");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_entry_that_pulls_in_code_says_so() {
        let entry = json!({
            "name": "Comfy",
            "include": ["https://comfy-themes.github.io/theme.script.js"],
        });
        let item = item_from_manifest(&entry, "theme", &repo()).expect("an item");
        assert!(item.tags.iter().any(|t| t == "external JS"));
        // With no authors of its own, the repository's owner stands in.
        assert_eq!(item.authors, vec!["spicetify"]);
    }
}
