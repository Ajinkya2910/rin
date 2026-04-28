// src/registry/parser.rs — Parses CRAN/Bioconductor PACKAGES format
//                          and standalone DESCRIPTION files.
//
// The PACKAGES file is a flat-text database with entries separated by
// blank lines. Each entry uses the same format as a standalone
// DESCRIPTION file inside a package tarball.
//
// Shared shape:
//   Package: DESeq2
//   Version: 1.42.0
//   Depends: R (>= 4.4.0), methods, BiocGenerics (>= 0.44.0)
//   Imports: GenomicRanges, Rcpp (>= 1.0.0)
//   LinkingTo: Rcpp, RcppArmadillo
//   NeedsCompilation: yes
//   SystemRequirements: C++17
//   Remotes: github::satijalab/seurat-data       <- DESCRIPTION-only
//
// We have two entry points:
//   parse_packages(text)       → many entries from a PACKAGES index
//   parse_description(text)    → one entry from a tarball's DESCRIPTION
// Both share parse_fields() for the line-by-line key:value extraction.

use super::{Dependency, PackageMetadata, PackageSource};
use anyhow::{anyhow, Result};
use std::collections::HashMap;

// --- Shared field extraction ------------------------------------------------

/// Parse the line-by-line "Key: Value" structure of one entry.
/// Handles continuation lines (lines starting with whitespace).
///
/// This is the part shared between PACKAGES entries and DESCRIPTION files —
/// they have identical line-level syntax, only the field semantics differ.
fn parse_fields(entry: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let mut current_key = String::new();
    let mut current_value = String::new();

    for line in entry.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation of the current value.
            // Don't prepend a space if the value is empty (multi-line field
            // where the key line was just "Fieldname:" with no inline value).
            if !current_value.is_empty() {
                current_value.push(' ');
            }
            current_value.push_str(line.trim());
        } else if let Some((key, value)) = line.split_once(':') {
            // New field — flush the previous one.
            // Match on bare ':' so we accept both "Key: value" and "Key:" (value follows on next lines).
            if !current_key.is_empty() {
                fields.insert(current_key.clone(), current_value.clone());
            }
            current_key = key.to_string();
            current_value = value.trim_start().to_string();
        }
    }

    // Flush the last field.
    if !current_key.is_empty() {
        fields.insert(current_key, current_value);
    }

    fields
}

// --- PACKAGES index parsing (for CRAN/Bioc) --------------------------------

/// Parse the full PACKAGES file text into a list of package metadata.
pub fn parse_packages(content: &str, source: PackageSource) -> Result<Vec<PackageMetadata>> {
    let mut packages = Vec::new();

    for entry in content.split("\n\n") {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some(pkg) = parse_single_entry(entry, &source) {
            packages.push(pkg);
        }
    }

    Ok(packages)
}

/// Parse a single PACKAGES entry into a PackageMetadata.
/// Returns None if the entry is malformed (we skip it silently).
fn parse_single_entry(entry: &str, source: &PackageSource) -> Option<PackageMetadata> {
    let fields = parse_fields(entry);

    let name = fields.get("Package")?.clone();
    let version = fields.get("Version").cloned().unwrap_or_default();

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
        .map(|lt| parse_linking_to(lt))
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

// --- DESCRIPTION parsing (for GitHub tarballs) -----------------------------

/// Result of parsing a single DESCRIPTION file.
///
/// Shape mirrors PackageMetadata but without a `source` field — that's
/// added by the caller, since DESCRIPTION files don't know where they
/// came from. Adds `remotes`, which only appears in DESCRIPTION files.
#[derive(Debug, Clone)]
pub struct ParsedDescription {
    pub name: String,
    pub version: Option<String>,
    pub depends: Vec<Dependency>,
    pub imports: Vec<Dependency>,
    pub linking_to: Vec<String>,
    pub remotes: Vec<String>,
    pub needs_compilation: bool,
    pub system_requirements: Option<String>,
}

/// Parse a standalone DESCRIPTION file (the one inside a package tarball).
///
/// Unlike PACKAGES entries, a missing Package: field is a hard error —
/// the file is malformed and we want to surface that.
pub fn parse_description(text: &str) -> Result<ParsedDescription> {
    let fields = parse_fields(text);

    let name = fields
        .get("Package")
        .ok_or_else(|| anyhow!("DESCRIPTION has no Package: field"))?
        .clone();

    // Version is Option here — the caller decides whether to error.
    // (For GitHub packages we error; for some future use we might not.)
    let version = fields.get("Version").cloned();

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
        .map(|lt| parse_linking_to(lt))
        .unwrap_or_default();

    // Remotes: comma-separated list of source specs. Format:
    //   github::user/repo
    //   github::user/repo@v1.0.0
    //   github::user/repo/subdir@ref
    //   cran::pkgname            (we ignore non-github prefixes for Phase 1)
    let remotes = fields
        .get("Remotes")
        .map(|text| {
            text.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let needs_compilation = fields
        .get("NeedsCompilation")
        .map(|v| v.trim() == "yes")
        .unwrap_or(false);

    let system_requirements = fields.get("SystemRequirements").cloned();

    Ok(ParsedDescription {
        name,
        version,
        depends,
        imports,
        linking_to,
        remotes,
        needs_compilation,
        system_requirements,
    })
}

// --- Shared field-value parsers --------------------------------------------

/// Parse a comma-separated dependency list:
///   "R (>= 4.4.0), methods, BiocGenerics (>= 0.44.0)"
/// into a Vec<Dependency>, dropping base R packages.
fn parse_dependency_list(input: &str) -> Vec<Dependency> {
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
            let (name, version_req) = if let Some(paren_pos) = s.find('(') {
                let name = s[..paren_pos].trim();
                let version_part = s[paren_pos..].trim_matches(|c| c == '(' || c == ')');
                (name, Some(version_part.trim().to_string()))
            } else {
                (s, None)
            };

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

/// Parse a LinkingTo field — same structure as Depends/Imports but we
/// only keep the names. Version constraints are stripped.
fn parse_linking_to(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(|s| {
            let trimmed = s.trim();
            match trimmed.find('(') {
                Some(pos) => trimmed[..pos].trim().to_string(),
                None => trimmed.to_string(),
            }
        })
        .filter(|s| !s.is_empty())
        .collect()
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dependency_list() {
        let input = "R (>= 4.4.0), methods, BiocGenerics (>= 0.44.0), ggplot2";
        let deps = parse_dependency_list(input);

        // R kept (not in BASE_PACKAGES). methods filtered out.
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

    #[test]
    fn test_parse_description_basic() {
        let text = "\
Package: SeuratData
Version: 0.2.2
Depends: R (>= 3.5.0), Seurat (>= 3.0.0)
Imports: methods, utils
LinkingTo: Rcpp
NeedsCompilation: no";

        let parsed = parse_description(text).unwrap();
        assert_eq!(parsed.name, "SeuratData");
        assert_eq!(parsed.version, Some("0.2.2".to_string()));
        assert_eq!(parsed.depends.len(), 2);  // R, Seurat
        assert_eq!(parsed.imports.len(), 0);  // methods/utils filtered as base
        assert_eq!(parsed.linking_to, vec!["Rcpp"]);
        assert!(parsed.remotes.is_empty());
        assert!(!parsed.needs_compilation);
    }

    #[test]
    fn test_parse_description_with_remotes() {
        let text = "\
Package: MyPkg
Version: 0.1.0
Imports: SeuratData, dplyr
Remotes: github::satijalab/seurat-data, github::user/other@v1.0.0";

        let parsed = parse_description(text).unwrap();
        assert_eq!(parsed.remotes.len(), 2);
        assert_eq!(parsed.remotes[0], "github::satijalab/seurat-data");
        assert_eq!(parsed.remotes[1], "github::user/other@v1.0.0");
    }

    #[test]
    fn test_parse_description_missing_version() {
        let text = "Package: MyPkg\nImports: dplyr";
        let parsed = parse_description(text).unwrap();
        assert_eq!(parsed.version, None);  // caller decides whether to error
    }

    #[test]
    fn test_parse_description_missing_package_errors() {
        let text = "Version: 1.0.0\nImports: dplyr";
        assert!(parse_description(text).is_err());
    }
    #[test]
    fn test_parse_description_multiline_imports() {
        // seurat-data style: field name on one line, values indented below.
        let text = "\
Package: SeuratData
Version: 0.2.2
Depends:
    R (>= 3.5.0)
Imports:
    cli,
    crayon,
    Matrix,
    methods,
    Seurat (>= 5.0.0)
NeedsCompilation: no";

        let parsed = parse_description(text).unwrap();
        assert_eq!(parsed.name, "SeuratData");
        assert_eq!(parsed.depends.len(), 1, "expected R in depends");
        assert_eq!(parsed.depends[0].name, "R");

        let import_names: Vec<&str> = parsed.imports.iter().map(|d| d.name.as_str()).collect();
        // methods filtered as base; cli, crayon, Matrix, Seurat remain
        assert!(import_names.contains(&"cli"), "got: {:?}", import_names);
        assert!(import_names.contains(&"Seurat"), "got: {:?}", import_names);
        assert_eq!(parsed.imports.len(), 4);
    }
}