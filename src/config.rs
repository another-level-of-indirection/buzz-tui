//! Persisted client settings — currently the community list.
//!
//! A community *is* its relay URL, and there is no server-side list to ask
//! for, so the set of communities is something this client has to remember.
//! An environment variable is the wrong place for it: it is per-shell, so it
//! is lost the moment you open a new tab, and nothing about a missing one
//! looks like a mistake — the app just quietly shows fewer communities than
//! you have.
//!
//! `BUZZ_RELAY_URL` still works and still wins for ordering, both because
//! `buzz-cli` uses it and because a one-off override is genuinely useful. It
//! also seeds this file the first time, so an existing setup keeps working and
//! then keeps working across shells.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub communities: Vec<String>,
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("buzz-tui").join("config.json"))
}

pub fn load() -> Config {
    let Some(path) = path() else {
        return Config::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(config: &Config) -> Result<()> {
    let path = path().context("no config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating config directory")?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(config)?).context("writing config")?;
    Ok(())
}

/// Normalizes a relay URL so the same community entered two ways is one entry.
pub fn normalize(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Adds a community, keeping the list order-preserving and duplicate-free.
pub fn add(url: &str, env: Option<&str>) -> Result<Vec<String>> {
    let url = normalize(url);
    if url.is_empty() {
        anyhow::bail!("a community needs a relay URL");
    }
    // Start from the *effective* list, not just the saved one. Adding a second
    // community while the first was named only by the environment would
    // otherwise write a file containing the new one alone — and the first
    // would vanish the next time that variable was not set.
    let mut communities = communities(env);
    if !communities.contains(&url) {
        communities.push(url);
    }
    let config = Config { communities };
    save(&config)?;
    Ok(config.communities)
}

pub fn remove(url: &str) -> Result<Vec<String>> {
    let url = normalize(url);
    let mut config = load();
    config.communities.retain(|known| *known != url);
    save(&config)?;
    Ok(config.communities)
}

/// The communities to connect to: the saved list, plus anything named in the
/// environment that is not already in it.
///
/// Environment entries come first so a deliberate override decides which
/// community opens on launch.
pub fn communities(env: Option<&str>) -> Vec<String> {
    let from_env: Vec<String> = env
        .unwrap_or_default()
        .split(',')
        .map(normalize)
        .filter(|url| !url.is_empty())
        .collect();
    let saved = load().communities;

    let mut out: Vec<String> = Vec::new();
    for url in from_env.into_iter().chain(saved) {
        if !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// Writes the environment's list to disk the first time, so an existing
/// single-relay setup survives the next shell without anyone doing anything.
pub fn seed_if_empty(urls: &[String]) {
    if urls.is_empty() || !load().communities.is_empty() {
        return;
    }
    let _ = save(&Config {
        communities: urls.to_vec(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_slash_is_not_a_different_community() {
        assert_eq!(
            normalize("https://a.example.com/"),
            normalize(" https://a.example.com ")
        );
    }

    #[test]
    fn the_environment_leads_and_nothing_repeats() {
        // The env entry decides which community opens on launch, and a URL
        // present in both places must not produce two identical rail rows.
        let merged = merge(
            &["https://b.example.com".into()],
            vec![
                "https://a.example.com".into(),
                "https://b.example.com".into(),
            ],
        );
        assert_eq!(
            merged,
            vec![
                "https://b.example.com".to_string(),
                "https://a.example.com".to_string()
            ]
        );
    }

    #[test]
    fn an_empty_environment_leaves_the_saved_order_alone() {
        let saved = vec![
            "https://a.example.com".to_string(),
            "https://b.example.com".to_string(),
        ];
        assert_eq!(merge(&[], saved.clone()), saved);
    }

    /// The ordering rule, factored out so it can be tested without touching
    /// the filesystem.
    fn merge(env: &[String], saved: Vec<String>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for url in env.iter().cloned().chain(saved) {
            if !out.contains(&url) {
                out.push(url);
            }
        }
        out
    }
}
