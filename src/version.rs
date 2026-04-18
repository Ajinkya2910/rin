// src/version.rs — R package version parsing and comparison
//
// R versions look like "1.42.0", "0.12.16", "15.2.4-1"
// Version constraints look like ">= 1.2.0", "< 2.0.0"
//
// Unlike semver, R versions can have 2, 3, or 4 parts and use
// dashes for Debian-style revisions. We handle all of these.

use std::cmp::Ordering;
use std::fmt;

/// A parsed R package version
#[derive(Debug, Clone, Hash)]
pub struct RVersion {
    /// Version parts: "1.42.0" → [1, 42, 0], "15.2.4-1" → [15, 2, 4, 1]
    pub parts: Vec<u32>,
    /// Original string for display
    pub original: String,
}

impl RVersion {
    /// Parse a version string like "1.42.0" or "15.2.4-1"
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None;
        }

        // Split on dots and dashes: "15.2.4-1" → ["15", "2", "4", "1"]
        let parts: Vec<u32> = trimmed
            .split(|c| c == '.' || c == '-')
            .filter_map(|p| p.parse().ok())
            .collect();

        if parts.is_empty() {
            return None;
        }

        Some(RVersion {
            parts,
            original: trimmed.to_string(),
        })
    }
}

// RUST CONCEPT: Implementing Ord lets you use <, >, ==, etc.
// This is how the SAT solver will compare versions.
impl Ord for RVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        // Compare part by part: 1.42.0 vs 1.41.0
        // If one version has more parts, the missing parts are treated as 0
        let max_len = self.parts.len().max(other.parts.len());
        for i in 0..max_len {
            let a = self.parts.get(i).copied().unwrap_or(0);
            let b = other.parts.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => continue,
                other => return other,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for RVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for RVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for RVersion {}

impl fmt::Display for RVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.original)
    }
}

/// A version constraint like ">= 1.2.0" or "< 2.0.0"
#[derive(Debug, Clone)]
pub struct VersionConstraint {
    pub op: ConstraintOp,
    pub version: RVersion,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConstraintOp {
    Gte,  // >=
    Gt,   // >
    Lte,  // <=
    Lt,   // 
    Eq,   // ==
}
impl fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op_str = match self.op {
            ConstraintOp::Gte => ">=",
            ConstraintOp::Gt => ">",
            ConstraintOp::Lte => "<=",
            ConstraintOp::Lt => "<",
            ConstraintOp::Eq => "==",
        };
        write!(f, "{} {}", op_str, self.version)
    }
}

impl VersionConstraint {
    /// Parse a constraint string like ">= 1.2.0"
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim();

        // Try each operator prefix
        let (op, version_str) = if let Some(v) = trimmed.strip_prefix(">=") {
            (ConstraintOp::Gte, v)
        } else if let Some(v) = trimmed.strip_prefix(">") {
            (ConstraintOp::Gt, v)
        } else if let Some(v) = trimmed.strip_prefix("<=") {
            (ConstraintOp::Lte, v)
        } else if let Some(v) = trimmed.strip_prefix("<") {
            (ConstraintOp::Lt, v)
        } else if let Some(v) = trimmed.strip_prefix("==") {
            (ConstraintOp::Eq, v)
        } else {
            // No operator means exact version
            (ConstraintOp::Eq, trimmed)
        };

        let version = RVersion::parse(version_str.trim())?;
        Some(VersionConstraint { op, version })
    }

    /// Check if a version satisfies this constraint
    pub fn satisfies(&self, candidate: &RVersion) -> bool {
        match self.op {
            ConstraintOp::Gte => candidate >= &self.version,
            ConstraintOp::Gt => candidate > &self.version,
            ConstraintOp::Lte => candidate <= &self.version,
            ConstraintOp::Lt => candidate < &self.version,
            ConstraintOp::Eq => candidate == &self.version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_parse() {
        let v = RVersion::parse("1.42.0").unwrap();
        assert_eq!(v.parts, vec![1, 42, 0]);

        let v = RVersion::parse("15.2.4-1").unwrap();
        assert_eq!(v.parts, vec![15, 2, 4, 1]);

        let v = RVersion::parse("0.12").unwrap();
        assert_eq!(v.parts, vec![0, 12]);
    }

    #[test]
    fn test_version_compare() {
        let v1 = RVersion::parse("1.42.0").unwrap();
        let v2 = RVersion::parse("1.41.0").unwrap();
        assert!(v1 > v2);

        let v1 = RVersion::parse("1.2.0").unwrap();
        let v2 = RVersion::parse("1.2").unwrap();
        assert!(v1 == v2);  // trailing .0 is equal

        let v1 = RVersion::parse("15.2.4-1").unwrap();
        let v2 = RVersion::parse("15.2.4").unwrap();
        assert!(v1 > v2);  // -1 revision is higher
    }

    #[test]
    fn test_constraint_satisfies() {
        let c = VersionConstraint::parse(">= 1.2.0").unwrap();
        assert!(c.satisfies(&RVersion::parse("1.2.0").unwrap()));
        assert!(c.satisfies(&RVersion::parse("1.3.0").unwrap()));
        assert!(!c.satisfies(&RVersion::parse("1.1.0").unwrap()));

        let c = VersionConstraint::parse(">= 0.44.0").unwrap();
        assert!(c.satisfies(&RVersion::parse("0.48.1").unwrap()));
        assert!(!c.satisfies(&RVersion::parse("0.43.0").unwrap()));
    }

    #[test]
    fn test_bioc_version_constraint() {
        // Real example: DESeq2 depends on BiocGenerics (>= 0.44.0)
        let c = VersionConstraint::parse(">= 0.44.0").unwrap();
        let available = RVersion::parse("0.50.0").unwrap();
        assert!(c.satisfies(&available));
    }
}