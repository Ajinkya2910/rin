// src/bioc_releases.rs — Bioconductor release lookup
//
// Replaces the hardcoded R→Bioc match block. Loads the Bioc release list
// from disk cache (if fresh), else fetches Bioconductor's config.yaml,
// else falls back to the TOML shipped inside the binary.
//
// PYTHON ANALOGY: this is like a requests.get() with disk caching and
// a bundled fallback — think functools.lru_cache + a try/except around
// network calls + importlib.resources for the shipped default.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60); // 7 days
const BIOC_CONFIG_URL: &str = "https://bioconductor.org/config.yaml";

// RUST CONCEPT: include_str!
// Reads the file AT COMPILE TIME and embeds its contents as a &'static str
// in the binary. So even if a user has no network and an empty cache,
// they always have this fallback. Like Python's importlib.resources.
const SHIPPED_FALLBACK: &str = include_str!("../data/bioc_releases.toml");

/// One row from the release list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub bioc: String,
    pub r: String,
}

/// The full list. Stored newest-Bioc-first after parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseList {
    pub release: Vec<Release>,
}

impl ReleaseList {
    fn from_toml(s: &str) -> Result<Self> {
        toml::from_str(s).context("Failed to parse bioc_releases.toml")
    }

    fn to_toml(&self) -> Result<String> {
        toml::to_string(self).context("Failed to serialize bioc releases")
    }

    /// Parse Bioconductor's config.yaml. We only care about one field:
    /// `r_ver_for_bioc_ver: { "3.20": "4.4", "3.21": "4.5", ... }`
    /// All other fields are ignored (serde skips unknown keys by default).
    fn from_bioc_yaml(s: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Config {
            r_ver_for_bioc_ver: HashMap<String, String>,
        }
        let cfg: Config = serde_yaml::from_str(s)
            .context("Failed to parse Bioconductor config.yaml")?;

        let mut releases: Vec<Release> = cfg
            .r_ver_for_bioc_ver
            .into_iter()
            .map(|(bioc, r)| Release { bioc, r })
            .collect();

        // Sort newest Bioc first. Numeric, not lexicographic —
        // "3.10" must rank higher than "3.9".
        releases.sort_by(|a, b| parse_version(&b.bioc).cmp(&parse_version(&a.bioc)));

        Ok(ReleaseList { release: releases })
    }
}

/// Parse a "X.Y" string into (X, Y) for ordering. Bad input sorts last.
fn parse_version(v: &str) -> (u32, u32) {
    let mut p = v.split('.');
    let major = p.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor = p.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor)
}

/// Disk cache location. Lives alongside the GitHub tarball cache.
fn cache_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = PathBuf::from(home).join(".rin").join("cache");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("bioc_releases.toml"))
}

/// Load from disk cache if it exists AND is younger than CACHE_MAX_AGE.
/// Any failure (missing, stale, corrupt) returns None — caller falls through.
fn load_fresh_cache() -> Option<ReleaseList> {
    let path = cache_path().ok()?;
    let meta = std::fs::metadata(&path).ok()?;
    let modified = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age > CACHE_MAX_AGE {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    ReleaseList::from_toml(&content).ok()
}

/// Hit the network. On success, write to disk cache (best-effort —
/// a read-only HOME on HPC shouldn't break the resolve).
async fn fetch_and_cache() -> Result<ReleaseList> {
    let yaml = reqwest::get(BIOC_CONFIG_URL)
        .await
        .context("Failed to reach bioconductor.org")?
        .text()
        .await?;

    let list = ReleaseList::from_bioc_yaml(&yaml)?;

    if let Ok(path) = cache_path() {
        if let Ok(text) = list.to_toml() {
            let _ = std::fs::write(&path, text); // best-effort
        }
    }

    Ok(list)
}

/// The embedded TOML. Always available, never fails (modulo a build-time
/// typo we'd catch in tests).
fn shipped_fallback() -> Result<ReleaseList> {
    ReleaseList::from_toml(SHIPPED_FALLBACK)
}

/// Cache → network → shipped. Public so other modules can introspect if needed.
pub async fn load() -> Result<ReleaseList> {
    if let Some(list) = load_fresh_cache() {
        return Ok(list);
    }
    match fetch_and_cache().await {
        Ok(list) => Ok(list),
        Err(_) => shipped_fallback(),
    }
}

/// Pick the Bioconductor release for an installed R version.
///
/// Algorithm (mirrors BiocManager):
///   1. Reduce installed R to (major, minor) — patch version doesn't matter.
///   2. Walk the release list newest-first.
///   3. Return the first Bioc whose required R ≤ installed R.
pub async fn pick_for_r(r_version: &str) -> Result<String> {
    let list = load().await?;
    let user_r = parse_version(r_version);

    for rel in &list.release {
        let rel_r = parse_version(&rel.r);
        if rel_r <= user_r {
            return Ok(rel.bioc.clone());
        }
    }

    anyhow::bail!(
        "No Bioconductor release found for R {}. Your R version may be too old. \
         Supported: see https://bioconductor.org/config.yaml",
        r_version
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_fallback_parses() {
        let list = shipped_fallback().expect("shipped TOML must parse");
        assert!(!list.release.is_empty(), "shipped list must not be empty");
    }

    #[test]
    fn parse_version_numeric() {
        // "3.10" > "3.9" numerically, even though "3.10" < "3.9" lexicographically.
        assert!(parse_version("3.10") > parse_version("3.9"));
    }

    // Logic test against the shipped data (no network).
    #[tokio::test]
    async fn r_4_4_picks_latest_compatible() {
        let list = shipped_fallback().unwrap();
        let user_r = parse_version("4.4.1");
        let pick = list
            .release
            .iter()
            .find(|r| parse_version(&r.r) <= user_r)
            .map(|r| r.bioc.clone());
        // Newest Bioc paired with R 4.4 in the shipped list is 3.20.
        assert_eq!(pick.as_deref(), Some("3.20"));
    }
#[tokio::test]
#[ignore]  // skip in normal `cargo test` — only run on demand
async fn live_fetch_works() {
    let list = fetch_and_cache().await.expect("network fetch failed");
    println!("Fetched {} Bioc releases", list.release.len());
    for rel in list.release.iter().take(5) {
        println!("  Bioc {} → R {}", rel.bioc, rel.r);
    }
}
}