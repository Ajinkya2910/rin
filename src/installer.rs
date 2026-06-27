// src/installer.rs — Package installation orchestration
//
// This module handles the actual installation of R packages.
// In Phase 1, it uses source compilation (R CMD INSTALL) but does it
// SMARTLY: parallel where possible, with pre-flight checks and resume.
//
// RUST CONCEPT: Rayon for Parallelism
// Rayon is a data parallelism library. You replace `.iter()` with
// `.par_iter()` and your code runs in parallel across CPU cores.
// It automatically handles thread pools and work stealing.
//
//   // Sequential:
//   packages.iter().for_each(|p| install(p));
//
//   // Parallel (that's the ONLY change):
//   packages.par_iter().for_each(|p| install(p));
//
// Rayon figures out the optimal number of threads and distributes work.

use crate::resolver::{ResolvedDeps, ResolvedPackage};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;

/// State file path for tracking install progress (for --retry)
const STATE_FILE: &str = ".rin-install-state.json";

/// Whether to build packages with conda stripped from the environment.
/// Decided once at the start of `install()` (see `decide_conda_exclusion`),
/// then read by every parallel `run_r_cmd_install`. A process-global avoids
/// threading the flag through the rayon closures.
static EXCLUDE_CONDA: AtomicBool = AtomicBool::new(false);

/// Install all resolved packages in dependency order with parallelism.
///
/// The strategy:
/// 1. Group packages into "tiers" — packages whose deps are all satisfied
/// 2. Install each tier in parallel (within a tier, packages are independent)
/// 3. Track progress for resume capability
pub async fn install(resolved: &ResolvedDeps, bioc_version: &str) -> Result<usize> {
    use colored::Colorize;

    // Bug #46: ensure we have a writable install target before doing anything.
    // On HPC, R's default library is often the shared module install (read-only).
    // If so, auto-create .rin/lib so the install lands somewhere the user owns.
    let auto_created = ensure_writable_library()?;
    if auto_created {
        println!(
            "  {} System R library is read-only — created {} for this project.",
            "ℹ".blue(),
            ".rin/lib".bold()
        );
        println!(
            "    {} for a fully-configured environment with an activate script.",
            "Run `rin venv` instead".dimmed()
        );
    }

    // macOS-only: an active conda env makes R link compiled packages against
    // conda's libs, which then fail to load via @rpath. Offer to build with
    // conda excluded. On Linux/HPC conda is often the intended toolchain, so
    // `decide_conda_exclusion` returns false there and we keep the Bug #16
    // injection advisory below.
    let exclude_conda = decide_conda_exclusion();
    EXCLUDE_CONDA.store(exclude_conda, Ordering::Relaxed);

    // Bug #16: one-time advisory so users see why their conda libs are visible.
    // Suppressed when we're excluding conda (decide_conda_exclusion already
    // printed what it's doing).
    if !exclude_conda {
        if let Some((pkgconfig_dir, _)) = conda_env_additions() {
            println!(
                "  {} conda env detected: prepending {} to PKG_CONFIG_PATH",
                "ℹ".blue(),
                pkgconfig_dir.dimmed()
            );
        }
    }
    let total = resolved.packages.len();
    let mut installed: HashSet<String> = HashSet::new();
    let mut failed: Vec<(String, String)> = Vec::new(); // (name, error)
    let mut retry_queue: HashSet<String> = HashSet::new();
    // Find packages already installed on this system. These are part of the
    // resolved closure but need no work — collapse them into one summary line
    // instead of one line each, so the packages we're *actually* installing
    // aren't buried. (Pass -v / RIN_VERBOSE to list them.)
    let already_installed = check_installed_versions(&resolved.packages);
    let already_count = already_installed.len();
    for name in &already_installed {
        installed.insert(name.clone());
    }
    // `to_install` scopes the progress counter to real work, not the whole
    // dependency graph, so a single new package reads (1/1), not (48/48).
    let to_install = total - already_count;
    let verbose = std::env::var("RIN_VERBOSE").is_ok();
    println!(
        "  {} Resolved {} package(s) · {}",
        "✓".green(),
        total,
        if to_install == 0 {
            "all up to date".dimmed().to_string()
        } else {
            format!("{} already installed", already_count).dimmed().to_string()
        }
    );
    if verbose {
        for name in &already_installed {
            println!("    {} {} (already installed)", "·".dimmed(), name.dimmed());
        }
    }

    // Install in tiers
    // RUST CONCEPT: `loop` is an infinite loop. We break out when done.
    // Rust also has `while` and `for`, but `loop` is idiomatic when
    // the exit condition is complex.
    loop {
        // Find packages whose dependencies are all satisfied
        let ready: Vec<&ResolvedPackage> = resolved
            .packages
            .iter()
            .filter(|pkg| {
                // Not yet installed
                !installed.contains(&pkg.name)
                    // Not already failed
                    && !failed.iter().any(|(n, _)| n == &pkg.name)
                    // All dependencies are installed
                    && pkg.dependencies.iter().all(|dep| {
                        installed.contains(dep)
                            // Or the dep isn't in our resolve set (base package)
                            || !resolved.packages.iter().any(|p| p.name == *dep)
                    })
            })
            .collect();

        if ready.is_empty() {
            // Nothing more to install — either done or stuck
            break;
        }

        // Show which packages are in this batch, not just the count. In a real
        // terminal the progress bar below also surfaces the current package,
        // but in a non-TTY console (e.g. RStudio) the bar doesn't render, so
        // this line is the only signal of what's compiling. Cap the list so a
        // large tier doesn't flood the screen.
        const MAX_NAMES: usize = 8;
        let names: Vec<&str> = ready.iter().map(|p| p.name.as_str()).collect();
        let shown = names
            .iter()
            .take(MAX_NAMES)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if names.len() > MAX_NAMES {
            format!(", … +{} more", names.len() - MAX_NAMES)
        } else {
            String::new()
        };
        println!(
            "\n  {} Installing {} package(s) in parallel: {}{}",
            "→".blue(),
            ready.len(),
            shown.dimmed(),
            suffix.dimmed()
        );

        // Install this tier in parallel using rayon
        //
        // RUST CONCEPT: par_iter() from rayon
        // This is the parallel magic. Each package in the tier gets
        // compiled on a separate thread. Rayon handles the thread pool.
        //
        // We collect results into a Vec of (name, Result) tuples.
        let pb = ProgressBar::new(to_install as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("  [{bar:30.green/dim}] {pos}/{len} {msg}")
                .unwrap()
                .progress_chars("█░░"),
        );
        pb.set_position((installed.len() - already_count) as u64);

        let results: Vec<(String, Result<()>)> = ready
            .par_iter()
            .map(|pkg| {
                pb.set_message(pkg.name.clone());
                let result = install_single_package(pkg,&bioc_version);
                pb.inc(1);
                (pkg.name.clone(), result)
            })
            .collect();

        pb.finish_and_clear();

        // Note: using .iter().map() instead of .par_iter() for now
        // because async + rayon interaction needs care.
        // Switch to par_iter() when you're ready:
        //
        //   use rayon::prelude::*;
        //   let results: Vec<_> = ready.par_iter().map(|pkg| {
        //       (pkg.name.clone(), install_single_package(pkg))
        //   }).collect();

        // Process results
        for (name, result) in results {
            match result {
                Ok(()) => {
                    println!(
                        "  {} {} {}",
                        "✓".green(),
                        name,
                        format!("({}/{})", installed.len() + 1 - already_count, to_install)
                            .dimmed()
                    );
                    installed.insert(name);
                }
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    // Bug #6: only retry on a probe-confirmed race. The [LAZY_RACE]
                    // marker is set by parse_compile_error after loadNamespace
                    // succeeded standalone. A bare "lazy loading failed" without
                    // that marker is a real error and should fail through.
                    if err_msg.contains("[LAZY_RACE]") && !retry_queue.contains(&name) {
                        println!("  {} {} — will retry next tier (probe-confirmed race)", "↻".yellow(), name.yellow());
                        retry_queue.insert(name);
                    } else {
                        // Permanent failure (either not lazy loading, or already retried once)
                        println!("  {} {} — {}", "✗".red(), name.red(), err_msg);
                        failed.push((name, err_msg));
                    }
                }
            }
        }

        // Save progress for --retry
        save_install_state(&installed, &failed)?;
    }

    // Sweep for packages that were never attempted because a dependency
    // failed (or got blocked itself), or because of a dependency cycle.
    // Without this, blocked packages silently vanish from the report: they
    // are neither in `installed` nor `failed`, the counts don't add up, and
    // a pure cycle would even return Ok(()) — a false success.
    for pkg in &resolved.packages {
        if installed.contains(&pkg.name) || failed.iter().any(|(n, _)| n == &pkg.name) {
            continue;
        }
        let blocker = pkg.dependencies.iter().find(|dep| {
            !installed.contains(*dep) && resolved.packages.iter().any(|p| &p.name == *dep)
        });
        let reason = match blocker {
            Some(dep) => format!("skipped — blocked by failed/unbuilt dependency '{}'", dep),
            None => "skipped — dependency cycle or unresolved constraint".to_string(),
        };
        failed.push((pkg.name.clone(), reason));
    }
    // Persist the final state so --retry sees the blocked packages too.
    save_install_state(&installed, &failed)?;

    // Report results
    if !failed.is_empty() {
        println!(
            "\n{} {}/{} packages installed. {} failed:",
            "⚠".yellow(),
            installed.len(),
            total,
            failed.len()
        );

        for (name, error) in &failed {
            println!("  {} {}: {}", "✗".red(), name, error);
        }

        println!(
            "\n{}",
            "Fix the issues above, then run: rin install --retry".bold()
        );

        // Return error so the process exits with non-zero status
        anyhow::bail!("{} packages failed to install", failed.len());
    }

    // Success summary: lead with what changed, not the closure size.
    let new_count = installed.len() - already_count;
    if new_count == 0 {
        println!("\n  {} Everything up to date.", "✓".green());
    } else {
        println!(
            "\n  {} Done. {} installed, {} up to date.",
            "✓".green(),
            new_count,
            already_count
        );
    }

    // Return how many were actually built so callers can phrase their final
    // line truthfully ("installed N" vs "already up to date").
    Ok(new_count)
}

/// Install a single R package from source using R CMD INSTALL
fn install_single_package(pkg: &ResolvedPackage, bioc_version: &str) -> Result<()> {
    // GitHub packages: install from the cached tarball, skipping download.
    if let Some(gh) = &pkg.github_source {
        return install_from_github_tarball(pkg, gh);
    }

    // ── CRAN / Bioconductor path ──────────────────────────────────────────

    let download_dir = PathBuf::from("/tmp/rin-downloads");
    std::fs::create_dir_all(&download_dir)?;

    let tarball_path = download_dir.join(format!("{}_{}.tar.gz", pkg.name, pkg.version));

    // Reuse a previously downloaded tarball only if it looks complete.
    let cached_ok = tarball_path.exists()
        && std::fs::metadata(&tarball_path).map(|m| m.len()).unwrap_or(0) > 1000;

    if !cached_ok {
        if tarball_path.exists() {
            std::fs::remove_file(&tarball_path).ok();
        }

        // Try each known location in priority order. For CRAN this now includes
        // the Archive, where every superseded version lives — without it, any
        // lockfile pinning a non-latest version can never be downloaded.
        let candidates = candidate_urls(pkg, bioc_version)?;
        let mut downloaded = false;
        for url in &candidates {
            if download_tarball_curl(url, &tarball_path) {
                downloaded = true;
                break;
            }
            // Drop any partial/error body before trying the next candidate.
            std::fs::remove_file(&tarball_path).ok();
        }

        if !downloaded {
            anyhow::bail!(
                "Failed to download {} {} from any known location (live repo, CRAN Archive, fallbacks).\n  Tried:\n    {}",
                pkg.name,
                pkg.version,
                candidates.join("\n    ")
            );
        }
    }

    // Bug #4: integrity check. When the lockfile carries a hash for this
    // package, verify the downloaded bytes match before installing. CRAN/Bioc
    // entries currently have no hash (sha256 = None), so this is a no-op for
    // them today — it activates automatically once the resolver records one.
    if let Some(expected) = pkg.sha256.as_deref().filter(|s| !s.is_empty()) {
        let actual = sha256_file(&tarball_path)?;
        if !actual.eq_ignore_ascii_case(expected) {
            std::fs::remove_file(&tarball_path).ok();
            anyhow::bail!(
                "Integrity check failed for {} {}:\n  expected {}\n  got      {}\n  \
                 Downloaded tarball does not match the lockfile — removed it. Re-run to retry.",
                pkg.name,
                pkg.version,
                expected,
                actual
            );
        }
    }

    run_r_cmd_install(&tarball_path, &pkg.name)?;
    Ok(())
}

/// Build the ordered list of URLs to try for a CRAN/Bioc source tarball.
///
/// CRAN keeps only the *current* version under /src/contrib/; every previous
/// version is moved to /src/contrib/Archive/{pkg}/. A lockfile pins exact
/// versions, so restoring an environment routinely needs the Archive path.
fn candidate_urls(pkg: &ResolvedPackage, bioc_version: &str) -> Result<Vec<String>> {
    let mut urls = Vec::new();

    match pkg.source.as_str() {
        "cran" => {
            // Live repo (only holds the latest version).
            urls.push(format!(
                "https://cloud.r-project.org/src/contrib/{}_{}.tar.gz",
                pkg.name, pkg.version
            ));
            // Archive (superseded versions).
            urls.push(format!(
                "https://cloud.r-project.org/src/contrib/Archive/{}/{}_{}.tar.gz",
                pkg.name, pkg.name, pkg.version
            ));

            // Version-suffix normalization (e.g. "1.0-1" -> "1.0"): try the
            // stripped form against both live and archive too.
            if let Some(pos) = pkg.version.rfind('-') {
                let suffix = &pkg.version[pos + 1..];
                if suffix.len() <= 2 && suffix.chars().all(|c| c.is_ascii_digit()) {
                    let stripped = &pkg.version[..pos];
                    urls.push(format!(
                        "https://cloud.r-project.org/src/contrib/{}_{}.tar.gz",
                        pkg.name, stripped
                    ));
                    urls.push(format!(
                        "https://cloud.r-project.org/src/contrib/Archive/{}/{}_{}.tar.gz",
                        pkg.name, pkg.name, stripped
                    ));
                }
            }
        }
        "bioc" => {
            // Bioconductor splits packages across categories; we don't always
            // know which one a package belongs to, so try each.
            for cat in ["bioc", "data/annotation", "data/experiment", "workflows"] {
                urls.push(format!(
                    "https://bioconductor.org/packages/{}/{}/src/contrib/{}_{}.tar.gz",
                    bioc_version, cat, pkg.name, pkg.version
                ));
            }
        }
        other => anyhow::bail!("Unknown source: {}", other),
    }

    Ok(urls)
}

/// Download `url` to `dest` with curl. Returns true only if a real tarball
/// landed. `-f` makes curl fail (non-zero, no body written) on HTTP >= 400,
/// so a 404 from the live repo doesn't leave an HTML error page behind.
///
/// We `.output()` rather than `.status()` so curl's stderr is captured, not
/// inherited. Each candidate URL is tried in priority order (live repo, then
/// Archive), so a 404 on the first attempt is *expected* and routine — letting
/// curl print `curl: (22) ... 404` to the terminal would corrupt the progress
/// bar with noise for a non-error. If every candidate fails, the caller's
/// `bail!` already reports the full list of URLs tried.
fn download_tarball_curl(url: &str, dest: &PathBuf) -> bool {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "3",
            "--retry-delay",
            "1",
            "--max-time",
            "300",
            "-o",
        ])
        .arg(dest)
        .arg(url)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0) > 1000
        }
        _ => false,
    }
}

/// Compute the hex-encoded SHA-256 of a file.
fn sha256_file(path: &PathBuf) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {} for integrity check", path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Install a GitHub-sourced package from its already-downloaded tarball.
///
/// Day 2 cached the tarball at ~/.rin/cache/github/{owner}/{repo}/{sha}.tar.gz.
/// We extract it to a unique temp dir, locate the package root, and run
/// R CMD INSTALL pointing at the directory (not the tarball).
///
/// On success: clean up the temp dir.
/// On failure: leave it so the user can inspect what went wrong.
fn install_from_github_tarball(
    pkg: &ResolvedPackage,
    gh: &crate::resolver::GitHubSource,
) -> Result<()> {
    let cache_dir = github_cache_dir()?;
    let tarball_path = cache_dir
        .join("github")
        .join(&gh.owner)
        .join(&gh.repo)
        .join(format!("{}.tar.gz", gh.commit_sha));

    // The resolver normally populates this during the resolve phase. But the
    // cache can be evicted between resolve and install (manual cleanup, a TMP
    // sweep, a separate `rin` run). Rather than dead-end the user with "try
    // re-running", re-fetch the exact pinned SHA on demand and verify it against
    // the lockfile's recorded hash.
    if !tarball_path.exists() {
        redownload_github_tarball(gh, &cache_dir, &tarball_path)?;
    }

    // Unique temp dir per (owner, repo, full-sha) — no collision risk.
    let tmp_extract = std::env::temp_dir().join(format!(
        "rin-gh-{}-{}-{}",
        gh.owner, gh.repo, gh.commit_sha
    ));

    // Clean any prior leftover from a failed run on the same SHA.
    let _ = std::fs::remove_dir_all(&tmp_extract);
    std::fs::create_dir_all(&tmp_extract)?;

    extract_tarball(&tarball_path, &tmp_extract)
        .with_context(|| format!("Failed to extract {}", tarball_path.display()))?;

    let pkg_dir = crate::registry::github::find_package_root(
        &tmp_extract,
        gh.subdir.as_deref(),
    )?;

    match run_r_cmd_install(&pkg_dir, &pkg.name) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&tmp_extract);
            Ok(())
        }
        Err(e) => {
            // Preserve the source so the user can inspect what failed.
            eprintln!(
                "  source preserved at {} for inspection",
                tmp_extract.display()
            );
            Err(e.context(format!("Failed to install {} from GitHub", pkg.name)))
        }
    }
}

/// Re-fetch a GitHub tarball that's gone missing from the cache between
/// resolve and install. Downloads the exact pinned commit SHA and verifies
/// the bytes against the SHA-256 recorded at resolve time, so a re-download
/// can never silently substitute different contents.
///
/// Runs from a sync rayon worker, so it spins up a small current-thread tokio
/// runtime to drive the async download rather than borrowing the outer one.
fn redownload_github_tarball(
    gh: &crate::resolver::GitHubSource,
    cache_dir: &PathBuf,
    tarball_path: &PathBuf,
) -> Result<()> {
    eprintln!(
        "  {} re-fetching {}/{}@{} (missing from cache)",
        "↻", gh.owner, gh.repo, &gh.commit_sha[..gh.commit_sha.len().min(7)]
    );

    let spec = crate::source::GitHubSpec {
        owner: gh.owner.clone(),
        repo: gh.repo.clone(),
        r#ref: Some(gh.commit_sha.clone()),
        subdir: gh.subdir.clone(),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to start runtime for tarball re-download")?;
    let client = reqwest::Client::new();
    let (_, digest) = rt
        .block_on(crate::registry::github::download_tarball(
            &spec,
            &gh.commit_sha,
            cache_dir,
            &client,
        ))
        .with_context(|| {
            format!(
                "Re-downloading {}/{}@{} from GitHub",
                gh.owner, gh.repo, gh.commit_sha
            )
        })?;

    // Integrity gate: the re-downloaded bytes must match what the lockfile
    // pinned. A mismatch means the upstream ref moved or the cache is poisoned.
    if !gh.tarball_sha256.is_empty() && digest != gh.tarball_sha256 {
        anyhow::bail!(
            "Re-downloaded tarball for {}/{}@{} has SHA-256 {} but the lockfile \
             expected {}. Refusing to install mismatched contents.",
            gh.owner, gh.repo, gh.commit_sha, digest, gh.tarball_sha256
        );
    }

    if !tarball_path.exists() {
        anyhow::bail!(
            "Re-download reported success but tarball is still missing: {}",
            tarball_path.display()
        );
    }
    Ok(())
}

/// Extract a .tar.gz into a destination directory.
fn extract_tarball(tarball: &PathBuf, dest: &PathBuf) -> Result<()> {
    let file = std::fs::File::open(tarball)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(dest)?;
    Ok(())
}

/// Where Day 2 wrote the GitHub cache. Mirrors prepare_github_packages.
fn github_cache_dir() -> Result<PathBuf> {
    let base = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h).join(".rin").join("cache"),
        Err(_) => std::env::temp_dir().join("rin-cache"),
    };
    std::fs::create_dir_all(&base)?;
    Ok(base)
}
/// Turn a "symbol not found" dlopen failure into an actionable
/// build-vs-runtime version-mismatch message. `chunk` is the joined dyld error
/// text (the `.so` path and the missing symbol).
fn explain_symbol_mismatch(pkg_name: &str, chunk: &str) -> String {
    let symbol = extract_missing_symbol(chunk);
    let culprit = extract_so_package(chunk);
    let lib = symbol.as_deref().and_then(guess_system_lib);

    let culprit_str = culprit
        .as_deref()
        .map(|p| format!(" ({})", p))
        .unwrap_or_default();
    let symbol_str = symbol
        .as_deref()
        .map(|s| format!(" (missing symbol {})", s))
        .unwrap_or_default();

    let mut msg = format!(
        "Lazy loading failed for '{}'. Real cause: a compiled dependency{} was built \
         against one version of its system library but is loading a different one{}.\n  \
         This is a build/runtime version mismatch, not a missing R package.",
        pkg_name, culprit_str, symbol_str
    );

    let rebuild = culprit.as_deref().unwrap_or("<package>");
    match lib {
        Some("HDF5") => msg.push_str(&format!(
            "\n  HDF5: Homebrew ships 2.x, but hdf5r needs the 1.10–1.14 API. Rebuild against 1.x:\n    \
               brew install hdf5@1.10 && brew unlink hdf5 && brew link --overwrite --force hdf5@1.10\n    \
               rm -rf \"$(R RHOME)\"/../{0} ; rin install {0}\n    \
               (afterward: brew unlink hdf5@1.10 && brew link hdf5  — keeps gdal/netcdf working)\n  \
             Or sidestep entirely: conda install -c conda-forge r-hdf5r",
            rebuild
        )),
        Some(other) => msg.push_str(&format!(
            "\n  Ensure only one version of {} is active (check: otool -L / ldd on the .so), \
             then rebuild {}: rin install {}",
            other, rebuild, rebuild
        )),
        None => msg.push_str(
            "\n  Inspect which library it links (otool -L on macOS, ldd on Linux), make one \
             consistent version active, then rebuild the offending dependency.",
        ),
    }
    msg
}

/// Pull the missing symbol name out of a dyld error, e.g. `_H5Dread_chunk`.
/// Handles both the flat-namespace form and newer "Symbol not found:" form.
fn extract_missing_symbol(s: &str) -> Option<String> {
    if let Some(i) = s.find("flat namespace ") {
        return extract_quoted(&s[i..]).map(|x| x.to_string());
    }
    if let Some(i) = s.find("Symbol not found:") {
        let rest = s[i + "Symbol not found:".len()..].trim();
        let sym: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        if !sym.is_empty() {
            return Some(sym);
        }
    }
    None
}

/// From a dlopen error mentioning `.../<pkg>/libs/<pkg>.so`, recover `<pkg>`.
fn extract_so_package(s: &str) -> Option<String> {
    let path = s.split('\'').find(|seg| seg.contains(".so"))?;
    let file = path.rsplit('/').next()?;
    let stem = file.strip_suffix(".so")?;
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

/// Map a missing C symbol to the system library it belongs to, when the prefix
/// is recognizable. Drives a library-specific remediation hint.
fn guess_system_lib(symbol: &str) -> Option<&'static str> {
    let s = symbol.trim_start_matches('_');
    if s.starts_with("H5") {
        Some("HDF5")
    } else if s.starts_with("GDAL") || s.starts_with("OGR") {
        Some("GDAL")
    } else if s.starts_with("proj_") || s.starts_with("pj_") {
        Some("PROJ")
    } else if s.starts_with("GEOS") {
        Some("GEOS")
    } else {
        None
    }
}

fn extract_quoted(line: &str) -> Option<&str> {
    for (open, close) in &[('\'', '\''), ('\u{2018}', '\u{2019}'), ('"', '"')] {
        if let Some(start) = line.find(*open) {
            let after = &line[start + open.len_utf8()..];
            if let Some(end) = after.find(*close) {
                return Some(&after[..end]);
            }
        }
    }
    None
}
/// Build a `Command` for the R binary, preferring the R that invoked rin.
///
/// When rin is spawned from an R session (e.g. RStudio), `R_HOME` points at
/// that session's R. Invoking `$R_HOME/bin/R` by absolute path keeps rin on the
/// *same* R the user is running, instead of whatever `R` is first on `PATH`.
/// On machines with conda/anaconda, the PATH-first `R` is often a different
/// install whose Makeconf lacks per-standard compilers (e.g. `CC17` undefined),
/// breaking C17 packages like locfit even though the user's actual R is fine.
pub fn r_command() -> Command {
    if let Ok(home) = std::env::var("R_HOME") {
        if !home.is_empty() {
            let candidate = std::path::Path::new(&home).join("bin").join("R");
            if candidate.is_file() {
                return Command::new(candidate);
            }
        }
    }
    Command::new("R")
}

/// Run `R CMD INSTALL` against a tarball OR an extracted package directory.
/// R CMD INSTALL accepts both — that's why this helper works for both paths.
fn run_r_cmd_install(target: &PathBuf, pkg_name: &str) -> Result<()> {
    let lib_arg = get_venv_lib().map(|p| format!("--library={}", p.display()));

    let mut cmd = r_command();
    cmd.args(["CMD", "INSTALL", "--no-test-load"]);
    if let Some(ref lib) = lib_arg {
        cmd.arg(lib);
    }
    cmd.arg(target);

    if EXCLUDE_CONDA.load(Ordering::Relaxed) {
        // User opted to build conda-free: strip conda from the subprocess env
        // so R links against system/Homebrew libs instead of conda's.
        if let Ok(prefix) = std::env::var("CONDA_PREFIX") {
            sanitize_conda_from_cmd(&mut cmd, &prefix);
        }
    } else if let Some((pkgconfig_dir, lib_dir)) = conda_env_additions() {
        // Bug #16: propagate conda env libs into the build subprocess.
        // Prepend so user-set values still win for collisions.
        let existing_pc = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
        let new_pc = if existing_pc.is_empty() {
            pkgconfig_dir
        } else {
            format!("{}:{}", pkgconfig_dir, existing_pc)
        };
        cmd.env("PKG_CONFIG_PATH", new_pc);

        #[cfg(target_os = "macos")]
        let ld_var = "DYLD_LIBRARY_PATH";
        #[cfg(not(target_os = "macos"))]
        let ld_var = "LD_LIBRARY_PATH";

        let existing_ld = std::env::var(ld_var).unwrap_or_default();
        let new_ld = if existing_ld.is_empty() {
            lib_dir
        } else {
            format!("{}:{}", existing_ld, lib_dir)
        };
        cmd.env(ld_var, new_ld);
    }

    let output = cmd.output().context("R is not installed")?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!(
            "=== stdout ===\n{}\n\n=== stderr ===\n{}",
            stdout, stderr
        );

        // Persist the full stderr for debugging — friendly_error below
        // is a one-line summary; users need the real output too.
        let log_path = std::path::PathBuf::from(format!(
            "/tmp/rin-fail-{}.log",
            pkg_name
        ));
        let _ = std::fs::write(&log_path, combined.as_bytes());

        let friendly_error = parse_compile_error(&combined, pkg_name);
        anyhow::bail!(
            "{}\n  Full output: {}",
            friendly_error,
            log_path.display()
        );
    }

    Ok(())
}

/// Parse a compilation error and return a human-friendly message
///
/// Instead of dumping 200 lines of g++ output, we extract the actual problem.
fn parse_compile_error(stderr: &str, pkg_name: &str) -> String {
    // Check for common error patterns

    // Missing header file
    if let Some(header) = stderr
        .lines()
        .find(|l| l.contains("No such file or directory") && l.contains(".h"))
    {
        // Extract the header name
        let header_name = header
            .split("fatal error:")
            .nth(1)
            .unwrap_or(header)
            .trim();
        return with_sysreq_hint(
            format!("Missing header: {}\n  A system library is probably not installed.", header_name),
            pkg_name,
        );
    }

    // C++ standard mismatch
     if stderr.contains("std::filesystem")
        || stderr.contains("is not available in C++")
        || stderr.contains("is a C++14 extension")
        || stderr.contains("is a C++17 extension")
        || stderr.contains("is a C++20 extension")
        || stderr.contains("requires '-std=c++")
    {
        return "C++ standard mismatch: package needs a newer C++ standard than R is using.\n  \
                Fix: echo 'CXX_STD = CXX17' >> ~/.R/Makevars\n  \
                Then: rin install --retry".to_string();
    }

    // Missing Fortran compiler
    if stderr.contains("gfortran: command not found") || stderr.contains("gfortran: not found") {
        return "Fortran compiler not found.\n  Fix: sudo apt install gfortran".to_string();
    }

    // Missing cmake
    if stderr.contains("cmake") && stderr.contains("not found") {
        return "cmake not found or too old.\n  Fix: sudo apt install cmake".to_string();
    }
    // Missing R package dependency
    // e.g., "ERROR: dependency 'sitmo' is not available for package 'dqrng'"
    if let Some(line) = stderr.lines().find(|l| l.contains("dependency") && l.contains("is not available")) {
        // Extract the missing package name from between the quotes
        let missing = line.split('\'').nth(1);
        return match missing {
            Some(pkg) => format!(
                "Missing R dependency: {}\n  Fix: rin install {} first, then retry",
                pkg, pkg
            ),
            None => format!(
                "Missing R dependency (couldn't parse name).\n  Check the error output above for the missing package."
            ),
        };

    }

    // Lazy loading failure (parallel compilation race condition)
    // e.g., "ERROR: lazy loading failed for package 'ggrepel'"
    // Lazy loading failure — probe for the real cause (Bugs #19, #6).
    if stderr.contains("lazy loading failed") {
    // Pattern A: dyn.load failure — surfaces ABI/library mismatches
    if let Some(idx) = stderr.find("unable to load shared object") {
        let chunk: String = stderr[idx..]
            .lines()
            .take(3)
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with("Calls:"))
            .collect::<Vec<_>>()
            .join(" ");

        // A "symbol not found" dlopen failure means a compiled package was
        // built against one version of a system library but is loading a
        // different one at runtime — not a missing package. Translate the raw
        // dyld message into a version-mismatch diagnosis with the fix.
        if chunk.contains("symbol not found") || chunk.contains("Symbol not found") {
            return explain_symbol_mismatch(pkg_name, &chunk);
        }

        return format!(
            "Lazy loading failed for '{}'. Real cause:\n  {}",
            pkg_name, chunk
        );
    }

    // Pattern B: missing namespace dependency
    if let Some(line) = stderr
        .lines()
        .find(|l| l.contains("there is no package called"))
    {
        if let Some(dep) = extract_quoted(line) {
            return format!(
                "Lazy loading failed for '{}'. Real cause: missing dependency '{}' — install it and retry.",
                pkg_name, dep
            );
        }
    }

    // Fallback: known failure mode but unrecognized pattern
    return format!(
        "Lazy loading failed for '{}'. See full log for details.",
        pkg_name
    );
}

    // Permission denied
    if stderr.contains("Permission denied") || stderr.contains("cannot create directory") {
        return "Permission denied when writing to library.\n  Fix: use rin venv to create a project-local library, or check directory permissions.".to_string();
    }

    // Disk space
    if stderr.contains("No space left on device") {
        return "Disk full — no space left on device.\n  Fix: free disk space and retry.".to_string();
    }
     // Fortran library path mismatch (macOS Makevars issue)
    if stderr.contains("library 'gfortran' not found") 
        || stderr.contains("/opt/gfortran/lib") 
    {
        return "Fortran library path mismatch — R is looking for gfortran in the wrong location.\n  Fix: update ~/.R/Makevars with the correct gfortran path.\n  Run rin audit for details.".to_string();
    }

    // Linker errors (missing system library at link time)
    if stderr.contains("undefined reference to")
        || stderr.contains("symbol(s) not found")
        || stderr.contains("ld: library not found")
    {
        return with_sysreq_hint(
            "Linker error — a system library is missing or not found by the linker.\n  Fix: run rin audit to check system dependencies.".to_string(),
            pkg_name,
        );
    }

    // R toolchain: a package requested a C/C++ standard whose compiler macro
    // (e.g. CC17) is empty in the active R's Makeconf — "C17 standard requested
    // but CC17 is not defined". Seen intermittently under heavy parallel builds;
    // a retry usually clears it. Persisting usually means a stale/older R on
    // PATH (< 4.3, before per-standard compilers) or a personal ~/.R/Makevars
    // overriding CC/CXX.
    if stderr.contains("standard requested but") && stderr.contains("is not defined") {
        return format!(
            "R toolchain: {} — a C/C++ standard's compiler is undefined in R's Makeconf.\n  \
             Fix: usually transient — run `rin install --retry` (or rin::install(retry=TRUE)).\n  \
             If it persists: ensure rin uses a current R (R --version ≥ 4.3) and that ~/.R/Makevars isn't overriding CC/CXX.",
            stderr
                .lines()
                .find(|l| l.contains("standard requested but"))
                .map(str::trim)
                .unwrap_or("standard not defined")
        );
    }

    // Generic: take the last meaningful error line. Case-insensitive so a
    // capitalized "Error:" (e.g. R's own build errors) isn't missed — that gap
    // is what turned the CC17 failure above into a useless "Unknown" message.
    let last_error = stderr
        .lines()
        .rev()
        .find(|l| {
            let lower = l.to_ascii_lowercase();
            lower.contains("error:") || lower.contains("fatal error") || l.contains("ERROR")
        })
        .unwrap_or("Unknown compilation error");

    // A bare "configuration failed" / generic compile error is the most common
    // shape for a missing system library that didn't trip the header/linker
    // patterns above (e.g. rJava's configure aborting). Attach a remediation
    // hint when we know what this package needs.
    with_sysreq_hint(format!("Compilation error: {}", last_error.trim()), pkg_name)
}

/// Append module/conda remediation hints to a failure message when `rin` has an
/// offline sysreq mapping for the package. No-op (returns `msg` unchanged) for
/// packages with no known system dependencies — pure-R build failures stay terse.
fn with_sysreq_hint(msg: String, pkg_name: &str) -> String {
    match crate::sysreq::sysreq_hints(pkg_name) {
        Some(hint) => format!("{}\n{}", msg, hint),
        None => msg,
    }
}

/// Get the active venv library path, if any
fn get_venv_lib() -> Option<std::path::PathBuf> {
    // First check if venv is activated via environment variable
    if let Ok(path) = std::env::var("RIN_VENV") {
        let lib_path = std::path::PathBuf::from(path).join("lib");
        if lib_path.exists() {
            return Some(lib_path);
        }
    }
    // Fallback: check if .rin/lib exists in current directory
    let local = std::path::PathBuf::from(".rin/lib");
    if local.exists() {
        return Some(std::fs::canonicalize(&local).unwrap_or(local));
    }
    None
}

/// Bug #46: ensure rin has a writable library before any install starts.
///
/// Decision tree:
///   1. A venv is already active (RIN_VENV set, or .rin/lib exists in CWD)
///      → use it, no change. (get_venv_lib already handles this.)
///   2. R's default library is writable
///      → use it (preserves normal Linux/Mac dev experience).
///   3. R's default library is read-only (typical HPC: shared module install)
///      → auto-create .rin/lib in the current directory. get_venv_lib() will
///        pick this up on every subsequent call, so the rest of the install
///        pipeline needs no changes.
///
/// Returns true if auto-creation happened — the caller prints a one-time notice.
fn ensure_writable_library() -> Result<bool> {
    use std::io::Write;

    // Case 1: venv already active. Nothing to do.
    if get_venv_lib().is_some() {
        return Ok(false);
    }

    // Ask R for its default library path.
    let r_default: Option<PathBuf> = r_command()
        .args(["--vanilla", "--slave", "-e", "cat(.libPaths()[1])"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    // Case 2: test if R's default is writable by opening a probe file.
    // This is more portable than checking Unix permissions and handles
    // edge cases like read-only mounts and quota-exhausted directories.
    if let Some(ref default_lib) = r_default {
        let probe = default_lib.join(".rin-write-test");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&probe)
        {
            let _ = f.write_all(b"ok");
            let _ = std::fs::remove_file(&probe);
            return Ok(false); // System library is writable — use it.
        }
    }

    // Case 3: system library is read-only (or R is unavailable).
    // Auto-create .rin/lib. get_venv_lib() will see it on the next call.
    let auto_lib = PathBuf::from(".rin/lib");
    std::fs::create_dir_all(&auto_lib)
        .context("Failed to create .rin/lib for fallback install location")?;

    Ok(true)
}

/// If a conda env is active, return (pkgconfig_dir, lib_dir) to prepend
/// to PKG_CONFIG_PATH and LD_LIBRARY_PATH (DYLD_LIBRARY_PATH on macOS).
/// Returns None if CONDA_PREFIX is unset or doesn't look like a real env.
fn conda_env_additions() -> Option<(String, String)> {
    let prefix = std::env::var("CONDA_PREFIX").ok()?;
    let prefix_path = std::path::PathBuf::from(&prefix);

    // Sanity: stale CONDA_PREFIX pointing at a deleted env is common.
    if !prefix_path.join("lib").is_dir() {
        return None;
    }

    Some((
        prefix_path.join("lib").join("pkgconfig").display().to_string(),
        prefix_path.join("lib").display().to_string(),
    ))
}

/// macOS-only: decide whether to build with conda excluded from the environment.
///
/// Returns false (keep current behavior) when no conda env is active, or when
/// not on macOS — on Linux/HPC conda is often the intended toolchain and the
/// `@rpath` dlopen failure that motivates this is dyld-specific.
///
/// On macOS with conda active: warn, then prompt [Y/n] (default Y). A
/// non-interactive run can't answer, so it defaults to conda-free (the safe
/// choice) and prints a notice instead of blocking.
fn decide_conda_exclusion() -> bool {
    use colored::Colorize;
    use std::io::{IsTerminal, Write};

    // Only relevant when a conda env with real libs is active.
    if conda_env_additions().is_none() {
        return false;
    }
    // dyld-specific problem — scope to macOS.
    if std::env::consts::OS != "macos" {
        return false;
    }

    let prefix = std::env::var("CONDA_PREFIX").unwrap_or_default();
    println!(
        "  {} conda env active ({})",
        "⚠".yellow(),
        prefix.dimmed()
    );
    println!(
        "    Building against conda libs often fails to load on macOS (@rpath errors)."
    );

    // Non-interactive (CI, piped input): default to the safe choice.
    if !std::io::stdin().is_terminal() {
        println!(
            "    {} non-interactive — building with a conda-free environment.",
            "ℹ".blue()
        );
        return true;
    }

    print!("    Build with conda excluded from the environment? [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return true; // can't read — default safe
    }
    let ans = line.trim().to_ascii_lowercase();
    let exclude = !(ans == "n" || ans == "no");
    println!(
        "    {} {}",
        "→".blue(),
        if exclude {
            "building with a conda-free environment".to_string()
        } else {
            "keeping conda on the build environment".to_string()
        }
    );
    exclude
}

/// Strip conda from a build subprocess's environment, mirroring what
/// `conda deactivate` would do — without touching the user's shell.
/// Removes conda marker vars and any conda-prefixed entries from the
/// path-like vars that steer the compiler/linker.
fn sanitize_conda_from_cmd(cmd: &mut Command, prefix: &str) {
    for var in ["CONDA_PREFIX", "CONDA_DEFAULT_ENV", "CONDA_PROMPT_MODIFIER"] {
        cmd.env_remove(var);
    }

    for var in [
        "PKG_CONFIG_PATH",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
    ] {
        if let Ok(val) = std::env::var(var) {
            let cleaned = strip_conda_entries(&val, prefix);
            if cleaned.is_empty() {
                cmd.env_remove(var);
            } else {
                cmd.env(var, cleaned);
            }
        }
    }

    // PATH: only strip conda if R itself doesn't live under the conda prefix —
    // otherwise we'd remove the very R we're about to invoke.
    if let Ok(path) = std::env::var("PATH") {
        if !r_binary_under(prefix) {
            cmd.env("PATH", strip_conda_entries(&path, prefix));
        }
    }
}

/// Drop ':'-separated entries that live under `prefix` (and empties).
fn strip_conda_entries(value: &str, prefix: &str) -> String {
    value
        .split(':')
        .filter(|e| !e.is_empty() && !e.starts_with(prefix))
        .collect::<Vec<_>>()
        .join(":")
}

/// Does the `R` on PATH resolve to a path under `prefix` (i.e. conda's own R)?
fn r_binary_under(prefix: &str) -> bool {
    Command::new("which")
        .arg("R")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().starts_with(prefix))
        .unwrap_or(false)
}
/// Return the set of all package names currently installed in the
/// active R library (venv if active, system library otherwise).
///
/// Bug #28 enabling: the resolver uses this to recognize packages that
/// are installed on disk but not in any registry — e.g. a GitHub-only
/// package installed via a prior `rin install` invocation.
pub fn list_installed_packages() -> std::collections::HashSet<String> {
    use std::collections::HashSet;

    let r_code = match get_venv_lib() {
        Some(lib) => format!(
            "cat(rownames(installed.packages(lib.loc='{}')), sep='\\n')",
            lib.display()
        ),
        None => "cat(rownames(installed.packages()), sep='\\n')".to_string(),
    };

    match r_command()
        .args(["--vanilla", "--slave", "-e", &r_code])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => HashSet::new(), // Fail open: empty set = nothing pre-installed
    }
}
/// Check which packages from the resolved set are already installed
pub fn check_installed_versions(packages: &[ResolvedPackage]) -> Vec<String> {
    if packages.is_empty() {
        return Vec::new();
    }

    // We only verify packages in the resolved set, so embed those names as an
    // R vector. Single quotes can't appear in R package names, so stripping
    // them is enough to keep the literal safe.
    let candidates_r = packages
        .iter()
        .map(|p| format!("'{}'", p.name.replace('\'', "")))
        .collect::<Vec<_>>()
        .join(",");

    // Setup differs only in whether a venv lib dir is prepended to the search
    // path. Prepending (not replacing) lets loadNamespace() resolve a compiled
    // package's dependencies from the venv first, base R packages second.
    let lib_setup = match get_venv_lib() {
        Some(venv_path) => format!(
            ".libPaths(c('{p}', .libPaths())); loc <- '{p}'",
            p = venv_path.display()
        ),
        None => "loc <- NULL".to_string(),
    };

    // installed.packages() only proves a package is *registered*, not that its
    // shared object still loads. A cached build can link against a library that
    // has since left the search path (e.g. a conda lib after `conda deactivate`),
    // leaving a registered-but-unloadable package. For packages that ship
    // compiled code (a `libs/` dir), gate "installed" on loadNamespace()
    // actually succeeding — that triggers the dlopen and surfaces a stale link,
    // so the package is rebuilt instead of silently skipped. Pure-R packages
    // can't hit this and skip the (slower) load probe.
    let r_code = format!(
        "{lib_setup}\n\
         ip <- installed.packages(lib.loc=loc)\n\
         for (p in c({candidates})) {{\n\
           if (!(p %in% rownames(ip))) next\n\
           ver <- ip[p, 'Version']\n\
           dir <- tryCatch(find.package(p, quiet=TRUE), error=function(e) NA_character_)\n\
           ok <- TRUE\n\
           if (!is.na(dir) && dir.exists(file.path(dir, 'libs'))) {{\n\
             ok <- tryCatch({{ loadNamespace(p); TRUE }}, error=function(e) FALSE)\n\
           }}\n\
           if (ok) cat(p, ver, '\\n')\n\
         }}",
        lib_setup = lib_setup,
        candidates = candidates_r,
    );

    let output = r_command()
        .args(["--vanilla", "--slave", "-e", &r_code])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let installed_str = String::from_utf8_lossy(&out.stdout);

            // Build a map of package name → loadable installed version
            let mut installed_map: std::collections::HashMap<&str, &str> =
                std::collections::HashMap::new();
            for line in installed_str.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() == 2 {
                    installed_map.insert(parts[0], parts[1]);
                }
            }

            packages
                .iter()
                .filter(|pkg| {
                    match installed_map.get(pkg.name.as_str()) {
                        Some(installed_ver) => *installed_ver == pkg.version.as_str(),
                        None => false,
                    }
                })
                .map(|pkg| pkg.name.clone())
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Resume a previously failed installation
/// Resume a previously failed installation.
///
/// Strategy: reconstruct the resolved tree from rin.lock, then hand it to
/// `install()`. The installer's existing `check_installed_versions()` skip
/// logic handles "already done" packages automatically — so the retry
/// naturally targets failed + unattempted packages without custom filtering.
///
/// State file is optional (used for messaging). Lockfile is required —
/// without it we'd have to re-resolve, which defeats the point of retry.
pub async fn retry_install() -> Result<()> {
    use colored::Colorize;

    // State file is informational only — installer rebuilds it on success.
    let state = load_install_state().ok();

    let lockfile = crate::lockfile::read("rin.lock").context(
        "retry needs rin.lock. Run `rin install <packages>` to generate one first.",
    )?;

    // Convert locked entries back into ResolvedPackages. Mirrors the same
    // logic in cmd_restore (main.rs), minus the integrity check — retry is
    // resuming the SAME session, so tarballs are already verified-by-download.
    let resolved = ResolvedDeps {
        packages: lockfile
            .packages
            .iter()
            .map(|pkg| {
                let github_source = if pkg.source == "github" {
                    let repo = pkg.repo.as_ref().unwrap();
                    let (owner, repo_name) = repo.split_once('/').unwrap();
                    Some(crate::resolver::GitHubSource {
                        owner: owner.to_string(),
                        repo: repo_name.to_string(),
                        commit_sha: pkg.r#ref.clone().unwrap(),
                        subdir: pkg.subdir.clone(),
                        tarball_sha256: pkg.tarball_sha256.clone().unwrap(),
                    })
                } else {
                    None
                };

                ResolvedPackage {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    source: pkg.source.clone(),
                    needs_compilation: false,
                    dependencies: pkg.deps.clone(),
                    sha256: pkg.sha256.clone(),
                    github_source,
                    system_requirements: None,
                }
            })
            .collect(),
        duration_secs: 0.0,
    };

    match &state {
        Some(s) => println!(
            "{} {} already installed, {} failed previously — re-attempting failures and unattempted packages.",
            "↻".blue(),
            s.installed.len(),
            s.failed.len(),
        ),
        None => println!(
            "{} no prior state — attempting full lockfile.",
            "↻".blue()
        ),
    }

    install(&resolved, &lockfile.metadata.bioc_version).await?;

    println!("\n{} Retry complete.", "✓".green());
    Ok(())
}

// --- State persistence for --retry ---

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct InstallState {
    installed: Vec<String>,
    failed: Vec<(String, String)>,
}

fn save_install_state(
    installed: &HashSet<String>,
    failed: &[(String, String)],
) -> Result<()> {
    let state = InstallState {
        installed: installed.iter().cloned().collect(),
        failed: failed.to_vec(),
    };

    let json = serde_json::to_string_pretty(&state)?;
    std::fs::write(STATE_FILE, json)?;

    Ok(())
}

fn load_install_state() -> Result<InstallState> {
    let content = std::fs::read_to_string(STATE_FILE)
        .context("No install state found. Run `rin install` first.")?;

    let state: InstallState = serde_json::from_str(&content)?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::{explain_symbol_mismatch, guess_system_lib, strip_conda_entries};

    #[test]
    fn symbol_mismatch_identifies_hdf5_and_culprit() {
        // The exact failure shape from SeuratDisk loading a bad hdf5r.so.
        let chunk = "unable to load shared object \
            '/Users/x/.rin/lib/hdf5r/libs/hdf5r.so': \
            dlopen(/Users/x/.rin/lib/hdf5r/libs/hdf5r.so, 0x0006): \
            symbol not found in flat namespace '_H5Dread_chunk'";
        let msg = explain_symbol_mismatch("SeuratDisk", chunk);
        assert!(msg.contains("version mismatch"));
        assert!(msg.contains("hdf5r")); // recovered the offending package
        assert!(msg.contains("_H5Dread_chunk")); // named the symbol
        assert!(msg.contains("hdf5@1.10")); // HDF5-specific remediation
    }

    #[test]
    fn guess_system_lib_maps_prefixes() {
        assert_eq!(guess_system_lib("_H5Dread_chunk"), Some("HDF5"));
        assert_eq!(guess_system_lib("GDALOpen"), Some("GDAL"));
        assert_eq!(guess_system_lib("_proj_create"), Some("PROJ"));
        assert_eq!(guess_system_lib("_some_random_sym"), None);
    }

    #[test]
    fn strips_only_conda_prefixed_entries() {
        let prefix = "/opt/homebrew/anaconda3";
        let path = "/opt/homebrew/anaconda3/bin:/opt/homebrew/bin:/usr/bin:/bin";
        // Conda's bin goes; Homebrew's bin (shares the /opt/homebrew root but
        // not the full prefix) and system dirs stay.
        assert_eq!(
            strip_conda_entries(path, prefix),
            "/opt/homebrew/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn drops_empty_segments() {
        let prefix = "/opt/conda";
        assert_eq!(strip_conda_entries("/opt/conda/lib::/usr/lib", prefix), "/usr/lib");
    }

    #[test]
    fn all_conda_yields_empty() {
        let prefix = "/opt/conda";
        assert_eq!(strip_conda_entries("/opt/conda/lib:/opt/conda/lib/pkgconfig", prefix), "");
    }
}
