// src/registry/parser.rs — Parses CRAN/Bioconductor PACKAGES format
//
// The PACKAGES file is a flat-text database with entries separated by
// blank lines. Each entry looks like:
//
//   Package: DESeq2
//   Version: 1.42.0
//   Depends: R (>= 4.4.0), methods, stats, BiocGenerics (>= 0.44.0)
//   Imports: GenomicRanges, SummarizedExperiment, Rcpp (>= 1.0.0)
//   LinkingTo: Rcpp, RcppArmadillo
//   NeedsCompilation: yes
//   SystemRequirements: C++17
//
// Our job: parse this into Vec<PackageMetadata>.
//
// RUST CONCEPT: Iterators
// Rust's iterators are one of its most powerful features. They're lazy
// (like Python generators) and the compiler optimizes them into efficient
// loops. The chain: .lines().filter().map().collect() compiles down to
// a single efficient loop — no intermediate allocations.

use super::{Dependency, PackageMetadata, PackageSource};
use anyhow::Result;

/// Parse the full PACKAGES file text into a list of package metadata
///
/// RUST CONCEPT: &str vs String
///   - &str is a "string slice" — a reference to string data. Cheap, no allocation.
///     Like a view/pointer. Used for function parameters (reading).
///   - String is an "owned string" — heap-allocated, growable, you own it.
///     Like Python's str. Used for storing data in structs.
///
/// Rule of thumb: take &str as input, return String when you need to store it.
pub fn parse_packages(content: &str, source: PackageSource) -> Result<Vec<PackageMetadata>> {
    let mut packages = Vec::new();

    // Split by blank lines to get individual package entries
    // RUST CONCEPT: `split("\n\n")` returns an iterator over &str slices.
    // No memory allocation — it just gives you views into the original string.
    for entry in content.split("\n\n") {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        // RUST CONCEPT: `if let Some(pkg) = ...` is an "if-let" pattern.
        // It tries to match the result. If it's Some(value), we use it.
        // If it's None, we skip this entry.
        if let Some(pkg) = parse_single_entry(entry, &source) {
            packages.push(pkg);
        }
    }

    Ok(packages)
}

/// Parse a single package entry from the PACKAGES file
///
/// Returns None if the entry is malformed (we skip it silently).
fn parse_single_entry(entry: &str, source: &PackageSource) -> Option<PackageMetadata> {
    // RUST CONCEPT: HashMap for temporary key-value storage
    // We parse each "Key: Value" line into a map, then extract what we need.
    let mut fields = std::collections::HashMap::new();

    let mut current_key = String::new();
    let mut current_value = String::new();

    for line in entry.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation line — append to current value
            // (PACKAGES format uses indentation for multi-line values)
            current_value.push(' ');
            current_value.push_str(line.trim());
        } else if let Some((key, value)) = line.split_once(": ") {
            // New field — save the previous one
            if !current_key.is_empty() {
                fields.insert(current_key.clone(), current_value.clone());
            }
            current_key = key.to_string();
            current_value = value.to_string();
        }
    }

    // Don't forget the last field
    if !current_key.is_empty() {
        fields.insert(current_key, current_value);
    }

    // Extract the package name — if missing, skip this entry
    // RUST CONCEPT: The `?` operator works inside functions that return Option too.
    // But here we're being explicit with `match` for clarity.
    let name = fields.get("Package")?.clone();
    let version = fields.get("Version").cloned().unwrap_or_default();

    // Parse dependency fields
    let depends = fields
        .get("Depends")
        .map(|d| parse_dependency_list(d))
        .unwrap_or_default();

    let imports = fields
        .get("Imports")
        .map(|d| parse_dependency_list(d))
        .unwrap_or_default();

    let linking_to = fields
        .get("LinkingTo")
        .map(|lt| {
            lt.split(',')
                .map(|s| {
                    let trimmed = s.trim();
                    // Strip version constraint: "BH (>= 1.64.0-1)" → "BH"
                    match trimmed.find('(') {
                        Some(pos) => trimmed[..pos].trim().to_string(),
                        None => trimmed.to_string(),
                    }
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let needs_compilation = fields
        .get("NeedsCompilation")
        .map(|v| v.trim() == "yes")
        .unwrap_or(false);

    let system_requirements = fields.get("SystemRequirements").cloned();

    Some(PackageMetadata {
        name,
        version,
        source: source.clone(),
        depends,
        imports,
        linking_to,
        needs_compilation,
        system_requirements,
    })
}

/// Parse a comma-separated dependency list like:
///   "R (>= 4.4.0), methods, BiocGenerics (>= 0.44.0)"
/// into a Vec<Dependency>, filtering out "R" and base packages.
///
/// RUST CONCEPT: Iterator chains
/// This is idiomatic Rust — transform data through a chain of operations.
/// Each step is lazy (doesn't allocate) until .collect() materializes the result.
///
///   input.split(',')           // iterator over &str pieces
///     .map(|s| s.trim())       // trim whitespace from each
///     .filter(|s| !s.is_empty()) // remove empty entries
///     .map(|s| parse_one(s))   // transform each into Dependency
///     .collect()               // gather into Vec<Dependency>
fn parse_dependency_list(input: &str) -> Vec<Dependency> {
    // Base R packages that are always available — we skip these
    const BASE_PACKAGES: &[&str] = &[
         "base", "compiler", "datasets", "grDevices", "graphics",
        "grid", "methods", "parallel", "splines", "stats", "stats4",
        "tcltk", "tools", "utils",
    ];

    input
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            // RUST CONCEPT: filter_map combines filter + map.
            // Return Some(value) to keep it, None to skip it.

            // Parse "PackageName (>= 1.2.0)" or just "PackageName"
            let (name, version_req) = if let Some(paren_pos) = s.find('(') {
                let name = s[..paren_pos].trim();
                let version_part = s[paren_pos..].trim_matches(|c| c == '(' || c == ')');
                (name, Some(version_part.trim().to_string()))
            } else {
                (s, None)
            };

            // Skip base packages
            if BASE_PACKAGES.contains(&name) {
                return None;
            }

            Some(Dependency {
                name: name.to_string(),
                version_req,
            })
        })
        .collect()
}

// --- Tests ---
//
// RUST CONCEPT: #[cfg(test)] module
// Code inside #[cfg(test)] only compiles when running tests.
// Run tests with: `cargo test`
// This is like Python's unittest but built into the language.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dependency_list() {
        let input = "R (>= 4.4.0), methods, BiocGenerics (>= 0.44.0), ggplot2";
        let deps = parse_dependency_list(input);

        // "R" and "methods" are base packages — should be filtered out
        assert_eq!(deps[0].name, "R");
        assert_eq!(deps[0].version_req, Some(">= 4.4.0".to_string()));
        assert_eq!(deps[1].name, "BiocGenerics");
        assert_eq!(deps[1].version_req, Some(">= 0.44.0".to_string()));
        assert_eq!(deps[2].name, "ggplot2");
        assert_eq!(deps[2].version_req, None);
    }

    #[test]
    fn test_parse_single_entry() {
        let entry = "\
Package: DESeq2
Version: 1.42.0
Depends: R (>= 4.4.0), methods, BiocGenerics
Imports: GenomicRanges, Rcpp
LinkingTo: Rcpp, RcppArmadillo
NeedsCompilation: yes
SystemRequirements: C++17";

        let pkg = parse_single_entry(entry, &PackageSource::Bioconductor).unwrap();

        assert_eq!(pkg.name, "DESeq2");
        assert_eq!(pkg.version, "1.42.0");
        assert!(pkg.needs_compilation);
        assert_eq!(pkg.imports.len(), 2);
        assert_eq!(pkg.linking_to, vec!["Rcpp", "RcppArmadillo"]);
    }
}
