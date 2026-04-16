// src/sysreq.rs — System dependency checking and resolution
//
// This module maps R packages to the system libraries they need
// (like libhdf5-dev, libcurl4-openssl-dev) and checks if they're installed.
//
// It uses a curated mapping database — similar to the one at
// https://github.com/rstudio/r-system-requirements
//
// RUST CONCEPT: Static Data
// The SYSREQ_MAP is hardcoded here for the MVP. In production, you'd
// fetch this from a JSON file or API. Rust's `const` and `static` are
// for compile-time constants.

use crate::resolver::ResolvedDeps;
use anyhow::Result;
use std::process::Command;

/// Result of auditing system dependencies
#[derive(Debug)]
pub struct SysreqReport {
    /// System packages that are already installed
    pub found: Vec<InstalledDep>,

    /// System packages that are missing
    pub missing: Vec<MissingDep>,
}

#[derive(Debug)]
pub struct InstalledDep {
    pub name: String,
    pub version: String,
}

#[derive(Debug)]
pub struct MissingDep {
    pub name: String,
    /// Which R packages need this system library
    pub needed_by: Vec<String>,
}

/// Map of R package names → required system (apt) packages
///
/// RUST CONCEPT: &[(&str, &[&str])]
/// This reads as: "a slice of tuples, where each tuple contains
/// a string slice and a slice of string slices."
///
/// Breaking it down:
///   &str          = a string reference (like "libhdf5-dev")
///   &[&str]       = a slice of string references (a list of strings)
///   (&str, &[&str]) = a tuple: (r_package, [system_libs])
///   &[...]        = a slice (reference to an array)
///
/// This is all stack-allocated — no heap allocation at all.
/// In Python, this would be a dict, but Rust's const arrays are faster.
const SYSREQ_MAP: &[(&str, &[&str])] = &[
    // Bioconductor packages
    ("rhdf5", &["libhdf5-dev"]),
    ("HDF5Array", &["libhdf5-dev"]),
    ("Rhtslib", &["libbz2-dev", "liblzma-dev", "libcurl4-openssl-dev"]),
    ("Rsamtools", &["libbz2-dev", "liblzma-dev"]),

    // Common CRAN packages with system deps
    ("curl", &["libcurl4-openssl-dev"]),
    ("openssl", &["libssl-dev"]),
    ("xml2", &["libxml2-dev"]),
    ("httr", &["libcurl4-openssl-dev", "libssl-dev"]),
    ("git2r", &["libgit2-dev"]),
    ("sodium", &["libsodium-dev"]),
    ("RcppGSL", &["libgsl-dev"]),
    ("gsl", &["libgsl-dev"]),
    ("nloptr", &["cmake"]),
    ("sf", &["libgdal-dev", "libgeos-dev", "libproj-dev"]),
    ("terra", &["libgdal-dev", "libgeos-dev", "libproj-dev"]),
    ("magick", &["libmagick++-dev"]),
    ("av", &["libavfilter-dev"]),
    ("ragg", &["libfreetype6-dev", "libpng-dev", "libtiff5-dev"]),
    ("textshaping", &["libharfbuzz-dev", "libfribidi-dev"]),
    ("systemfonts", &["libfontconfig1-dev"]),
    ("cairo", &["libcairo2-dev"]),
    ("rjags", &["jags"]),
    ("RMySQL", &["libmariadb-dev"]),
    ("RPostgres", &["libpq-dev"]),
    ("odbc", &["unixodbc-dev"]),

    // Compilation essentials (needed by any package with C/C++/Fortran)
    ("Rcpp", &["build-essential"]),
    ("RcppArmadillo", &["build-essential"]),
    ("RcppEigen", &["build-essential"]),
];

/// Linux apt name → macOS Homebrew name
const BREW_MAP: &[(&str, &str)] = &[
    ("libcurl4-openssl-dev", "curl"),
    ("libssl-dev", "openssl"),
    ("libxml2-dev", "libxml2"),
    ("libhdf5-dev", "hdf5"),
    ("libgsl-dev", "gsl"),
    ("libgit2-dev", "libgit2"),
    ("libsodium-dev", "libsodium"),
    ("libfontconfig1-dev", "fontconfig"),
    ("libharfbuzz-dev", "harfbuzz"),
    ("libfribidi-dev", "fribidi"),
    ("libfreetype6-dev", "freetype"),
    ("libpng-dev", "libpng"),
    ("libtiff5-dev", "libtiff"),
    ("libcairo2-dev", "cairo"),
    ("libmagick++-dev", "imagemagick"),
    ("libpq-dev", "postgresql"),
    ("libmariadb-dev", "mariadb"),
    ("libgdal-dev", "gdal"),
    ("libgeos-dev", "geos"),
    ("libproj-dev", "proj"),
    ("build-essential", "gcc"),
    ("gfortran", "gcc"),  // gfortran comes with gcc on brew
    ("cmake", "cmake"),
];

/// Check which system dependencies are installed and which are missing.
///
/// RUST CONCEPT: impl Trait Patterns
/// Instead of returning a generic type, we return a concrete struct.
/// This makes the API clear about what you get back.
pub fn audit(resolved: &ResolvedDeps) -> Result<SysreqReport> {
    let mut required_syslibs: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    // Walk through resolved packages and check which need system libs
    for pkg in &resolved.packages {
        // Look up this R package in our sysreq map
        for (r_pkg, sys_libs) in SYSREQ_MAP {
            if pkg.name == *r_pkg {
                for lib in *sys_libs {
                    // RUST CONCEPT: Entry API
                    // HashMap's entry() API is elegant for "get or insert" patterns.
                    // `entry(key).or_default()` gets the value if it exists,
                    // or inserts a default (empty Vec) if it doesn't.
                    required_syslibs
                        .entry(lib.to_string())
                        .or_default()
                        .push(pkg.name.clone());
                }
            }
        }

        // Also check if any package needs compilation (needs build-essential)
        if pkg.needs_compilation {
            required_syslibs
                .entry("build-essential".to_string())
                .or_default()
                .push(pkg.name.clone());

            // Check if Fortran is needed (common for statistical packages)
            // Heuristic: packages with "linalg", "blas", "lapack" in deps
            let needs_fortran = pkg
                .dependencies
                .iter()
                .any(|d| ["Matrix", "survival", "minqa"].contains(&d.as_str()));

            if needs_fortran {
                required_syslibs
                    .entry("gfortran".to_string())
                    .or_default()
                    .push(pkg.name.clone());
            }
        }
    }

    // Now check which are installed
    let mut found = Vec::new();
    let mut missing = Vec::new();

    for (lib_name, needed_by) in &required_syslibs {
        if let Some(version) = check_installed(lib_name) {
            found.push(InstalledDep {
                name: lib_name.clone(),
                version,
            });
        } else {
            missing.push(MissingDep {
                name: lib_name.clone(),
                needed_by: needed_by.clone(),
            });
        }
    }

    Ok(SysreqReport { found, missing })
}
fn is_macos() -> bool {
    std::env::consts::OS == "macos"
}

/// Check if a system package is installed (works on both macOS and Linux)
fn check_installed(package_name: &str) -> Option<String> {
    if is_macos() {
        check_brew_installed(package_name)
    } else {
        check_dpkg_installed(package_name)
    }
}

/// Check if a Homebrew package is installed on macOS
fn check_brew_installed(linux_name: &str) -> Option<String> {
     // Special cases: these come from Xcode, not Homebrew
    if linux_name == "build-essential" {
        let output = Command::new("cc").arg("--version").output().ok()?;
        if output.status.success() {
            return Some("xcode".to_string());
        }
    }

    if linux_name == "gfortran" {
        let output = Command::new("gfortran").arg("--version").output().ok()?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let version = stdout.lines().next().unwrap_or("installed");
            return Some(version.to_string());
        }
    }
    if linux_name == "libcurl4-openssl-dev" {
        let output = Command::new("curl").arg("--version").output().ok()?;
        if output.status.success() {
            return Some("system".to_string());
        }
    }
    // Translate Linux name to Brew name
    let brew_name = BREW_MAP
        .iter()
        .find(|(linux, _)| *linux == linux_name)
        .map(|(_, brew)| *brew)
        .unwrap_or(linux_name);

    let output = Command::new("brew")
        .args(["list", "--versions", brew_name])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Output is like "openssl@3 3.2.1" — extract version
    let version = trimmed.split_whitespace().last().unwrap_or("unknown");
    Some(version.to_string())
}
/// Check if a dpkg package is installed and return its version
///
/// Runs: dpkg -s <package> and parses the output
/// Check if a dpkg package is installed on Linux
fn check_dpkg_installed(package_name: &str) -> Option<String> {
    let output = Command::new("dpkg")
        .args(["-s", package_name])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    let is_installed = stdout
        .lines()
        .any(|line| line.starts_with("Status:") && line.contains("installed"));

    if !is_installed {
        return None;
    }

    let version = stdout
        .lines()
        .find(|line| line.starts_with("Version:"))
        .map(|line| line.trim_start_matches("Version:").trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    Some(version)
}
/// Translate a Linux package name to its Homebrew equivalent
pub fn get_brew_name(linux_name: &str) -> String {
    BREW_MAP
        .iter()
        .find(|(linux, _)| *linux == linux_name)
        .map(|(_, brew)| brew.to_string())
        .unwrap_or_else(|| linux_name.to_string())
}

/// Install missing system packages using apt
pub fn install_missing(report: &SysreqReport) -> Result<()> {
    if report.missing.is_empty() {
        return Ok(());
    }

    if is_macos() {
        // Translate to brew names
        let brew_names: Vec<String> = report
            .missing
            .iter()
            .map(|d| {
                BREW_MAP
                    .iter()
                    .find(|(linux, _)| *linux == d.name.as_str())
                    .map(|(_, brew)| brew.to_string())
                    .unwrap_or_else(|| d.name.clone())
            })
            .collect();

        println!("Running: brew install {}", brew_names.join(" "));

        let status = Command::new("brew")
            .arg("install")
            .args(&brew_names)
            .status()?;

        if !status.success() {
            anyhow::bail!("Failed to install. Run manually:\n  brew install {}", brew_names.join(" "));
        }
    } else {
        let package_names: Vec<&str> = report.missing.iter().map(|d| d.name.as_str()).collect();

        println!("Running: sudo apt install -y {}", package_names.join(" "));

        let status = Command::new("sudo")
            .arg("apt")
            .arg("install")
            .arg("-y")
            .args(&package_names)
            .status()?;

        if !status.success() {
            anyhow::bail!("Failed to install. Run manually:\n  sudo apt install {}", package_names.join(" "));
        }
    }

    Ok(())
}
