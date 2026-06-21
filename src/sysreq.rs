// src/sysreq.rs — System dependency checking and resolution
//
// Detection strategy (in order of precedence):
//   1. Capability-based probes — `pkg-config --exists` or `which <tool>`.
//      This is what actually matters: can the R build find the library?
//      Works on any OS, any install method (apt / rpm / brew / HPC modules / manual).
//   2. Package-manager fallback — dpkg (Debian/Ubuntu), rpm (RHEL/Rocky/Fedora),
//      brew (macOS). Used only if capability probe has no entry for the lib.
//
// Why capability-first:
//   - On HPC clusters, compilers and libs come from `module load`, not `apt`.
//     dpkg / rpm will say "not installed" even though the tool is on PATH.
//   - On a random dev machine, someone may have built openssl from source.
//     pkg-config will find it; dpkg won't.
//   - The R build system itself uses pkg-config to locate libraries, so
//     if pkg-config reports the lib, R will find it too.

use crate::resolver::ResolvedDeps;
use anyhow::Result;
use std::process::Command;

/// Result of auditing system dependencies
#[derive(Debug)]
pub struct SysreqReport {
    pub found: Vec<InstalledDep>,
    pub missing: Vec<MissingDep>,
    /// Compiled R packages with no RSPM entry AND no SYSREQ_MAP entry,
    /// but whose DESCRIPTION declares system requirements we couldn't map.
    /// Surfaced to the user with a "consider --skip-sysreq" hint (Bug #2 UX).
    pub uncertain: Vec<UncertainPkg>,

}

/// A compiled package rv couldn't map, plus the cleaned `SystemRequirements:`
/// tokens from its DESCRIPTION. `libs` is the single source of truth: it both
/// gates whether the package is flagged (empty → not uncertain) and is what the
/// advisory displays. See `normalize_sysreq`.
#[derive(Debug)]
pub struct UncertainPkg {
    pub name: String,
    pub libs: Vec<String>,
}

/// Normalize a raw `SystemRequirements:` string into displayable library tokens.
///
/// R's `SystemRequirements:` is unstructured free-text mixing real C libraries
/// (`gmp`, `libpng`, `fftw3`) with things that are never libraries to install:
/// language standards (`C++17`, `C99`) and build tools (`GNU make`). We split on
/// commas/semicolons, strip embedded version constraints, and drop that known
/// non-library noise. Conservative on purpose — `pandoc`, `cmake`, `java` etc.
/// can genuinely be required, so they survive. An empty result means the package
/// declares nothing we'd ask the user to install.
pub fn normalize_sysreq(raw: Option<&str>) -> Vec<String> {
    let raw = match raw {
        Some(s) => s,
        None => return Vec::new(),
    };

    raw.split([',', ';'])
        .map(|token| {
            // Drop a trailing version constraint: "gmp (>= 4.2.3)" → "gmp".
            let token = token.split('(').next().unwrap_or(token);
            token.trim().to_string()
        })
        .filter(|token| !token.is_empty())
        .filter(|token| {
            let lower = token.to_ascii_lowercase();
            // Language standards: "C++", "C++11/14/17/20", "C99", "C11".
            let is_lang_std = lower.starts_with("c++")
                || lower == "c99"
                || lower == "c11"
                || lower == "c89"
                || lower == "c90";
            // Build tools that are always already present in a build env.
            let is_build_tool = lower == "gnu make" || lower == "make";
            !is_lang_std && !is_build_tool
        })
        .collect()
}

#[derive(Debug)]
pub struct InstalledDep {
    pub name: String,
    pub version: String,
}

#[derive(Debug)]
pub struct MissingDep {
    pub name: String,
    pub needed_by: Vec<String>,
}

/// R package → required system libraries (Debian-style canonical names).
/// RHEL/macOS names are translated via RPM_MAP / BREW_MAP below.
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
    ("gert", &["libgit2-dev"]), 
    ("sodium", &["libsodium-dev"]),
    ("RcppGSL", &["libgsl-dev"]),
    ("gsl", &["libgsl-dev"]),
    ("nloptr", &["cmake"]),
    ("sf", &["libgdal-dev", "libgeos-dev", "libproj-dev"]),
    ("terra", &["libgdal-dev", "libgeos-dev", "libproj-dev"]),
    ("magick", &["libmagick++-dev"]),
    ("av", &["libavfilter-dev"]),
    ("ragg", &["libfreetype6-dev", "libpng-dev", "libtiff5-dev"]),
    ("svglite", &["libharfbuzz-dev", "libfribidi-dev"]),
    ("textshaping", &["libharfbuzz-dev", "libfribidi-dev"]),
    ("systemfonts", &["libfontconfig1-dev"]),
    ("cairo", &["libcairo2-dev"]),
    ("rjags", &["jags"]),
    ("RMySQL", &["libmariadb-dev"]),
    ("RPostgres", &["libpq-dev"]),
    ("odbc", &["unixodbc-dev"]),

    // Compilation essentials
    ("Rcpp", &["build-essential"]),
    ("RcppArmadillo", &["build-essential"]),
    ("RcppEigen", &["build-essential"]),
];

/// RUST CONCEPT: Enums with data
/// Like Python's Enum but each variant can carry its own typed payload.
/// This is Rust's standard way to model "one of these cases, each with
/// different data" — cleaner than Python's match-on-isinstance patterns.
///
/// Probe = how to detect a given library without calling a package manager.
enum Probe {
    /// Look for an executable on PATH, e.g. `gcc`, `gfortran`, `cmake`.
    /// Equivalent to: `which <name>` returning 0.
    Bin(&'static str),

    /// Use pkg-config to check for a development library, e.g. `libcurl`, `openssl`.
    /// Equivalent to: `pkg-config --exists <name>` returning 0.
    Pc(&'static str),
    /// Search LD_LIBRARY_PATH for a library file matching this prefix.
    /// e.g. `Ld("libhdf5")` matches `libhdf5.so`, `libhdf5.so.100`, etc.
    /// Workhorse for HPC modules: they update LD_LIBRARY_PATH but typically
    /// ship neither pkg-config files nor binaries on PATH.
    Ld(&'static str),

    /// Try multiple probes in order; succeeds if any one succeeds.
    /// Use this when a library may be installed via different routes —
    /// e.g. pkg-config on a normal Linux box, LD_LIBRARY_PATH on HPC.
    Any(&'static [Probe]),
}

/// PRIMARY detection map: Debian canonical name → capability probe.
///
/// Ordering principle: if a library exposes itself via `pkg-config`, use `Pc`.
/// If it exposes itself as a binary (compilers, *-config tools), use `Bin`.
const CAPABILITY_MAP: &[(&str, Probe)] = &[
    // --- Compilers & build tools (binary probes) ---
    ("build-essential", Probe::Bin("gcc")),
    ("gfortran",        Probe::Bin("gfortran")),
    ("cmake",           Probe::Bin("cmake")),

    // --- Libraries discoverable via pkg-config ---
    ("libcurl4-openssl-dev", Probe::Any(&[
        Probe::Pc("libcurl"),
        Probe::Ld("libcurl"),
    ])),
    ("libssl-dev",           Probe::Any(&[
        Probe::Pc("openssl"),
        Probe::Ld("libssl"),
    ])),
   ("libxml2-dev",          Probe::Any(&[
        Probe::Pc("libxml-2.0"),
        Probe::Ld("libxml2"),
    ])),
    ("libhdf5-dev",          Probe::Any(&[
        Probe::Pc("hdf5"),
        Probe::Ld("libhdf5"),
    ])),
   ("libgsl-dev",           Probe::Any(&[
        Probe::Pc("gsl"),
        Probe::Ld("libgsl"),
    ])),
    ("libgit2-dev",          Probe::Any(&[
        Probe::Pc("libgit2"),
        Probe::Ld("libgit2"),
    ])),
    ("libsodium-dev",        Probe::Pc("libsodium")),
    ("libfontconfig1-dev",   Probe::Pc("fontconfig")),
    ("libharfbuzz-dev",      Probe::Pc("harfbuzz")),
    ("libfribidi-dev",       Probe::Pc("fribidi")),
    ("libfreetype6-dev",     Probe::Any(&[
        Probe::Pc("freetype2"),
        Probe::Ld("libfreetype"),
    ])),
    ("libpng-dev",           Probe::Any(&[
        Probe::Pc("libpng"),
        Probe::Ld("libpng"),
    ])),
    ("libtiff5-dev",         Probe::Pc("libtiff-4")),
    ("libcairo2-dev",        Probe::Pc("cairo")),
    ("libpq-dev",            Probe::Any(&[
        Probe::Pc("libpq"),
        Probe::Ld("libpq"),
    ])),

    ("libproj-dev",          Probe::Pc("proj")),
    ("libavfilter-dev",      Probe::Pc("libavfilter")),

    // --- Libraries that ship their own *-config tool instead of pkg-config ---
    ("libgdal-dev",     Probe::Bin("gdal-config")),
    ("libgeos-dev",     Probe::Bin("geos-config")),
    ("libmariadb-dev",  Probe::Bin("mariadb_config")),
    ("unixodbc-dev",    Probe::Bin("odbc_config")),
    ("libmagick++-dev", Probe::Bin("Magick++-config")),

    // --- Binaries / libs without pkg-config files ---
    ("jags",        Probe::Bin("jags")),
    ("libbz2-dev",  Probe::Bin("bzip2")),   // bzip2 rarely ships pkg-config
    ("liblzma-dev", Probe::Bin("xz")),      // similarly for lzma
];

/// Debian canonical name → RHEL/Rocky/Fedora package name.
/// Used as fallback when capability probe has no entry (or we're installing).
const RPM_MAP: &[(&str, &str)] = &[
    ("libcurl4-openssl-dev", "libcurl-devel"),
    ("libssl-dev",           "openssl-devel"),
    ("libxml2-dev",          "libxml2-devel"),
    ("libhdf5-dev",          "hdf5-devel"),
    ("libgsl-dev",           "gsl-devel"),
    ("libgit2-dev",          "libgit2-devel"),
    ("libsodium-dev",        "libsodium-devel"),
    ("libfontconfig1-dev",   "fontconfig-devel"),
    ("libharfbuzz-dev",      "harfbuzz-devel"),
    ("libfribidi-dev",       "fribidi-devel"),
     ("libfreetype6-dev",     "freetype-devel"),
    ("libpng-dev",           "libpng-devel"),
    ("libtiff5-dev",         "libtiff-devel"),
    ("libcairo2-dev",        "cairo-devel"),
    ("libmagick++-dev",      "ImageMagick-c++-devel"),
   ("libpq-dev",            "libpq-devel"),
    ("libmariadb-dev",       "mariadb-devel"),
    ("libgdal-dev",          "gdal-devel"),
    ("libgeos-dev",          "geos-devel"),
    ("libproj-dev",          "proj-devel"),
    ("libbz2-dev",           "bzip2-devel"),
    ("liblzma-dev",          "xz-devel"),
    ("libavfilter-dev",      "ffmpeg-devel"),
    ("unixodbc-dev",         "unixODBC-devel"),
    ("build-essential",      "gcc-c++"),
    ("gfortran",             "gcc-gfortran"),
    ("cmake",                "cmake"),
    ("jags",                 "jags"),
];

/// Debian canonical name → HPC module name hint (Bug #27).
/// `module spider libgeos-dev` finds nothing; `module spider geos` finds it.
/// Hand-curated mapping for known packages; generic fallback strips the
/// `lib` prefix and `-dev`/`-devel` suffix.
const MODULE_HINT_MAP: &[(&str, &str)] = &[
    ("libcurl4-openssl-dev", "curl"),
    ("libssl-dev",           "openssl"),
    ("libxml2-dev",          "libxml2"),
    ("libhdf5-dev",          "hdf5"),
    ("libgsl-dev",           "gsl"),
    ("libgit2-dev",          "libgit2"),
    ("libsodium-dev",        "libsodium"),
    ("libfontconfig1-dev",   "fontconfig"),
    ("libharfbuzz-dev",      "harfbuzz"),
    ("libfribidi-dev",       "fribidi"),
    ("libfreetype6-dev",     "freetype"),
    ("libpng-dev",           "libpng"),
    ("libtiff5-dev",         "libtiff"),
    ("libcairo2-dev",        "cairo"),
    ("libpq-dev",            "postgresql"),
    ("libmariadb-dev",       "mariadb"),
    ("libgdal-dev",          "gdal"),
    ("libgeos-dev",          "geos"),
    ("libproj-dev",          "proj"),
    ("libbz2-dev",           "bzip2"),
    ("liblzma-dev",          "xz"),
    ("libmagick++-dev",      "imagemagick"),
    ("libavfilter-dev",      "ffmpeg"),
    ("unixodbc-dev",         "unixODBC"),
    ("build-essential",      "gcc"),
    ("gfortran",             "gcc"),
];

/// Translate a Debian-canonical sysreq name to the most likely HPC module name.
pub fn module_hint(debian_name: &str) -> String {
    if let Some(hint) = MODULE_HINT_MAP
        .iter()
        .find(|(d, _)| *d == debian_name)
        .map(|(_, m)| *m)
    {
        return hint.to_string();
    }
    let mut s = debian_name;
    if let Some(stripped) = s.strip_prefix("lib") {
        s = stripped;
    }
    if let Some(stripped) = s.strip_suffix("-dev") {
        s = stripped;
    } else if let Some(stripped) = s.strip_suffix("-devel") {
        s = stripped;
    }
    s.to_string()
}

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
    ("gfortran", "gcc"),
    ("cmake", "cmake"),
];

/// RUST CONCEPT: Enums as lightweight tags
/// Think of this as a Python Enum but zero runtime cost.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LinuxFamily {
    Debian, // Ubuntu, Debian, Mint, ...
    Rhel,   // RHEL, Rocky, CentOS, AlmaLinux, Fedora
    Unknown,
}

/// Detect the Linux distribution family by reading /etc/os-release.
///
/// The `ID=` and `ID_LIKE=` fields are standardized by systemd's os-release spec.
/// Examples:
///   Ubuntu:    ID=ubuntu  ID_LIKE=debian
///   Rocky:     ID=rocky   ID_LIKE="rhel centos fedora"
///   Fedora:    ID=fedora  (no ID_LIKE)
fn linux_family() -> LinuxFamily {
    let content = match std::fs::read_to_string("/etc/os-release") {
        Ok(s) => s,
        Err(_) => return LinuxFamily::Unknown,
    };

    let (mut id, mut id_like) = (String::new(), String::new());
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("ID=") {
            id = v.trim_matches('"').to_lowercase();
        } else if let Some(v) = line.strip_prefix("ID_LIKE=") {
            id_like = v.trim_matches('"').to_lowercase();
        }
    }

    let combined = format!("{} {}", id, id_like);
    if ["debian", "ubuntu", "mint"].iter().any(|k| combined.contains(k)) {
        LinuxFamily::Debian
    } else if ["rhel", "fedora", "centos", "rocky", "almalinux", "ol"]
        .iter().any(|k| combined.contains(k))
    {
        LinuxFamily::Rhel
    } else {
        LinuxFamily::Unknown
    }
}
/// Is Homebrew available? Used by the install path to decide whether
/// to offer auto-install of macOS sysreqs.
pub fn has_brew() -> bool {
    Command::new("brew")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn is_macos() -> bool {
    std::env::consts::OS == "macos"
}
/// Detect HPC environment via module-system env vars (Bug #1).
/// Lmod sets LMOD_CMD; classic modules set MODULESHOME; both set MODULEPATH.
/// On HPC, sudo apt/dnf will fail — we adapt prompts and skip RSPM calls
/// (no outbound network on compute nodes is common).
pub fn is_hpc_environment() -> bool {
    std::env::var("LMOD_CMD").is_ok()
        || std::env::var("MODULESHOME").is_ok()
        || std::env::var("MODULEPATH").is_ok()
}
/// Bug #44 + #49: detect HDF5 module problems before they cause hdf5r failures.
///
/// Common HPC scenarios:
///   - Module loads an MPI-only HDF5 build (h5pcc present, h5cc missing)
///     → hdf5r's configure script needs h5cc/h5pcc; if only h5pcc is there
///        but MPI itself isn't loaded, the build dies on `mpi.h`.
///   - No HDF5 module loaded at all: silent until install fails.
///
/// Returns a warning string if there's a problem, None otherwise.
/// Only fires when an HDF5-needing package is being installed.
pub fn detect_hdf5_problem(package_names: &[String]) -> Option<String> {
    let hdf5_packages = ["hdf5r", "rhdf5", "HDF5Array", "Rhtslib", "Rsamtools"];
    let needs_hdf5 = package_names
        .iter()
        .any(|p| hdf5_packages.contains(&p.as_str()));
    if !needs_hdf5 {
        return None;
    }

    let has_h5cc = has_binary("h5cc");
    let has_h5pcc = has_binary("h5pcc");
    let has_mpicc = has_binary("mpicc") || has_binary("mpicxx");

    // Serial HDF5 available — but is its *version* compatible? The CRAN hdf5r
    // (1.3.x) only supports the HDF5 1.10–1.14 API. HDF5 2.x removed symbols it
    // relies on (e.g. H5FD_family_init, plain H5Dread_chunk), so a too-new HDF5
    // makes hdf5r fail — at compile time, or worse, only at load time when the
    // build mixes new headers with an old/new lib. Homebrew ships HDF5 2.x by
    // default, so this bites Mac users hard. Warn before the long compile.
    if has_h5cc {
        if let Some((major, full)) = hdf5_version() {
            if major >= 2 {
                return Some(format!(
                    "Active HDF5 is {} — too new for hdf5r (needs the 1.10–1.14 API).\n  \
                     Building against HDF5 2.x fails (missing symbols like H5FD_family_init),\n  \
                     or compiles but won't load ('symbol not found: _H5Dread_chunk').\n  \
                     Fixes:\n    \
                       macOS: brew install hdf5@1.10 && brew unlink hdf5 && \
                     brew link --overwrite --force hdf5@1.10\n           \
                       (relink hdf5 afterward so gdal/netcdf keep working)\n    \
                       any OS: conda install -c conda-forge r-hdf5r  (precompiled, bundles HDF5)",
                    full
                ));
            }
        }
        return None;
    }

    // MPI HDF5 loaded but no MPI compiler — will fail on mpi.h.
    if has_h5pcc && !has_mpicc {
        return Some(
            "MPI HDF5 loaded (h5pcc only, no h5cc) AND no MPI compiler detected.\n  \
             hdf5r/rhdf5 needs either:\n    \
               (a) a serial HDF5 module — try: module spider hdf5\n    \
               (b) MPI loaded alongside — try: module load mpich (or openmpi/mvapich)\n  \
             Alternative: conda install -c conda-forge hdf5".to_string()
        );
    }

    // No HDF5 binaries at all — install will fail with a clear error anyway.
    None
}
/// Probe the active HDF5 library version. Returns (major, full_version).
///
/// Uses `h5cc -showconfig` first (that's exactly what hdf5r's configure runs,
/// so it reflects the version the build will actually use), falling back to
/// `pkg-config --modversion hdf5`.
fn hdf5_version() -> Option<(u32, String)> {
    use std::process::Command;

    // h5cc -showconfig prints a line like "    HDF5 Version: 2.1.1".
    if let Ok(out) = Command::new("h5cc").arg("-showconfig").output() {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some(rest) = line.split("HDF5 Version:").nth(1) {
                if let Some(v) = parse_major_version(rest.trim()) {
                    return Some(v);
                }
            }
        }
    }

    // Fallback: pkg-config reports just the version string, e.g. "2.1.1".
    if let Ok(out) = Command::new("pkg-config")
        .args(["--modversion", "hdf5"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(v) = parse_major_version(text.trim()) {
            return Some(v);
        }
    }

    None
}

/// Parse a version string like "2.1.1" into (major, "2.1.1").
fn parse_major_version(s: &str) -> Option<(u32, String)> {
    let v = s.trim();
    let major = v.split('.').next()?.parse::<u32>().ok()?;
    Some((major, v.to_string()))
}

/// Detect installed gcc version. Returns (major, full_version_string).
/// Bug #14: gcc 15+ is too strict for many R packages (esp. older Bioc).
pub fn detect_gcc_version() -> Option<(u32, String)> {
    let output = Command::new("gcc").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next()?.to_string();

    // Bug #14 fix: only flag GNU gcc. Apple clang reports as "gcc" on macOS
    // but has different rules and shouldn't trigger the gcc-15-too-strict warning.
    if first_line.to_lowercase().contains("clang") {
        return None;
    }

    for token in first_line.split_whitespace() {
        let cleaned = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if let Some(major_str) = cleaned.split('.').next() {
            if let Ok(major) = major_str.parse::<u32>() {
                if major >= 4 && major <= 99 {
                    return Some((major, first_line));
                }
            }
        }
    }
    None
}

/// Parsed /etc/os-release values normalized to RSPM-canonical names.
struct OsRelease {
    distribution: String, // "ubuntu", "debian", "rockylinux", "redhat", "centos", "opensuse"
    release: String,      // "22.04" or "9" depending on distro convention
}

/// Read /etc/os-release and return RSPM-canonical (distribution, release).
/// Returns None on macOS or unparseable systems.
fn os_release_info() -> Option<OsRelease> {
    if is_macos() {
        return None;
    }
    let content = std::fs::read_to_string("/etc/os-release").ok()?;

    let mut id = String::new();
    let mut version_id = String::new();
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("ID=") {
            id = v.trim_matches('"').to_lowercase();
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            version_id = v.trim_matches('"').to_string();
        }
    }
    if id.is_empty() || version_id.is_empty() {
        return None;
    }

    // RSPM names: rocky/alma → rockylinux, RHEL → redhat, Oracle → oraclelinux.
    // RSPM canonical names : RSPM rejects "rockylinux" and "rhel" with HTTP 400.
    // "centos" works for Rocky/Alma (binary-compatible); "redhat" is the canonical RHEL name.
    let distribution = match id.as_str() {
        "rocky" | "almalinux" | "centos" => "centos".to_string(),
        "rhel" => "redhat".to_string(),
        "ol" => "oraclelinux".to_string(),
        _ => id,
    };

    // RHEL-family: RSPM wants just the major version ("9.4" → "9").
    let release = match distribution.as_str() {
        "rockylinux" | "redhat" | "centos" | "oraclelinux" => version_id
            .split('.')
            .next()
            .unwrap_or(&version_id)
            .to_string(),
        _ => version_id,
    };

    Some(OsRelease { distribution, release })
}

// ---------------------------------------------------------------------------
// Capability probes
// ---------------------------------------------------------------------------

/// Is this binary on PATH?
/// Python equivalent: `shutil.which(name) is not None`
fn has_binary(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Does pkg-config know about this library?
fn has_pkgconfig(name: &str) -> bool {
    Command::new("pkg-config")
        .args(["--exists", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
/// Does any directory in LD_LIBRARY_PATH contain a library file
/// starting with this prefix?
fn has_ld_library(prefix: &str) -> bool {
    let paths = match std::env::var("LD_LIBRARY_PATH") {
        Ok(p) => p,
        Err(_) => return false,
    };

    for dir in paths.split(':').filter(|s| !s.is_empty()) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Match libfoo.so, libfoo.so.1, libfoo.so.1.2.3, libfoo.dylib.
            if name_str.starts_with(prefix)
                && (name_str.contains(".so") || name_str.ends_with(".dylib"))
            {
                return true;
            }
        }
    }
    false
}

/// Get the version string from pkg-config (best-effort).
fn pkgconfig_version(name: &str) -> String {
    Command::new("pkg-config")
        .args(["--modversion", name])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "detected".to_string())
}

/// Try capability-based detection.
/// Returns `Some(version)` if the library is usable on this system,
/// regardless of how it was installed.
fn check_capability(lib_name: &str) -> Option<String> {
    let probe = CAPABILITY_MAP
        .iter()
        .find(|(n, _)| *n == lib_name)
        .map(|(_, p)| p)?;

    run_probe(probe)
}

/// Recursive probe runner — separates dispatch from lookup so `Any`
/// can call back into itself without re-resolving CAPABILITY_MAP.
fn run_probe(probe: &Probe) -> Option<String> {
    match probe {
        Probe::Bin(name) => {
            if has_binary(name) {
                Some("detected".to_string())
            } else {
                None
            }
        }
        Probe::Pc(name) => {
            if has_pkgconfig(name) {
                Some(pkgconfig_version(name))
            } else {
                None
            }
        }
        Probe::Ld(prefix) => {
            if has_ld_library(prefix) {
                Some("detected (LD_LIBRARY_PATH)".to_string())
            } else {
                None
            }
        }
        Probe::Any(probes) => probes.iter().find_map(run_probe),
    }
}

// ---------------------------------------------------------------------------
// Package-manager fallback checks
// ---------------------------------------------------------------------------

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

fn check_rpm_installed(package_name: &str) -> Option<String> {
    // Translate Debian-style canonical name → RHEL package name
    let rpm_name = RPM_MAP
        .iter()
        .find(|(deb, _)| *deb == package_name)
        .map(|(_, rpm)| *rpm)
        .unwrap_or(package_name);

    // rpm -q prints "package not installed" to stdout (with non-zero exit) when absent,
    // or "<name>-<version>-<release>" when present. Using --qf makes parsing reliable.
    let output = Command::new("rpm")
        .args(["-q", "--qf", "%{VERSION}", rpm_name])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() || version.contains("not installed") {
        None
    } else {
        Some(version)
    }
}

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
    let version = trimmed.split_whitespace().last().unwrap_or("unknown");
    Some(version.to_string())
}

/// Unified install-check: capability first, package manager second.
fn check_installed(package_name: &str) -> Option<String> {
    // Primary: capability-based detection.
    if let Some(v) = check_capability(package_name) {
        return Some(v);
    }
    // Fallback: OS-specific package manager.
    if is_macos() {
        check_brew_installed(package_name)
    } else {
        match linux_family() {
            LinuxFamily::Debian => check_dpkg_installed(package_name),
            LinuxFamily::Rhel => check_rpm_installed(package_name),
            LinuxFamily::Unknown => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub async fn audit(resolved: &ResolvedDeps) -> Result<SysreqReport> {
    // Routing context.
    let on_macos = is_macos();
    let on_hpc = is_hpc_environment();
    let os_info = os_release_info();

    //query RSPM whenever we have OS info, including on HPC.
    // Login nodes have network; compute nodes typically don't, but the
    // 3-second per-query timeout plus the rspm_unreachable short-circuit
    // below limits the cost to one 3s hang on a no-network node.
    let use_rspm = !on_macos && os_info.is_some();
    let mut rspm_cache = if use_rspm {
        let o = os_info.as_ref().unwrap();
        rspm::load(&o.distribution, &o.release)
    } else {
        rspm::Cache::default()
    };
    let client = if use_rspm { Some(reqwest::Client::new()) } else { None };

    let mut required_syslibs: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut uncertain: Vec<UncertainPkg> = Vec::new();
    let mut cache_dirty = false;
    //  first failed RSPM query short-circuits the rest of the
    // queries this run. Avoids 3s × N timeouts on compute nodes.
    let mut rspm_unreachable = false;

    for pkg in &resolved.packages {
        let libs = lookup_sysreqs_for_pkg(
            pkg,
            &os_info,
            use_rspm,
            &mut rspm_cache,
            &mut cache_dirty,
            &mut rspm_unreachable,
            client.as_ref(),
        )
        .await;

        match libs {
            Some(libs) => {
                for lib in libs {
                    required_syslibs.entry(lib).or_default().push(pkg.name.clone());
                }
            }
            None => {
                // Pure-R packages with no entry almost certainly need nothing.
                // Only mark compiled packages with no mapping as uncertain — and
                // only when DESCRIPTION declares real libs after normalization
                // (a package whose sole requirement is "C++17" needs nothing).
                if pkg.needs_compilation && !is_in_sysreq_map(&pkg.name) {
                    let libs = normalize_sysreq(pkg.system_requirements.as_deref());
                    if !libs.is_empty() {
                        uncertain.push(UncertainPkg { name: pkg.name.clone(), libs });
                    }
                }
            }
        }

        // Compilation tooling (unchanged from before).
        if pkg.needs_compilation {
            required_syslibs
                .entry("build-essential".to_string())
                .or_default()
                .push(pkg.name.clone());

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

    // Persist cache only if RSPM gave us new info.
    if cache_dirty {
        if let Some(o) = &os_info {
            let _ = rspm::save(&o.distribution, &o.release, &rspm_cache);
        }
    }

    // Capability probes (unchanged) — these are the authoritative "is it usable?" check.
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for (lib_name, needed_by) in &required_syslibs {
        if let Some(version) = check_installed(lib_name) {
            found.push(InstalledDep { name: lib_name.clone(), version });
        } else {
            missing.push(MissingDep {
                name: lib_name.clone(),
                needed_by: needed_by.clone(),
            });
        }
    }

    Ok(SysreqReport { found, missing, uncertain })
}

/// Per-package router. Returns Some(libs) on a definitive answer (may be empty),
/// None when neither RSPM nor SYSREQ_MAP can tell us anything.
async fn lookup_sysreqs_for_pkg(
    pkg: &crate::resolver::ResolvedPackage,
    os_info: &Option<OsRelease>,
    use_rspm: bool,
    rspm_cache: &mut rspm::Cache,
    cache_dirty: &mut bool,
    rspm_unreachable: &mut bool,
    client: Option<&reqwest::Client>,
) -> Option<Vec<String>> {
    // RSPM is CRAN-only. Bioc / GitHub / macOS skip straight to SYSREQ_MAP.
    let rspm_eligible =
        use_rspm && pkg.source == "cran" && client.is_some() && !*rspm_unreachable;

    if rspm_eligible {
    let os = os_info.as_ref().unwrap();
    if let Some(libs) = rspm_cache.entries.get(&pkg.name) {
        // Bug #4: cached empty result with non-empty DESCRIPTION SystemRequirements
        // means RSPM's rule database doesn't cover this package — surface as uncertain.
        if libs.is_empty() && pkg.system_requirements.as_deref().map_or(false, |s| !s.trim().is_empty()) {
            return None;
        }
        return Some(libs.clone());
    }
    if let Some(libs) =
        rspm::query(client.unwrap(), &pkg.name, &os.distribution, &os.release).await
    {
        rspm_cache.entries.insert(pkg.name.clone(), libs.clone());
        *cache_dirty = true;
        // Same check on fresh RSPM response.
        if libs.is_empty() && pkg.system_requirements.as_deref().map_or(false, |s| !s.trim().is_empty()) {
            return None;
        }
        return Some(libs);
    }
    *rspm_unreachable = true;
}

    // SYSREQ_MAP — for Bioc, GitHub, macOS, HPC, or RSPM fallback.
    for (r_pkg, sys_libs) in SYSREQ_MAP {
        if pkg.name == *r_pkg {
            return Some(sys_libs.iter().map(|s| s.to_string()).collect());
        }
    }
    None
}

fn is_in_sysreq_map(name: &str) -> bool {
    SYSREQ_MAP.iter().any(|(n, _)| *n == name)
}

pub fn get_brew_name(linux_name: &str) -> String {
    BREW_MAP
        .iter()
        .find(|(linux, _)| *linux == linux_name)
        .map(|(_, brew)| brew.to_string())
        .unwrap_or_else(|| linux_name.to_string())
}

/// Install missing system packages using the correct package manager.
pub fn install_missing(report: &SysreqReport) -> Result<()> {
    if report.missing.is_empty() {
        return Ok(());
    }

    if is_macos() {
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
        let status = Command::new("brew").arg("install").args(&brew_names).status()?;
        if !status.success() {
            anyhow::bail!(
                "Failed to install. Run manually:\n  brew install {}",
                brew_names.join(" ")
            );
        }
        return Ok(());
    }

    match linux_family() {
        LinuxFamily::Debian => {
            let names: Vec<&str> = report.missing.iter().map(|d| d.name.as_str()).collect();
            println!("Running: sudo apt install -y {}", names.join(" "));
            let status = Command::new("sudo")
                .arg("apt").arg("install").arg("-y")
                .args(&names)
                .status()?;
            if !status.success() {
                anyhow::bail!(
                    "Failed to install. Run manually:\n  sudo apt install {}",
                    names.join(" ")
                );
            }
        }
        LinuxFamily::Rhel => {
            let rpm_names: Vec<String> = report
                .missing
                .iter()
                .map(|d| {
                    RPM_MAP
                        .iter()
                        .find(|(deb, _)| *deb == d.name.as_str())
                        .map(|(_, rpm)| rpm.to_string())
                        .unwrap_or_else(|| d.name.clone())
                })
                .collect();

            // Prefer dnf (modern), fall back to yum.
            let installer = if has_binary("dnf") { "dnf" } else { "yum" };
            println!("Running: sudo {} install -y {}", installer, rpm_names.join(" "));
            let status = Command::new("sudo")
                .arg(installer).arg("install").arg("-y")
                .args(&rpm_names)
                .status()?;
            if !status.success() {
                anyhow::bail!(
                    "Failed to install.\n\
                     If you are on an HPC system, you likely do not have sudo privileges.\n\
                     Ask your admin, or load the appropriate environment modules \
                     (e.g. `module load gcc openssl libcurl`).\n\
                     Manual command: sudo {} install {}",
                    installer,
                    rpm_names.join(" ")
                );
            }
        }
        LinuxFamily::Unknown => {
            anyhow::bail!(
                "Could not detect Linux distribution family. Please install these manually: {}",
                report.missing.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Makevars fixup (macOS-specific, unchanged)
// ---------------------------------------------------------------------------

pub fn check_makevars() -> Option<MakevarsFix> {
    if !is_macos() {
        return None;
    }

    let gfortran_check = Command::new("gfortran").arg("--version").output().ok()?;
    if !gfortran_check.status.success() {
        return None;
    }

    let flibs_output = Command::new("R").args(["CMD", "config", "FLIBS"]).output().ok()?;
    let flibs = String::from_utf8_lossy(&flibs_output.stdout).trim().to_string();

    let bad_paths: Vec<String> = flibs
        .split_whitespace()
        .filter(|s| s.starts_with("-L"))
        .map(|s| s.trim_start_matches("-L").to_string())
         .filter(|path| {
            // Bug #56: a path is "bad" if either:
            //   (a) the directory doesn't exist, OR
            //   (b) the directory exists but contains no libgfortran*
            //       (empty stubs left behind by old R installers).
            let p = std::path::Path::new(path);
            if !p.exists() {
                return true;
            }
            // Directory exists — check whether libgfortran* actually lives there.
            match std::fs::read_dir(p) {
                Ok(entries) => !entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("libgfortran")),
                Err(_) => true, // can't read it — treat as bad
            }
        })
        .collect();

    if bad_paths.is_empty() {
        return None;
    }

    let gfortran_path = Command::new("which")
        .arg("gfortran")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let correct_lib = if gfortran_path.contains("/opt/homebrew/") {
        "/opt/homebrew/lib/gcc/current".to_string()
    } else if gfortran_path.contains("/usr/local/") {
        "/usr/local/lib/gcc/current".to_string()
    } else {
        let parent = std::path::Path::new(&gfortran_path)
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("lib/gcc/current"))
            .unwrap_or_default();
        parent.to_string_lossy().to_string()
    };

    let makevars_path = dirs_or_home().join(".R/Makevars");
    if makevars_path.exists() {
        let content = std::fs::read_to_string(&makevars_path).unwrap_or_default();
        if content.contains(&correct_lib) {
            return None;
        }
    }

    Some(MakevarsFix {
        bad_paths,
        correct_lib,
        gfortran_path,
        makevars_path,
    })
}

fn dirs_or_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
}

pub struct MakevarsFix {
    pub bad_paths: Vec<String>,
    pub correct_lib: String,
    pub gfortran_path: String,
    pub makevars_path: std::path::PathBuf,
}

pub fn fix_makevars(fix: &MakevarsFix) -> Result<()> {
    let r_dir = fix.makevars_path.parent().unwrap();
    std::fs::create_dir_all(r_dir)?;

    let makevars_content = format!(
        "# Added by rv — fixes gfortran path for Homebrew\n\
         FC = {}\n\
         FLIBS = -L{} -lgfortran -lquadmath\n",
        fix.gfortran_path, fix.correct_lib
    );

    if fix.makevars_path.exists() {
        let existing = std::fs::read_to_string(&fix.makevars_path)?;
        if !existing.contains("FC =") {
            let updated = format!("{}\n{}", existing.trim(), makevars_content);
            std::fs::write(&fix.makevars_path, updated)?;
        }
    } else {
        std::fs::write(&fix.makevars_path, makevars_content)?;
    }

    Ok(())
}
// ---------------------------------------------------------------------------
// RSPM sysreqs lookup (Bug #3)
//
// Layered design:
//   1. On-disk cache  (~/.cache/rv/sysreqs/{distro}-{release}.json) — survives runs
//   2. RSPM HTTP API  (3s timeout; cache write on success)
//   3. SYSREQ_MAP     (fallback; also the only source for Bioc/GitHub/macOS)
//
// We skip RSPM entirely on HPC (often no outbound network) and on macOS
// (RSPM returns Linux names). Capability probes downstream are unchanged.
// ---------------------------------------------------------------------------

mod rspm {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;
    use serde::{Deserialize, Serialize};

    const RSPM_BASE: &str = "https://packagemanager.posit.co/__api__/repos/cran";
    const TIMEOUT_SECS: u64 = 3;

    /// Persistent cache keyed by R-package name. Empty Vec = "RSPM says no sysreqs".
    /// Key absent = "never queried" (caller falls back to SYSREQ_MAP).
    #[derive(Serialize, Deserialize, Default)]
    pub struct Cache {
        pub entries: HashMap<String, Vec<String>>,
    }

    fn cache_path(distro: &str, release: &str) -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        Some(PathBuf::from(home)
            .join(".cache/rv/sysreqs")
            .join(format!("{}-{}.json", distro, release)))
    }

    pub fn load(distro: &str, release: &str) -> Cache {
        cache_path(distro, release)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(distro: &str, release: &str, cache: &Cache) -> anyhow::Result<()> {
        let path = cache_path(distro, release)
            .ok_or_else(|| anyhow::anyhow!("HOME unset; cannot write sysreq cache"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(cache)?)?;
        Ok(())
    }

    /// Query RSPM for one R package. Returns Some(libs) on a definitive
    /// answer (possibly empty list); None on miss/error so caller can
    /// fall through to SYSREQ_MAP.
    ///
    /// Uses .text() + manual serde_json parse to avoid needing reqwest's
    /// "json" feature in Cargo.toml.
    pub async fn query(
        client: &reqwest::Client,
        pkg: &str,
        distro: &str,
        release: &str,
    ) -> Option<Vec<String>> {
        let url = format!(
            "{base}/sysreqs?all=false&pkgname={pkg}&distribution={distro}&release={release}",
            base = RSPM_BASE,
            pkg = pkg,
            distro = distro,
            release = release,
        );

        let resp = client
            .get(&url)
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }

        let text = resp.text().await.ok()?;
        let json: serde_json::Value = serde_json::from_str(&text).ok()?;

        // Shape: { "requirements": [ { "requirements": { "packages": [...] }, ... }, ... ] }
        let reqs = json.get("requirements")?.as_array()?;
        let mut libs: Vec<String> = Vec::new();
        for r in reqs {
            if let Some(pkgs) = r.pointer("/requirements/packages").and_then(|p| p.as_array()) {
                for p in pkgs {
                    if let Some(s) = p.as_str() {
                        let s = s.to_string();
                        if !libs.contains(&s) {
                            libs.push(s);
                        }
                    }
                }
            }
        }
        Some(libs)
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_sysreq;

    #[test]
    fn none_yields_empty() {
        assert!(normalize_sysreq(None).is_empty());
    }

    #[test]
    fn blank_yields_empty() {
        assert!(normalize_sysreq(Some("   ")).is_empty());
    }

    #[test]
    fn strips_version_constraints() {
        assert_eq!(
            normalize_sysreq(Some("gmp (>= 4.2.3), mpfr (>= 3.0.0)")),
            vec!["gmp", "mpfr"]
        );
    }

    #[test]
    fn drops_language_standards() {
        // A package whose sole requirement is a language standard needs nothing.
        assert!(normalize_sysreq(Some("C++17")).is_empty());
        assert!(normalize_sysreq(Some("C99")).is_empty());
        assert!(normalize_sysreq(Some("C++")).is_empty());
    }

    #[test]
    fn drops_make_keeps_real_libs() {
        // "GNU make" is noise; libpng is a real lib that should survive.
        assert_eq!(
            normalize_sysreq(Some("GNU make, libpng")),
            vec!["libpng"]
        );
    }

    #[test]
    fn keeps_ambiguous_tools() {
        // Conservative: pandoc / cmake / java can genuinely be required.
        assert_eq!(
            normalize_sysreq(Some("pandoc, cmake, java")),
            vec!["pandoc", "cmake", "java"]
        );
    }

    #[test]
    fn splits_on_semicolons_and_commas() {
        assert_eq!(
            normalize_sysreq(Some("fftw3; Clp, libpng")),
            vec!["fftw3", "Clp", "libpng"]
        );
    }

    #[test]
    fn mixed_noise_and_libs() {
        // Real-world-ish: language std + build tool + actual libs.
        assert_eq!(
            normalize_sysreq(Some("C++17, GNU make, gmp (>= 4.2.3), fftw3")),
            vec!["gmp", "fftw3"]
        );
    }
}