// src/source.rs — Where the user wants a package to come from.
//
// RUST CONCEPT: enums for "one of many"
// In Python you'd use isinstance() or string prefixes. Rust prefers an
// enum where each variant carries different data. Zero runtime cost,
// and the compiler forces you to handle every variant in a `match`.
//
// NAMING NOTE: yes, there is also a `PackageSource` enum in registry/mod.rs.
// That one is internal ("where did metadata come from"). This one is
// user-facing ("what did the user type"). They live in different modules
// and never need to coexist in the same file. Use module paths to refer
// to either: `source::PackageSource` vs `registry::PackageSource`.

use anyhow::{bail, Result};

/// What a single CLI argument or `Remotes:` entry says about a package.
///
/// Examples:
///   "DESeq2"             → Registry("DESeq2")
///   "user/repo"          → GitHub(...)   (bare form, like pak/remotes)
///   "gh:user/repo"       → GitHub(GitHubSpec { ... })
///   "github::user/repo"  → GitHub(...)   (the Remotes: field form)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    /// Resolve via CRAN/Bioconductor. The string is the package name.
    Registry(String),

    /// A specific GitHub repository.
    GitHub(GitHubSpec),
}

/// Identifies a GitHub-hosted R package.
///
/// A `r#ref` of None means "the default branch's HEAD." We'll resolve
/// that to a commit SHA on Day 2 when we actually hit the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubSpec {
    pub owner: String,
    pub repo: String,
    /// Branch, tag, or commit SHA. None = default branch.
    /// RUST CONCEPT: `r#ref` — `ref` is a keyword. The `r#` prefix is
    /// the "raw identifier" escape that lets you use a keyword as a name.
    pub r#ref: Option<String>,
    /// For monorepos: path inside the repo where the package lives.
    pub subdir: Option<String>,
}

impl PackageSource {
    /// Parse a single CLI argument or `Remotes:` entry.
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(rest) = s.strip_prefix("gh:") {
            return Ok(PackageSource::GitHub(parse_github_spec(rest)?));
        }
        if let Some(rest) = s.strip_prefix("github::") {
            return Ok(PackageSource::GitHub(parse_github_spec(rest)?));
        }
        // Bare "owner/repo" form, as accepted by pak and remotes
        // (e.g. `rin install satijalab/seurat-wrappers`). A CRAN/Bioconductor
        // package name can only contain letters, numbers, and dots — never a
        // slash — so any unprefixed spec containing '/' is unambiguously a
        // GitHub repository, not a registry name.
        if s.contains('/') {
            return Ok(PackageSource::GitHub(parse_github_spec(s)?));
        }
        Ok(PackageSource::Registry(s.to_string()))
    }
}

fn parse_github_spec(s: &str) -> Result<GitHubSpec> {
    // Split off the optional "@ref" suffix first.
    let (path_part, ref_part) = match s.split_once('@') {
        Some((p, r)) => (p, Some(r.trim().to_string())),
        None => (s, None),
    };

    // The path part is "user/repo" or "user/repo/sub/dir".
    // filter empties so "user//repo" is treated as "user/repo".
    let segments: Vec<&str> = path_part.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        bail!("Invalid GitHub spec: '{}' (expected user/repo)", s);
    }

    let owner = segments[0].to_string();
    let repo = segments[1].to_string();
    let subdir = if segments.len() > 2 {
        Some(segments[2..].join("/"))
    } else {
        None
    };

    Ok(GitHubSpec {
        owner,
        repo,
        r#ref: ref_part,
        subdir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bare_registry() {
        assert_eq!(
            PackageSource::parse("DESeq2").unwrap(),
            PackageSource::Registry("DESeq2".to_string())
        );
    }

    #[test]
    fn test_gh_simple() {
        let p = PackageSource::parse("gh:user/repo").unwrap();
        if let PackageSource::GitHub(spec) = p {
            assert_eq!(spec.owner, "user");
            assert_eq!(spec.repo, "repo");
            assert_eq!(spec.r#ref, None);
            assert_eq!(spec.subdir, None);
        } else {
            panic!("expected GitHub variant");
        }
    }

    #[test]
    fn test_gh_with_ref() {
        let p = PackageSource::parse("gh:user/repo@v1.0.0").unwrap();
        if let PackageSource::GitHub(spec) = p {
            assert_eq!(spec.r#ref, Some("v1.0.0".to_string()));
        } else {
            panic!("expected GitHub variant");
        }
    }

    #[test]
    fn test_gh_with_branch() {
        let p = PackageSource::parse("gh:user/repo@main").unwrap();
        if let PackageSource::GitHub(spec) = p {
            assert_eq!(spec.r#ref, Some("main".to_string()));
        } else {
            panic!("expected GitHub variant");
        }
    }

    #[test]
    fn test_gh_with_subdir() {
        let p = PackageSource::parse("gh:user/repo/r-pkg").unwrap();
        if let PackageSource::GitHub(spec) = p {
            assert_eq!(spec.subdir, Some("r-pkg".to_string()));
            assert_eq!(spec.r#ref, None);
        } else {
            panic!("expected GitHub variant");
        }
    }

    #[test]
    fn test_gh_with_multilevel_subdir() {
        let p = PackageSource::parse("gh:user/repo/projects/r").unwrap();
        if let PackageSource::GitHub(spec) = p {
            assert_eq!(spec.subdir, Some("projects/r".to_string()));
        } else {
            panic!("expected GitHub variant");
        }
    }

    #[test]
    fn test_gh_with_subdir_and_ref() {
        let p = PackageSource::parse("gh:user/repo/r-pkg@v1.0.0").unwrap();
        if let PackageSource::GitHub(spec) = p {
            assert_eq!(spec.subdir, Some("r-pkg".to_string()));
            assert_eq!(spec.r#ref, Some("v1.0.0".to_string()));
        } else {
            panic!("expected GitHub variant");
        }
    }

    #[test]
    fn test_remotes_prefix() {
        let p = PackageSource::parse("github::satijalab/seurat-data").unwrap();
        if let PackageSource::GitHub(spec) = p {
            assert_eq!(spec.owner, "satijalab");
            assert_eq!(spec.repo, "seurat-data");
        } else {
            panic!("expected GitHub variant");
        }
    }

    #[test]
    fn test_bare_owner_repo() {
        let p = PackageSource::parse("satijalab/seurat-wrappers").unwrap();
        if let PackageSource::GitHub(spec) = p {
            assert_eq!(spec.owner, "satijalab");
            assert_eq!(spec.repo, "seurat-wrappers");
            assert_eq!(spec.r#ref, None);
            assert_eq!(spec.subdir, None);
        } else {
            panic!("expected GitHub variant for bare owner/repo");
        }
    }

    #[test]
    fn test_bare_owner_repo_with_ref() {
        let p = PackageSource::parse("immunogenomics/presto@master").unwrap();
        if let PackageSource::GitHub(spec) = p {
            assert_eq!(spec.owner, "immunogenomics");
            assert_eq!(spec.repo, "presto");
            assert_eq!(spec.r#ref, Some("master".to_string()));
        } else {
            panic!("expected GitHub variant");
        }
    }

    #[test]
    fn test_invalid_no_repo() {
        assert!(PackageSource::parse("gh:user").is_err());
    }

    #[test]
    fn test_invalid_empty_after_prefix() {
        assert!(PackageSource::parse("gh:").is_err());
    }
}