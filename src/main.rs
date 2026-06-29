// src/main.rs — The entry point of the rin program.
//
// RUST CONCEPT: main() is where every Rust program starts, just like Python's
// `if __name__ == "__main__"` or C's main(). The `#[tokio::main]` attribute
// makes it async (we need this for HTTP requests to CRAN/Bioconductor).
//
// RUST CONCEPT: `mod` declarations tell Rust "there's a module here."
// Each `mod foo;` means Rust looks for either:
//   - src/foo.rs (single file module), or
//   - src/foo/mod.rs (directory module with sub-modules)
// This is like Python's import system but more explicit.

// Declare our modules — each is a major component of rin
mod cli;        // Command-line argument parsing
mod registry;   // Fetching package metadata from CRAN + Bioconductor
mod resolver;   // Dependency resolution (the brain of rin)
mod sysreq;     // System dependency checking (apt packages)
mod lockfile;   // rin.lock file generation and reading
mod installer;  // Package installation orchestration
mod cache;      // Built-package cache (link instead of recompile across projects)
mod version;
mod sat_resolver;
mod source;
mod bioc_releases;
// `use` brings items into scope — like `from X import Y` in Python
use anyhow::{Context, Result};
use cli::Cli;
use clap::Parser;

// #[tokio::main] transforms this into an async main function.
// Under the hood, it creates a tokio runtime and blocks on this function.
// Without it, you'd have to write:
//   fn main() {
//       let rt = tokio::runtime::Runtime::new().unwrap();
//       rt.block_on(async { ... });
//   }
#[tokio::main]
async fn main() -> Result<()> {
    // Parse command-line arguments.
    // Cli::parse() uses clap to read args from std::env::args().
    // If the user types something invalid, clap prints help and exits.
    let cli = Cli::parse();

    // Match on the subcommand — like a switch statement, but Rust's `match`
    // is exhaustive: the compiler forces you to handle every possible case.
    // This means you can never forget to handle a command.
    match cli.command {
        cli::Commands::Resolve { packages } => {
            // `rin resolve DESeq2 ggplot2`
            cmd_resolve(&packages).await?;
        }
        cli::Commands::Audit { packages } => {
            // `rin audit DESeq2`
            cmd_audit(&packages).await?;
        }
        cli::Commands::Install { packages, retry, skip_sysreq, ignore_missing, strict_sysreq } => {
            cmd_install(&packages, retry, skip_sysreq, ignore_missing, strict_sysreq).await?;
        }
        cli::Commands::Why { package } => {
            // `rin why rlang`
            cmd_why(&package).await?;
        }
        cli::Commands::Lock { packages } => {
            // `rin lock DESeq2 clusterProfiler`
            cmd_lock(&packages).await?;
        }
        cli::Commands::Restore => {
            cmd_restore().await?;
        }
         cli::Commands::Venv { path, r_version } => {
            cmd_venv_create(&path, r_version).await?;
        }
        cli::Commands::VenvInfo => {
            cmd_venv_info()?;
        }
        cli::Commands::VenvRemove { path } => {
            cmd_venv_remove(&path)?;
        }
        cli::Commands::Cache { action } => match action {
            cli::CacheCommands::Dir => {
                println!("{}", cache::cache_root().display());
            }
        },
    }

    // Ok(()) means "everything succeeded, return nothing."
    // RUST CONCEPT: Rust has no exceptions. Instead, functions return
    // Result<T, E> which is either Ok(value) or Err(error).
    // The `?` operator after function calls means "if this returned an error,
    // propagate it up immediately." It's like automatic try/except.
    Ok(())
}

// --- Command Implementations ---
// Each command follows the same pattern:
// 1. Fetch registry metadata
// 2. Do something with it
// 3. Display results

/// Resolve and display the dependency tree
async fn cmd_resolve(packages: &[String]) -> Result<()> {
    use colored::Colorize;

   let parsed: Vec<source::PackageSource> = packages
        .iter()
        .map(|s| source::PackageSource::parse(s))
        .collect::<Result<Vec<_>>>()?;

    println!("{}", "Resolving dependencies...".dimmed());
    let mut registry = registry::Registry::fetch().await?;
    let root_names = sat_resolver::prepare_github_packages(&mut registry, &parsed).await?;
    let resolved = sat_resolver::resolve_with_constraints(&mut registry, &root_names).await?;

    // Display the tree
    println!(
        "\n{} {} packages resolved in {:.1}s\n",
        "✓".green(),
        resolved.packages.len(),
        resolved.duration_secs
    );

    // Print the dependency tree
    for pkg in &resolved.packages {
        let source_label = match pkg.source.as_str() {
            "bioc" => "(bioc)".blue(),
            "cran" => "(cran)".dimmed(),
            "github" => "(github)".magenta(),
            _ => "(unknown)".dimmed(),
        };

        let compile_flag = if pkg.needs_compilation {
            " ⚙ C++".yellow().to_string()
        } else {
            String::new()
        };

        // RUST CONCEPT: format! is like Python's f-strings.
        // println! is a macro (note the !) that prints to stdout.
        println!("  {} {} {}{}", pkg.name, pkg.version, source_label, compile_flag);
    }

    // Print summary
    let bioc_count = resolved.packages.iter().filter(|p| p.source == "bioc").count();
    let cran_count = resolved.packages.iter().filter(|p| p.source == "cran").count();
    let compile_count = resolved.packages.iter().filter(|p| p.needs_compilation).count();

    println!("\n{}", "Summary:".bold());
    println!("  {} from Bioconductor", bioc_count.to_string().blue());
    println!("  {} from CRAN", cran_count);
    println!(
        "  {} need compilation",
        compile_count.to_string().yellow()
    );

    Ok(())
}

/// Apply the macOS Fortran/Makevars fix if R's FLIBS points at a stale
/// gfortran path. Idempotent and a no-op off macOS or when nothing's wrong —
/// safe to call from every code path that precedes a compile. Centralized so
/// `audit`, the install pre-flight, the post-sysreq pass, and `--retry` all
/// behave identically.
fn maybe_fix_makevars() -> Result<()> {
    use colored::Colorize;
    if let Some(fix) = sysreq::check_makevars() {
        println!("{} R's Fortran paths are misconfigured:", "⚠".yellow());
        println!("  R looks in:      {}", fix.bad_paths.join(", "));
        println!("  gfortran is at:  {}", fix.correct_lib);
        println!(
            "  {} writing fix to {}",
            "→".blue(),
            fix.makevars_path.display()
        );
        sysreq::fix_makevars(&fix)?;
        println!("  {} Makevars updated.\n", "✓".green());
    }
    Ok(())
}

/// Audit system dependencies before installing
async fn cmd_audit(packages: &[String]) -> Result<()> {
    use colored::Colorize;

    // Bare `rin audit` (no packages) audits the current environment: fall back
    // to the roots recorded in rin.lock. This is what users reach for after a
    // failed install — and it's exactly what rin's own error hints suggest
    // ("Run rin audit for details"), so it must work without arguments.
    let packages: Vec<String> = if packages.is_empty() {
        match lockfile::read("rin.lock") {
            Ok(lock) if !lock.metadata.roots.is_empty() => lock.metadata.roots.clone(),
            Ok(lock) => lock.packages.iter().map(|p| p.name.clone()).collect(),
            Err(_) => {
                anyhow::bail!(
                    "no packages given and no rin.lock found.\n  \
                     Run `rin audit <packages>` or `rin audit` from a directory with an rin.lock."
                );
            }
        }
    } else {
        packages.to_vec()
    };

    let parsed: Vec<source::PackageSource> = packages
        .iter()
        .map(|s| source::PackageSource::parse(s))
        .collect::<Result<Vec<_>>>()?;

    println!("{}", "Resolving dependencies...".dimmed());
    let mut registry = registry::Registry::fetch().await?;
    let root_names = sat_resolver::prepare_github_packages(&mut registry, &parsed).await?;
    let resolved = sat_resolver::resolve_with_constraints(&mut registry, &root_names).await?;

    println!("{}", "Checking system dependencies...".dimmed());
    let report = sysreq::audit(&resolved).await?;
    
    // Display results
    for dep in &report.found {
        println!("  {} {} {}", "✓".green(), dep.name, dep.version.dimmed());
    }
    for dep in &report.missing {
        println!(
            "  {} {} — needed by: {}",
            "✗".red(),
            dep.name.red(),
            dep.needed_by.join(", ").dimmed()
        );
    }
    
    if !report.missing.is_empty() {
        if sysreq::is_hpc_environment() {
            println!("\n{} HPC environment — resolve via the module system:", "Fix with:".bold());
            for dep in &report.missing {
                println!("  module spider {}", sysreq::module_hint(&dep.name));
            }
            println!("  module load <name>/<version>   # for each lib above");
        } else if std::env::consts::OS == "macos" {
            println!("\n{}\n  brew install {}", "Fix with:".bold(),
                report.missing.iter()
                    .map(|d| sysreq::get_brew_name(&d.name))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        } else {
            println!("\n{}\n  sudo apt install {}", "Fix with:".bold(),
                report.missing.iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }
    // Offer to fix Makevars if needed
    maybe_fix_makevars()?;

    Ok(())
}

/// Compute the dependency closure of `roots` within an already-resolved set.
///
/// Returns a `ResolvedDeps` containing only the requested packages plus
/// everything they transitively depend on, preserving the deps-first install
/// order. This is what `rin install X` actually installs — so an unrelated
/// broken package elsewhere in rin.lock (e.g. another project root) never gets
/// re-attempted or blocks the install.
fn requested_closure(
    resolved: &resolver::ResolvedDeps,
    roots: &[String],
) -> resolver::ResolvedDeps {
    use std::collections::{HashSet, VecDeque};

    let by_name: std::collections::HashMap<&str, &resolver::ResolvedPackage> =
        resolved.packages.iter().map(|p| (p.name.as_str(), p)).collect();

    let mut keep: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = roots.iter().cloned().collect();
    while let Some(name) = queue.pop_front() {
        if !keep.insert(name.clone()) {
            continue;
        }
        if let Some(pkg) = by_name.get(name.as_str()) {
            for dep in &pkg.dependencies {
                if !keep.contains(dep) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }

    let packages = resolved
        .packages
        .iter()
        .filter(|p| keep.contains(&p.name))
        .cloned()
        .collect();
    resolver::ResolvedDeps {
        packages,
        duration_secs: resolved.duration_secs,
    }
}

/// True when the on-disk rin.lock already matches the roots + resolved versions
/// we'd otherwise write — so a re-install is a genuine no-op and we can skip the
/// rewrite (and the misleading "Wrote rin.lock" line).
fn lock_is_current(
    existing: Option<&lockfile::Lockfile>,
    roots: &[String],
    resolved: &resolver::ResolvedDeps,
) -> bool {
    let Some(existing) = existing else {
        return false;
    };
    let existing_roots: std::collections::HashSet<&str> =
        existing.metadata.roots.iter().map(String::as_str).collect();
    let new_roots: std::collections::HashSet<&str> =
        roots.iter().map(String::as_str).collect();
    if existing_roots != new_roots {
        return false;
    }
    let existing_pkgs: std::collections::HashSet<(&str, &str)> = existing
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    let new_pkgs: std::collections::HashSet<(&str, &str)> = resolved
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    existing_pkgs == new_pkgs
}

/// Install packages
async fn cmd_install(
    packages: &[String],
    retry: bool,
    skip_sysreq: bool,
    ignore_missing: Vec<String>,
    strict_sysreq: bool,
) -> Result<()> {
    use colored::Colorize;

    // Day 1: parse package specs (registry name vs. gh:user/repo). 
    // Resolver wiring comes later — for now we just verify the parser
    // and bail early if the user asked for a GitHub package.
    if retry {
        // Bug #50c: apply the Makevars fix before retrying. The retry path used
        // to short-circuit straight into retry_install(), skipping every Fortran
        // fix below — so `rin install --retry` rebuilt RcppArmadillo with the
        // same stale FLIBS and failed identically. gfortran is reliably present
        // by retry time (the first run's pre-flight installed it), so this now
        // detects and rewrites the bad path before the rebuild.
        maybe_fix_makevars()?;
        println!("{}", "Retrying failed packages...".dimmed());
        installer::retry_install().await?;
        return Ok(());
    }
    // Bug #50: auto-apply Makevars fix in install path (not just audit).
    // The fix exists in sysreq::check_makevars() but was only wired into
    // `rin audit`. Users mostly skip audit and go straight to install, so
    // the Fortran-path mismatch bit them on install with a confusing error.
    // Run the same auto-fix at install start so RcppArmadillo / Matrix /
    // RcppEigen / fracdiff / SparseM / minqa work first try on macOS.
    maybe_fix_makevars()?;
    // Bug #14: warn when gcc is too new for older R packages.
    if let Some((major, _)) = sysreq::detect_gcc_version() {
        if major >= 15 {
            println!(
                "{} gcc {} detected. Many R packages (esp. older Bioconductor)",
                "⚠".yellow(),
                major
            );
            println!("  have known compatibility issues with gcc 15+ (stricter template rules).");
            if sysreq::is_hpc_environment() {
                println!("  Recommended: {}", "module load gcc/12".bold());
                println!("    {}", "module spider gcc   # see versions available".dimmed());
            } else if std::env::consts::OS == "macos" {
                println!("  Recommended: use Apple's clang (default) or {}", "brew install gcc@12".bold());
            } else {
                println!("  Recommended: use the distro's gcc-12 / gcc-13 package.");
            }
            println!("  If you hit confusing template errors, swap compilers and `rin install --retry`.\n");
        }
    }

    // Note: the macOS "Homebrew not detected" guidance now fires contextually,
    // only when a sysreq is actually missing and we'd have used brew to install
    // it (see the advisory branch below). That avoids nagging on every install
    // and lets the message name the specific libraries to `brew install`.

    // Parse package specs — registry names pass through, GitHub specs
    // get their metadata fetched and inserted into the registry below.
    let parsed: Vec<source::PackageSource> = packages
        .iter()
        .map(|s| source::PackageSource::parse(s))
        .collect::<Result<Vec<_>>>()?;
    // Warn if installing GitHub packages outside a venv.
    if !retry {
        let has_github = parsed
            .iter()
            .any(|p| matches!(p, source::PackageSource::GitHub(_)));
        let venv_active = std::env::var("RIN_VENV").is_ok()
            || std::path::Path::new(".rin/lib").exists();
        if has_github && !venv_active {
            println!(
                "\n{} installing GitHub package outside a virtual environment.",
                "warning:".yellow().bold()
            );
            println!(
                "  GitHub packages may shadow CRAN versions in your system library."
            );
            println!("  Consider running `rin venv` first.\n");
        }
    }

    println!("{}", "Resolving dependencies...".dimmed());
    let mut registry = registry::Registry::fetch().await?;

   

    // rin.lock is a full project manifest: all roots ever requested, plus their
    // resolved closures, so `rin restore` can rebuild the whole environment.
    // We therefore merge this invocation's packages into the existing roots and
    // resolve everything together (keeps versions consistent across the project)
    // — but the *installation* below is scoped to just what was asked for now.
    let existing_lock = lockfile::read("rin.lock").ok();
    let mut all_roots: Vec<String> = packages.to_vec();
    if let Some(existing) = &existing_lock {
        let prior_roots: Vec<String> = if existing.metadata.roots.is_empty() {
            // Old-format lockfile — fall back to using all packages as roots
            // so we don't silently lose them on incremental install.
            existing.packages.iter().map(|p| p.name.clone()).collect()
        } else {
            existing.metadata.roots.clone()
        };
        for r in prior_roots {
            if !all_roots.contains(&r) {
                all_roots.push(r);
            }
        }
    }

    let parsed: Vec<source::PackageSource> = all_roots
        .iter()
        .map(|s| source::PackageSource::parse(s))
        .collect::<Result<Vec<_>>>()?;
    let root_names = sat_resolver::prepare_github_packages(&mut registry, &parsed).await?;

    let resolved = sat_resolver::resolve_with_constraints(&mut registry, &root_names).await?;

    // `root_names` is 1:1 with `all_roots` (requested first, then prior roots),
    // so the first `packages.len()` entries are THIS invocation's requested
    // packages — resolved to real names (handles gh: specs too). Their closure
    // is all we install and all we fail on.
    let requested_roots: Vec<String> =
        root_names.iter().take(packages.len()).cloned().collect();
    let scoped = requested_closure(&resolved, &requested_roots);

    // Write rin.lock (the full project) only when it actually changes — avoids a
    // misleading "Wrote rin.lock" on a no-op re-install. We still write the full
    // `resolved` so `rin why` / `rin restore` see the whole project.
    if lock_is_current(existing_lock.as_ref(), &all_roots, &resolved) {
        println!("  {} rin.lock unchanged", "·".dimmed());
    } else {
        let lockfile_path = lockfile::write(
            &resolved,
            &all_roots,
            &registry.r_version,
            &registry.bioc_version,
        )?;
        println!(
            "  {} Wrote {} ({} packages)",
            "→".dimmed(),
            lockfile_path.display(),
            resolved.packages.len()
        );
    }
    // Bug #44 + #49: warn about HDF5/MPI module misconfigurations before
    // the user wastes time waiting for hdf5r to fail.
    let pkg_names: Vec<String> = scoped.packages.iter().map(|p| p.name.clone()).collect();
    if let Some(warning) = sysreq::detect_hdf5_problem(&pkg_names) {
        println!("\n{} {}\n", "⚠".yellow(), warning);
    }

    // Pre-flight system dependency check
    // ── Pre-flight system dependency check (Bugs #1, #2) ───────────────
    if skip_sysreq {
        println!(
            "{} skipping system-requirements check (--skip-sysreq).",
            "⚠".yellow()
        );
    } else {
        println!("{}", "Pre-flight check: system dependencies...".dimmed());
        let mut report = sysreq::audit(&scoped).await?;

        // #2: surgical override — drop user-named libs from the missing list.
        if !ignore_missing.is_empty() {
            let ignored: Vec<String> = report
                .missing
                .iter()
                .filter(|d| ignore_missing.contains(&d.name))
                .map(|d| d.name.clone())
                .collect();
            report.missing.retain(|d| !ignore_missing.contains(&d.name));
            for name in &ignored {
                println!("  {} {} (--ignore-missing)", "↷".dimmed(), name.dimmed());
            }
        }

        if !report.missing.is_empty() {
            if strict_sysreq {
                // --strict-sysreq: original gatekeeping behavior (pre-#41).
                // Useful for CI / automated builds that want to fail fast.
                println!(
                    "\n{} Missing {} system librar{}:",
                    "✗".red(),
                    report.missing.len(),
                    if report.missing.len() == 1 { "y" } else { "ies" }
                );
                for dep in &report.missing {
                    println!(
                        "  {} — needed by: {}",
                        dep.name.red(),
                        dep.needed_by.join(", ")
                    );
                }
                handle_missing_sysreqs(&report, &packages.join(" "))?;
            } else {
                // Bug #41 default: advisory pre-flight, not gatekeeping.
                //
                // The compiler is the source of truth — if a package builds,
                // the sysreq was satisfied (regardless of how rin detected it).
                // RSPM frequently over-reports (pandoc as a runtime-only dep,
                // libX11 for optional clipboard features, etc.); rin shouldn't
                // second-guess the compiler with false-positive blocks.
                //
                // Real blockers will surface as compile errors with actionable
                // messages; user fixes those (module load / conda / Makevars)
                // and `rin install --retry` resumes from where it stopped.
                println!(
                    "\n{} {} potentially missing system librar{} (advisory):",
                    "ℹ".blue(),
                    report.missing.len(),
                    if report.missing.len() == 1 { "y" } else { "ies" }
                );
                for dep in &report.missing {
                    println!(
                        "  {} — needed by: {}",
                        dep.name.dimmed(),
                        dep.needed_by.join(", ").dimmed()
                    );
                }

                if sysreq::is_hpc_environment() {
                    println!(
                        "\n  {} If a build fails on these, load via the module system:",
                        "→".dimmed()
                    );
                    for dep in &report.missing {
                        println!("    module spider {}", sysreq::module_hint(&dep.name));
                    }
                }

                // Bug #53: on macOS with brew, offer auto-install of advisory libs.
                // Safe here — homebrew is user-scoped, no sudo. Default to yes:
                // the user is mid-install and probably wants their packages to build.
                // Bug #53 + #53b: offer auto-install of advisory libs.
                // macOS uses brew (no sudo). Linux non-HPC uses apt/dnf (may sudo-prompt).
                // HPC skipped — no sudo on clusters, module hints already shown above.
                let can_auto_install = match std::env::consts::OS {
                    "macos" => sysreq::has_brew(),
                    _ => !sysreq::is_hpc_environment(),
                };

                if can_auto_install {
                    use std::io::{self, IsTerminal, Write};

                    // Decide whether to auto-install the flagged libs. A
                    // non-interactive console (RStudio, CI) can't answer a
                    // [Y/n] prompt — reading its EOF stdin and treating it as
                    // "yes" silently installs system packages behind a prompt
                    // the user never actually saw. Instead, detect that case
                    // up front (like decide_conda_exclusion does) and announce
                    // the auto-install explicitly.
                    let do_install = if io::stdin().is_terminal() {
                        // Interactive: ask, default Yes.
                        let prompt = if std::env::consts::OS == "macos" {
                            "brew install these now? [Y/n] "
                        } else {
                            "install these now (may prompt for sudo password)? [Y/n] "
                        };
                        print!("\n  {} {}", "?".blue(), prompt);
                        io::stdout().flush()?;

                        let mut answer = String::new();
                        io::stdin().read_line(&mut answer)?;
                        let answer = answer.trim().to_lowercase();
                        answer.is_empty() || answer == "y" || answer == "yes"
                    } else {
                        // Non-interactive: announce, then proceed (Option B).
                        let how = if std::env::consts::OS == "macos" {
                            "brew"
                        } else {
                            "the system package manager"
                        };
                        println!(
                            "\n  {} Non-interactive session (RStudio/CI) — auto-installing the missing librar{} via {}:",
                            "ℹ".blue(),
                            if report.missing.len() == 1 { "y" } else { "ies" },
                            how
                        );
                        for dep in &report.missing {
                            println!("    {} {}", "+".dimmed(), dep.name.dimmed());
                        }
                        true
                    };

                    if do_install {
                        match sysreq::install_missing(&report) {
                            Ok(()) => println!("  {} install completed.", "✓".green()),
                            Err(e) => println!(
                                "  {} install failed: {}\n    Continuing — compile errors will surface real blockers.",
                                "⚠".yellow(),
                                e
                            ),
                        }
                    }
                } else if std::env::consts::OS == "macos" && !sysreq::has_brew() {
                    // brew is how rin auto-installs sysreqs on macOS; without it
                    // we can't. Spell out exactly what to do — install brew, the
                    // specific libraries, then retry — instead of leaving a new
                    // user to decode a C compile error. Replaces the generic
                    // "Homebrew not detected" startup notice (removed) with a
                    // contextual one that names the actual missing libraries.
                    let brew_pkgs: Vec<String> = report
                        .missing
                        .iter()
                        .map(|d| sysreq::get_brew_name(&d.name))
                        .collect();
                    let plural = if brew_pkgs.len() == 1 { "y" } else { "ies" };
                    println!(
                        "\n  {} Homebrew isn't installed, so rin can't auto-install these for you.",
                        "⚠".yellow()
                    );
                    println!("    To fix:");
                    println!("      1. Install Homebrew:");
                    println!(
                        r#"           /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)""#
                    );
                    println!("      2. Install the librar{}:", plural);
                    println!("           brew install {}", brew_pkgs.join(" "));
                    println!(
                        "      3. Resume:  {}   (or {} in RStudio)",
                        "rin install --retry".bold(),
                        "rin::install(retry=TRUE)".bold()
                    );
                }

                println!(
                    "\n  {} Proceeding — compile errors will surface real blockers.",
                    "→".dimmed()
                );
                println!(
                    "    {} blocks install on any flagged sysreq (CI-friendly).",
                    "--strict-sysreq".bold()
                );
                println!(
                    "    {} silences this advisory entirely.\n",
                    "--skip-sysreq".bold()
                );
            }
        }
        if !report.uncertain.is_empty() {
            println!(
                "\n{} Could not verify sysreqs for {} compiled package{}:",
                "ℹ".blue(),
                report.uncertain.len(),
                if report.uncertain.len() == 1 { "" } else { "s" },
            );
            for u in &report.uncertain {
                println!(
                    "  {} declares: {}",
                    u.name.bold(),
                    u.libs.join(", ").dimmed()
                );
            }
            println!(
                "  rin couldn't map these automatically; check they're installed \
                 (e.g. via module/conda/brew), or re-run with {} to silence this.",
                "--skip-sysreq".bold()
            );
        }

    }

    // Bug #50b: re-run the Makevars fix AFTER the pre-flight sysreq install.
    //
    // The first attempt (above, near install start) runs before gfortran may
    // exist on the machine. check_makevars() bails early when `gfortran` isn't
    // on PATH, so on a fresh box it returns None and writes nothing. The
    // pre-flight check then `brew install`s gcc/gfortran — but by that point the
    // Makevars fix was already skipped, and R's FLIBS still points at the
    // nonexistent /opt/gfortran/lib. RcppArmadillo / Matrix / RcppEigen then
    // fail to link gfortran on first build.
    //
    // Re-running here closes that window: now that gfortran is installed, write
    // the correct FLIBS path before any package builds. Idempotent — returns
    // None if the earlier pass already wrote the fix.
    maybe_fix_makevars()?;

    println!("\n{}", "Installing packages...".dimmed());
    let built = installer::install(&scoped, &registry.bioc_version).await?;

    // Tell the truth: only say "installed" when something was actually built.
    if built == 0 {
        println!(
            "\n{} Already up to date — nothing to install.",
            "✓".green()
        );
    } else {
        println!(
            "\n{} Installed {} package(s) successfully.",
            "✓".green(),
            built
        );
    }

    // Contextual nudge to `rin restore`: if the project (full resolved set) has
    // packages outside what we just installed that aren't built, the user has a
    // partially-built project. Point them at the one command that fixes it,
    // teaching `restore` exactly when it matters.
    let scoped_names: std::collections::HashSet<&str> =
        scoped.packages.iter().map(|p| p.name.as_str()).collect();
    let others: Vec<resolver::ResolvedPackage> = resolved
        .packages
        .iter()
        .filter(|p| !scoped_names.contains(p.name.as_str()))
        .cloned()
        .collect();
    if !others.is_empty() {
        let installed = installer::check_installed_versions(&others);
        let unbuilt = others.len() - installed.len();
        if unbuilt > 0 {
            println!(
                "\n{} {} other package(s) in rin.lock aren't built yet.",
                "ℹ".blue(),
                unbuilt
            );
            println!(
                "  Run {} (or {} in RStudio) to build the whole project.",
                "rin restore".bold(),
                "rin::restore()".bold()
            );
        }
    }

    Ok(())
}

/// Explain why a package is in the dependency tree
async fn cmd_why(package: &str) -> Result<()> {
    use colored::Colorize;

    // Read the lockfile — the source of truth for what's resolved.
    let lockfile = lockfile::read("rin.lock").context(
        "rin why needs an rin.lock. Run `rin lock <packages>` first.",
    )?;

    // Index packages by name for fast lookup.
    let by_name: std::collections::HashMap<&str, &lockfile::LockedPackage> = lockfile
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();

    // Verify the target is actually in the tree.
    if !by_name.contains_key(package) {
        println!(
            "{} is not in the current dependency tree.",
            package.yellow()
        );
        return Ok(());
    }

    // Find every package that depends directly on `package`.
    // For deeper paths, repeat upward until we hit a package nothing depends on.
    let dependents_of = |target: &str| -> Vec<&str> {
        lockfile
            .packages
            .iter()
            .filter(|p| p.deps.iter().any(|d| d == target))
            .map(|p| p.name.as_str())
            .collect()
    };

    // Build all paths from a root (no dependents) down to the target.
    fn build_paths<'a>(
        target: &'a str,
        by_name: &std::collections::HashMap<&str, &'a lockfile::LockedPackage>,
        get_dependents: &impl Fn(&str) -> Vec<&'a str>,
        seen: &mut std::collections::HashSet<&'a str>,
    ) -> Vec<Vec<&'a str>> {
        if !seen.insert(target) {
            return vec![]; // cycle guard
        }
        let dependents = get_dependents(target);
        if dependents.is_empty() {
            seen.remove(target);
            return vec![vec![target]]; // root
        }
        let mut all_paths = Vec::new();
        for dep in dependents {
            for mut sub in build_paths(dep, by_name, get_dependents, seen) {
                sub.push(target);
                all_paths.push(sub);
            }
        }
        seen.remove(target);
        all_paths
    }

    let mut seen = std::collections::HashSet::new();
    let paths = build_paths(package, &by_name, &dependents_of, &mut seen);

    // Format a single package line: name version (source)
    let format_pkg = |name: &str| -> String {
        match by_name.get(name) {
            Some(pkg) => {
                let label = match pkg.source.as_str() {
                    "github" => {
                        let repo = pkg.repo.as_deref().unwrap_or("?");
                        let short_sha = pkg
                            .r#ref
                            .as_deref()
                            .map(|s| &s[..7.min(s.len())])
                            .unwrap_or("?");
                        format!("github: {}@{}", repo, short_sha).magenta().to_string()
                    }
                    "bioc" => "bioc".blue().to_string(),
                    "cran" => "cran".dimmed().to_string(),
                    other => other.dimmed().to_string(),
                };
                format!("{} {} ({})", name.bold(), pkg.version.dimmed(), label)
            }
            None => name.bold().to_string(),
        }
    };

    println!("Why is {} needed:\n", package.bold());

    if paths.is_empty() {
        // The target is itself a root — printed as a single line.
        println!("{}", format_pkg(package));
        println!("  {}", "(top-level dependency)".dimmed());
    } else {
        for path in &paths {
            for (i, step) in path.iter().enumerate() {
                let indent = "  ".repeat(i);
                let arrow = if i > 0 { "└── " } else { "" };
                println!("{}{}{}", indent, arrow, format_pkg(step));
            }
            println!();
        }
    }

    Ok(())
}

/// Generate a lockfile
async fn cmd_lock(packages: &[String]) -> Result<()> {
    use colored::Colorize;

    let parsed: Vec<source::PackageSource> = packages
        .iter()
        .map(|s| source::PackageSource::parse(s))
        .collect::<Result<Vec<_>>>()?;

    println!("{}", "Resolving dependencies...".dimmed());
    let mut registry = registry::Registry::fetch().await?;
    let root_names = sat_resolver::prepare_github_packages(&mut registry, &parsed).await?;
    let resolved = sat_resolver::resolve_with_constraints(&mut registry, &root_names).await?;

    let lockfile_path = lockfile::write(
        &resolved,
        packages,
        &registry.r_version,
        &registry.bioc_version,
    )?;

    println!(
        "\n{} Written to {} ({} packages)",
        "✓".green(),
        lockfile_path.display(),
        resolved.packages.len()
    );

    Ok(())
}
/// Restore packages from rin.lock
async fn cmd_restore() -> Result<()> {
    use colored::Colorize;

    // Read the lockfile
    println!("{}", "Reading rin.lock...".dimmed());
    let lockfile = lockfile::read("rin.lock")?;

    println!(
        "  Found {} packages for R {} / Bioconductor {}",
        lockfile.packages.len(),
        lockfile.metadata.r_version,
        lockfile.metadata.bioc_version
    );

    // ── Verify integrity of every GitHub-sourced lockfile entry ──────────
    // Re-download each pinned tarball and check SHA-256 against the lockfile.
    // Mismatch → bail. This catches lockfile tampering and upstream rewrites
    // before any install activity.
    let github_count = lockfile.packages.iter().filter(|p| p.source == "github").count();

    if github_count > 0 {
        println!("{}", "Verifying GitHub package integrity...".dimmed());

        let cache_dir = match std::env::var("HOME") {
            Ok(h) => std::path::PathBuf::from(h).join(".rin").join("cache"),
            Err(_) => std::env::temp_dir().join("rin-cache"),
        };
        std::fs::create_dir_all(&cache_dir)?;
        let client = reqwest::Client::new();

        for entry in &lockfile.packages {
            if entry.source != "github" {
                continue;
            }

            let repo = entry.repo.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} is source=github but lockfile has no repo field",
                    entry.name
                )
            })?;
            let r#ref = entry.r#ref.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} is source=github but lockfile has no ref field",
                    entry.name
                )
            })?;
            let expected_sha256 = entry.tarball_sha256.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} is source=github but lockfile has no tarball_sha256 — \
                     cannot verify integrity",
                    entry.name
                )
            })?;

            let (owner, repo_name) = repo.split_once('/').ok_or_else(|| {
                anyhow::anyhow!("Invalid repo field in lockfile: '{}'", repo)
            })?;

            let spec = source::GitHubSpec {
                owner: owner.to_string(),
                repo: repo_name.to_string(),
                r#ref: Some(r#ref.clone()),
                subdir: entry.subdir.clone(),
            };

            let short = &r#ref[..7.min(r#ref.len())];
            print!("  {} gh:{}@{} ", "verifying".dimmed(), repo, short);

            let (_path, actual_sha256) =
                registry::github::download_tarball(&spec, r#ref, &cache_dir, &client)
                    .await
                    .with_context(|| {
                        format!("Failed to download {} for verification", entry.name)
                    })?;

            if &actual_sha256 != expected_sha256 {
                println!("{}", "✗".red());
                anyhow::bail!(
                    "SHA-256 mismatch for {} ({}@{}):\n  \
                     expected (lockfile): {}\n  \
                     actual (downloaded): {}\n  \
                     The lockfile may be corrupted, or the upstream tarball was rewritten.",
                    entry.name, repo, r#ref, expected_sha256, actual_sha256
                );
            }
            println!("{}", "✓".green());
        }
    }

    // ── Convert locked packages into ResolvedPackages for the installer ──
    // Populate github_source so the installer knows to use the cached tarball.
    let resolved = resolver::ResolvedDeps {
        packages: lockfile
            .packages
            .iter()
            .map(|pkg| {
                let github_source = if pkg.source == "github" {
                    let repo = pkg.repo.as_ref().unwrap();
                    let (owner, repo_name) = repo.split_once('/').unwrap();
                    Some(resolver::GitHubSource {
                        owner: owner.to_string(),
                        repo: repo_name.to_string(),
                        commit_sha: pkg.r#ref.clone().unwrap(),
                        subdir: pkg.subdir.clone(),
                        tarball_sha256: pkg.tarball_sha256.clone().unwrap(),
                    })
                } else {
                    None
                };

                resolver::ResolvedPackage {
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

    // ── Skip-already-installed optimization (preserved from before) ──────
    println!("{}", "Checking installed packages...".dimmed());
    let already_installed = installer::check_installed_versions(&resolved.packages);

    let to_install: Vec<&resolver::ResolvedPackage> = resolved
        .packages
        .iter()
        .filter(|pkg| !already_installed.contains(&pkg.name))
        .collect();

    if to_install.is_empty() {
        println!(
            "\n{} All {} packages already installed at correct versions.",
            "✓".green(),
            resolved.packages.len()
        );
        return Ok(());
    }

    println!(
        "\n  {} already installed, {} to install",
        already_installed.len(),
        to_install.len()
    );

    installer::install(&resolved, &lockfile.metadata.bioc_version).await?;

    println!(
        "\n{} Environment restored from rin.lock ({} packages)",
        "✓".green(),
        resolved.packages.len()
    );

    Ok(())
}

/// Create a virtual environment
async fn cmd_venv_create(path: &str, r_version: Option<String>) -> Result<()> {
    use colored::Colorize;

    let venv_dir = std::path::PathBuf::from(path);
    let lib_dir = venv_dir.join("lib");

    if lib_dir.exists() {
        println!("{} Virtual environment already exists at {}/", "✓".green(), path);
        return Ok(());
    }

    // Determine R version
    let r_ver = match r_version {
        Some(v) => v,
        None => {
            // Auto-detect from system
            let output = crate::installer::r_command()
                .args(["--vanilla", "--slave", "-e", "cat(paste0(R.version$major, '.', R.version$minor))"])
                .output();
            match output {
                Ok(out) if out.status.success() => {
                    String::from_utf8_lossy(&out.stdout).trim().to_string()
                }
                _ => {
                    anyhow::bail!("R not found. Specify R version manually: rin venv --r-version 4.4.0");
                }
            }
        }
    };

    // Create directories
    std::fs::create_dir_all(&lib_dir)?;

    // Write config file
    let config = format!(
        "# rin virtual environment\n\
         r_version = \"{}\"\n\
         created = \"{}\"\n",
        r_ver,
        lockfile::chrono_now()
    );
    std::fs::write(venv_dir.join("config.toml"), config)?;

    // Write activate script (bash/zsh)
    let abs_lib = std::fs::canonicalize(&lib_dir).unwrap_or(lib_dir.clone());
    let abs_venv = std::fs::canonicalize(&venv_dir).unwrap_or(venv_dir.clone());

    let activate_content = format!(
        r#"#!/bin/sh
# rin virtual environment activation script
# Usage: source {path}/activate

# Save old values for deactivate
export _RIN_OLD_R_LIBS_USER="${{R_LIBS_USER:-}}"
export _RIN_OLD_PS1="${{PS1:-}}"

# Set the library path
export R_LIBS_USER="{lib}"
export RIN_VENV="{venv}"

# Update prompt to show active environment
export PS1="(rin:{name}) $PS1"

# Define deactivate function
deactivate() {{
    export R_LIBS_USER="$_RIN_OLD_R_LIBS_USER"
    export PS1="$_RIN_OLD_PS1"
    unset RIN_VENV
    unset _RIN_OLD_R_LIBS_USER
    unset _RIN_OLD_PS1
    unset -f deactivate
    echo "rin environment deactivated"
}}

echo "rin environment active (R {r_ver})"
echo "  Library: {lib}"
echo "  Deactivate: deactivate"
"#,
        path = path,
        lib = abs_lib.display(),
        venv = abs_venv.display(),
        name = venv_dir.file_name().unwrap_or_default().to_string_lossy(),
        r_ver = r_ver
    );
    std::fs::write(venv_dir.join("activate"), activate_content)?;

    // Write .gitignore for the venv
    std::fs::write(venv_dir.join(".gitignore"), "lib/\n")?;

    println!("{} Created virtual environment at {}/", "✓".green(), path);
    println!("  R version: {}", r_ver);
    println!("  Library:   {}/lib/", path);
    println!(
        "\n  To activate:\n    {}",
        format!("source {}/activate", path).bold()
    );
    println!(
        "  To deactivate:\n    {}",
        "deactivate".bold()
    );

    Ok(())
}

/// Show info about active virtual environment
fn cmd_venv_info() -> Result<()> {
    use colored::Colorize;

    match std::env::var("RIN_VENV") {
        Ok(path) => {
            let lib_path = std::path::PathBuf::from(&path);
            let count = std::fs::read_dir(&lib_path)
                .map(|entries| entries.filter(|e| e.is_ok()).count())
                .unwrap_or(0);

            println!("  Active environment: {}", path);
            println!("  Packages installed: {}", count);
            println!("  Deactivate: deactivate");
        }
        Err(_) => {
            // Check if .rin exists but isn't activated
            if std::path::Path::new(".rin/lib").exists() {
                println!("{} Virtual environment exists but is not activated", "!".yellow());
                println!("  Run: source .rin/activate");
            } else {
                println!("{} No virtual environment found", "✗".red());
                println!("  Run: rin venv");
            }
        }
    }

    Ok(())
}

/// Remove a virtual environment
fn cmd_venv_remove(path: &str) -> Result<()> {
    use colored::Colorize;

    let venv_dir = std::path::PathBuf::from(path);
    if venv_dir.exists() {
        std::fs::remove_dir_all(&venv_dir)?;
        println!("{} Removed {}/", "✓".green(), path);
    } else {
        println!("{} No virtual environment at {}/", "✗".red(), path);
    }

    Ok(())
}

/// Decide what to do about missing system libraries — environment-aware.
///
/// HPC (Lmod/Modules detected): no sudo available. Show `module spider`
/// guidance and prompt [s/N] — skip-and-trust-modules, or abort.
///
/// Non-HPC: prompt [Y/n/s] — install via apt/brew, abort, or skip-and-continue.
///
/// In both branches, surface --skip-sysreq / --ignore-missing at the moment
/// they become useful (Bug #2's UX requirement: don't make users dig in --help).
fn handle_missing_sysreqs(
    report: &sysreq::SysreqReport,
    install_cmd_tail: &str,
) -> Result<()> {
    use colored::Colorize;
    use std::io::{self, Write};

    if sysreq::is_hpc_environment() {
        println!(
            "\n{} HPC environment detected (Lmod/Modules).",
            "ℹ".blue()
        );
        println!("  rin can't install system libraries on a cluster (no sudo).");
        println!("  Resolve through the module system:");
        for dep in &report.missing {
                println!("  module spider {}", sysreq::module_hint(&dep.name));
            }
        println!("    module load <name>/<version>   # for each lib above\n");

        print!(
            "{} [s/N] {} ",
            "Already loaded the right modules? Skip the check?".bold(),
            "(default: abort)".dimmed()
        );
        io::stdout().flush()?;

        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        match answer.trim().to_lowercase().as_str() {
            "s" | "skip" => {
                println!(
                    "{} trusting loaded modules.\n  \
                     Tip: pass {} next time: rin install {} --skip-sysreq",
                    "⚠".yellow(),
                    "--skip-sysreq".bold(),
                    install_cmd_tail
                );
                Ok(())
            }
            _ => anyhow::bail!(
                "Aborted: load the required modules and re-run, or pass --skip-sysreq."
            ),
        }
    } else {
        print!(
            "\n{} [Y/n/s] {} ",
            "Install missing libraries now?".bold(),
            "(s = skip and continue without them)".dimmed()
        );
        io::stdout().flush()?;

        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        match answer.trim().to_lowercase().as_str() {
            "n" | "no" => anyhow::bail!(
                "Aborted by user. Install system deps manually and re-run."
            ),
            "s" | "skip" => {
                println!(
                    "{} proceeding without installing system libs.\n  \
                     Tip: pass {} next time, or {} to skip a single lib.",
                    "⚠".yellow(),
                    "--skip-sysreq".bold(),
                    "--ignore-missing <LIB>".bold()
                );
                Ok(())
            }
            _ => sysreq::install_missing(report),
        }
    }
}


