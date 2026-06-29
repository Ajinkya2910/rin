// src/cache.rs — built-package cache
//
// rin isolates by default (each project installs into its own .rin/lib). Without
// a cache that means recompiling the same package for every project. This module
// stores each *built* package once, keyed by build target, and links it into a
// project's library — isolation without the duplication.
//
// Key:    <platform> / <R-minor> / <name> / <version>   (GitHub: version = SHA)
// Layout: <cache>/built/<platform>/<rminor>/<name>/<version>/<name>/  (a real pkg dir)
// Link:   symlink the package dir into the project lib, copy as a fallback.
//
// Safety: only successful builds are published (build into a temp dir, then
// atomically rename into the slot), so a failed compile never poisons the cache,
// and concurrent builds of the same package can't corrupt a slot.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::resolver::ResolvedPackage;

/// Root of the built-package cache (the `built/` dir). Resolution order:
///   1. `$RIN_CACHE_DIR/built`   (escape hatch / HPC tuning / future shared cache)
///   2. `$XDG_CACHE_HOME/rin/built`
///   3. `~/.cache/rin/built`
pub fn cache_root() -> PathBuf {
    if let Ok(d) = std::env::var("RIN_CACHE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join("built");
        }
    }
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("rin").join("built");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".cache").join("rin").join("built")
}

/// (platform, R-minor) for the active R, e.g. ("aarch64-apple-darwin20", "4.4").
/// Queried once and memoized. `None` disables the cache for this run (we'd rather
/// skip caching than risk a wrong key), so a detection failure is never fatal.
fn build_target() -> Option<(String, String)> {
    static TARGET: OnceLock<Option<(String, String)>> = OnceLock::new();
    TARGET
        .get_or_init(|| {
            // platform on line 1; major.<minor-series> on line 2 (e.g. 4.4 — the
            // ABI-compatible series, not the patch level).
            let code = "cat(R.version$platform, \
                 paste(R.version$major, strsplit(R.version$minor, '.', fixed=TRUE)[[1]][1], sep='.'), \
                 sep='\\n')";
            let out = crate::installer::r_command()
                .args(["--vanilla", "--slave", "-e", code])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let text = String::from_utf8_lossy(&out.stdout);
            let mut lines = text.lines();
            let platform = lines.next()?.trim().to_string();
            let rminor = lines.next()?.trim().to_string();
            if platform.is_empty() || rminor.is_empty() {
                return None;
            }
            Some((platform, rminor))
        })
        .clone()
}

/// The version component of the key. GitHub packages key on the commit SHA
/// (a tag/branch can move); registry packages on the resolved version.
fn key_version(pkg: &ResolvedPackage) -> String {
    match &pkg.github_source {
        Some(gh) => gh.commit_sha.clone(),
        None => pkg.version.clone(),
    }
}

/// `<cache>/built/<platform>/<rminor>/<name>` — the per-package directory that
/// holds each version. `None` when the build target is unknown (cache disabled).
fn name_dir(pkg: &ResolvedPackage) -> Option<PathBuf> {
    let (platform, rminor) = build_target()?;
    Some(cache_root().join(platform).join(rminor).join(&pkg.name))
}

/// The cache slot for this exact package+version: the installed package dir.
/// `None` when caching is disabled for this run.
pub fn slot_path(pkg: &ResolvedPackage) -> Option<PathBuf> {
    Some(name_dir(pkg)?.join(key_version(pkg)).join(&pkg.name))
}

/// Return the slot if a built copy already exists.
pub fn lookup(pkg: &ResolvedPackage) -> Option<PathBuf> {
    let slot = slot_path(pkg)?;
    if slot.is_dir() {
        Some(slot)
    } else {
        None
    }
}

/// Begin a cached build: create and return a private staging library to
/// `R CMD INSTALL --library=` into. `Ok(None)` means caching is disabled this
/// run, so the caller should install straight into the project library.
pub fn begin_build(pkg: &ResolvedPackage) -> Result<Option<PathBuf>> {
    let Some(dir) = name_dir(pkg) else {
        return Ok(None);
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create cache dir {}", dir.display()))?;
    let staging = dir.join(format!(
        ".staging-{}-{}",
        key_version(pkg),
        std::process::id()
    ));
    // Clear any leftover from a crashed prior run on the same key/pid.
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("Failed to create staging dir {}", staging.display()))?;
    Ok(Some(staging))
}

/// Publish a freshly-built package: atomically move `staging` (which contains
/// `<name>/`) into the version slot. If another process published first, discard
/// our staging and use the existing slot. Returns the slot (`.../<version>/<name>`).
pub fn publish(pkg: &ResolvedPackage, staging: &Path) -> Result<PathBuf> {
    let dir = name_dir(pkg).context("cache disabled mid-build")?;
    let version_dir = dir.join(key_version(pkg));
    let slot = version_dir.join(&pkg.name);

    if version_dir.exists() {
        // Someone else won the race — use theirs, drop ours.
        let _ = std::fs::remove_dir_all(staging);
        return Ok(slot);
    }
    // Atomic on a single filesystem (staging is a sibling of version_dir).
    match std::fs::rename(staging, &version_dir) {
        Ok(()) => Ok(slot),
        Err(_) if version_dir.exists() => {
            // Lost a race between the check and the rename.
            let _ = std::fs::remove_dir_all(staging);
            Ok(slot)
        }
        Err(e) => Err(e).with_context(|| {
            format!("Failed to publish cache slot {}", version_dir.display())
        }),
    }
}

/// Link a cached package `slot` into `lib` as `lib/<name>`: symlink first
/// (cross-filesystem friendly, renv-style), copy as a fallback.
pub fn link_into_lib(slot: &Path, lib: &Path, name: &str) -> Result<()> {
    std::fs::create_dir_all(lib)
        .with_context(|| format!("Failed to create library {}", lib.display()))?;
    let dest = lib.join(name);

    // Clear any existing entry (stale symlink, real dir, or file).
    if let Ok(meta) = std::fs::symlink_metadata(&dest) {
        if meta.file_type().is_symlink() || meta.is_file() {
            let _ = std::fs::remove_file(&dest);
        } else if meta.is_dir() {
            let _ = std::fs::remove_dir_all(&dest);
        }
    }

    #[cfg(unix)]
    {
        if std::os::unix::fs::symlink(slot, &dest).is_ok() {
            return Ok(());
        }
    }
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(slot, &dest).is_ok() {
            return Ok(());
        }
    }

    // Fallback: independent recursive copy.
    copy_dir_all(slot, &dest)
        .with_context(|| format!("Failed to copy cached package into {}", dest.display()))
}

/// Recursively copy a directory tree (fallback when symlinks aren't available).
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
