// src/resolver.rs — Shared resolution types
//
// NOTE: The actual dependency resolution lives in `sat_resolver.rs`
// (`resolve_with_constraints`), which walks the full transitive tree with
// version-constraint and CRAN-Archive handling. This module now only holds
// the shared result types those resolvers produce and the installer consumes.

/// GitHub-specific provenance for a resolved package.
/// None for CRAN/Bioconductor packages.
#[derive(Debug, Clone)]
pub struct GitHubSource {
    pub owner: String,
    pub repo: String,
    pub commit_sha: String,
    pub subdir: Option<String>,
    pub tarball_sha256: String,
}

/// The result of dependency resolution.
#[derive(Debug)]
pub struct ResolvedDeps {
    /// Packages in installation order (dependencies first)
    pub packages: Vec<ResolvedPackage>,

    /// How long resolution took (in seconds)
    pub duration_secs: f64,
}

/// A single resolved package with all info needed for installation.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    pub source: String, // "cran" or "bioc" or "github"
    pub needs_compilation: bool,

    /// Direct dependencies of this package
    pub dependencies: Vec<String>,

    /// SHA256 hash of the source tarball (for lockfile)
    pub sha256: Option<String>,

    /// Populated for GitHub packages; None for registry packages.
    pub github_source: Option<GitHubSource>,

    /// Raw `SystemRequirements:` text from DESCRIPTION. Used by sysreq audit
    /// as a fallback when RSPM returns empty (covers RSPM rule gaps).
    pub system_requirements: Option<String>,
}
