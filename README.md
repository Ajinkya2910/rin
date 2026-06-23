# rin

**A fast, reliable R package manager for life sciences.**

rin is to R what [uv](https://github.com/astral-sh/uv) is to Python — a fast standalone package manager that handles dependency resolution, installation, and reproducible environments as one tool, without needing the language runtime installed to bootstrap. For R that means: a single binary that resolves, audits, and installs CRAN + Bioconductor packages with parallel compilation, lockfiles, and project-isolated environments — built with correctness on real-world bioinformatics setups as the non-negotiable baseline.

Built in Rust. First-class Bioconductor support. No background R process. No system R assumptions. No surprises halfway through a 56-package compile.

---

## The problem

Try installing DESeq2 on a fresh conda-R environment on an HPC cluster — the standard setup for reproducible bioinformatics — using the two most common R package managers:

| Tool | Result | Time |
|---|---|---|
| `BiocManager::install("DESeq2")` | ❌ **DESeq2 not installed.** Cascade failure across 6 packages starting from `zlib.h: No such file or directory` in XVector. | 10m 01s (failed) |
| `pak::pkg_install("bioc::DESeq2")` | ❌ Same cascade failure. Same cryptic compiler error. Same broken end state. | (failed) |
| **`rin install DESeq2`** | ✅ **56 packages installed, first try.** Pre-flight audit identified system dependencies before any compilation started. | **6m 02s** |

Both BiocManager and pak delegate compilation to R's build system, which silently assumes the user has already sorted out system dependencies. When they haven't, the install crashes mid-compile with a C-level error, and every downstream package falls apart. The user is left debugging compiler output across six failures to find the one real issue.

rin runs a pre-flight audit *before* it starts compiling: does the compiler exist, does `pkg-config` find the library, is the Fortran path sane? If anything looks off, rin flags it up front — in language a bioinformatician can act on, not a linker error 40 lines into a build log. By default the audit is advisory (it lists what looks missing and proceeds, letting the compiler confirm real blockers); `--strict-sysreq` turns it into a hard gate for CI.

**That's the thesis.** Speed is a bonus. Reliability is the product.

---

## Quick start

**Install rin** (one line, no Rust toolchain needed):

```bash
curl -sSf https://raw.githubusercontent.com/Ajinkya2910/rin/main/install.sh | sh
export PATH="$HOME/.rin/bin:$PATH"
```

**Install DESeq2 into an isolated environment:**

```bash
rin venv myproject
source myproject/activate
rin install DESeq2
```

Done. No sudo, no conda, no `BiocManager::install` incantations, no manual system-lib hunting.

**The full command surface, at a glance:**

`rin resolve` · `rin audit` · `rin install` · `rin lock` · `rin restore` · `rin why` · `rin venv`

(Detailed reference [below](#commands).)

---

## What rin does differently

### 1. Constraint-aware dependency resolution

BiocManager doesn't validate version constraints across the transitive dependency tree. When packages have conflicting requirements (`Depends: Matrix (>= 1.5)` in one place, `Depends: Matrix (< 1.4)` in another), you get the latest of each — and the conflict only surfaces later, often at package load time with a cryptic error.

rin parses every version constraint (`>=`, `>`, `<=`, `<`, `==`) across `Depends`, `Imports`, and `LinkingTo`, then validates the full tree. When a constraint can't be satisfied, rin tells you exactly which package wants what, and falls back to CRAN Archive to find a satisfying older version if needed.

### 2. First-class Bioconductor support

Bioconductor isn't an afterthought. rin fetches software, annotation, and experiment repositories at the correct version for your R version. Bioconductor packages get priority over CRAN when a name exists in both (e.g. `Matrix` → CRAN, but `GenomeInfoDbData` → Bioconductor annotation).

### 3. Pre-flight system audit

Before compiling anything, rin checks for required system libraries using **capability-based detection** (`pkg-config`, `which`) rather than package-manager heuristics. This matters because:

- On HPC, libraries come from `module load`, not `apt` or `rpm`
- On Mac, some tools come from Xcode, not Homebrew
- On a dev machine, someone may have built openssl from source

The question rin asks isn't "is the package installed" — it's "can R find this library when it needs to?" That's what actually determines whether the build succeeds.

The audit is **advisory by default**: rin lists anything that looks missing, then proceeds and lets the compiler confirm the real blockers (the sysreq map is curated, so a false positive shouldn't stop your install). You can tune this per-run:

```bash
rin install gert --ignore-missing libgit2-dev   # ignore one specific lib
rin install DESeq2 --skip-sysreq                 # skip the pre-flight entirely
rin install DESeq2 --strict-sysreq               # hard-fail if anything looks missing (CI)
```

### 4. Deterministic lockfiles

```bash
rin lock DESeq2 clusterProfiler   # writes rin.lock with pinned versions
rin restore                       # reproduces the exact environment elsewhere
```

BiocManager has no equivalent. renv exists but needs R to bootstrap. rin's lockfile is plain TOML, generated by the Rust binary, readable by anything.

### 5. Isolated virtual environments

Like Python's `venv`, rin venvs give each project its own R library:

```bash
rin venv analysis-2026
source analysis-2026/activate
```

`R_LIBS_USER` gets pointed at the venv; packages install there; deactivation restores the original environment. No system library pollution, no cross-project version drift.

### 6. Parallel compilation with tier-aware scheduling

rin computes the dependency DAG, groups packages into tiers (packages in tier N only depend on tiers < N), and compiles each tier in parallel using [rayon](https://github.com/rayon-rs/rayon). No background R process, no Rscript subprocess overhead — just parallel `R CMD INSTALL`.

### 7. Standalone binary, no Rust required

rin ships as a single statically-linked binary (musl on Linux, native on macOS). No `R`-based bootstrap, no runtime Python or Node dependencies, no shared library version mismatches. It just runs.

---

## Benchmark details

All numbers below are from a real run on a GWU HPC login node (Rocky Linux, x86_64, R 4.4.2, DESeq2 with 55 transitive dependencies).

### Clean environment (all 56 packages built from source)

| Tool | Environment | Outcome | Time |
|---|---|---|---|
| **rin** | Fresh `rin venv` | ✅ 56 packages installed, first try | **6m 02s** |
| BiocManager | Fresh conda-R env | ❌ 6 cascade failures — DESeq2 not installed | 10m 01s |
| pak | Fresh conda-R env (no zlib) | ❌ Same zlib cascade failure | — |

### With binary caching (best case for each tool)

| Tool | Environment | Outcome | Time |
|---|---|---|---|
| pak | Fresh conda-R env + zlib, PPM binaries available | ✅ 56 packages via Posit Public Package Manager binaries | **2m 47s** |

> **Read this before drawing conclusions from the numbers.**
>
> When pak has PPM binaries available, it skips compilation entirely and finishes in 2m 47s. rin compiles all 56 packages from source in 6m 02s. **This isn't a resolver comparison — it's cached binaries vs. source compilation.** Comparing them on speed alone is comparing apples to oranges.
>
> When PPM doesn't cover your platform — macOS conda-R, non-mainstream Linux distros, aarch64, air-gapped HPC, custom toolchains — pak falls back to source compilation and hits the same cascade failures as BiocManager. rin's pre-flight audit handles the source-build path reliably across all of those environments today.
>
> rin will match pak's binary-cache speed when [Phase 2 infrastructure](#roadmap) ships (Hetzner build farm + Cloudflare R2 storage, Bioconductor-first, HPC-targeted). Until then, rin's value is **always-reliable source builds**, not raw speed.

### BiocManager cascade: root cause

For the curious, here's the first error in the BiocManager / pak cascade, buried ~40 lines up from the final error:

```
io_utils.c:16:10: fatal error: zlib.h: No such file or directory
   16 | #include <zlib.h>
      |          ^~~~~~~~
compilation terminated.
```

One missing header. 6 packages fail. No pre-flight check to catch it. This is exactly the class of problem rin is built to eliminate.

---

## How rin compares

| | rin | BiocManager | pak | renv |
|---|---|---|---|---|
| Standalone binary | ✅ | ❌ needs R | ❌ needs R | ❌ needs R |
| Bioconductor first-class | ✅ | ✅ | ⚠️ via prefix | ❌ |
| Constraint validation across transitive deps | ✅ | ❌ | ✅ | ✅ |
| CRAN Archive fallback on version conflict | ✅ | ❌ | ❌ | ❌ |
| Pre-flight sysreq audit | ✅ | ❌ | ❌ | ❌ |
| Cross-platform audit (deb/rpm/brew/pkg-config) | ✅ | ❌ | ❌ | ❌ |
| Parallel compilation | ✅ (tiers + rayon) | ⚠️ via `Ncpus` | ✅ | ❌ |
| Lockfile (first-class workflow) | ✅ TOML | ❌ | ⚠️ peripheral (`pak::lockfile_create()`) | ✅ R-specific |
| Virtual environments | ✅ | ❌ | ❌ | ✅ |
| Binary cache (today) | ❌ (Phase 2) | ❌ | ✅ via PPM | ❌ |

---

## Installation

### One-line install (recommended)

```bash
curl -sSf https://raw.githubusercontent.com/Ajinkya2910/rin/main/install.sh | sh
```

This downloads a statically-linked binary to `~/.rin/bin/rin`. Add it to your PATH:

```bash
export PATH="$HOME/.rin/bin:$PATH"   # one-time, current shell
echo 'export PATH="$HOME/.rin/bin:$PATH"' >> ~/.bashrc   # persistent
```

### Supported platforms

| Target | Status |
|---|---|
| Linux x86_64 (musl, works on any distro) | ✅ |
| macOS arm64 (Apple Silicon) | ✅ |
| macOS x86_64 (Intel) | ✅ |
| Linux aarch64 | ⏳ planned |
| Windows | ⏳ planned |

### Build from source

```bash
git clone https://github.com/Ajinkya2910/rin
cd rin
cargo build --release
```

Requires Rust 1.70+.

---

## Commands

```bash
rin resolve <pkg>...        # show the full dependency tree without installing
rin audit <pkg>...          # check system deps against the resolved tree
rin install <pkg>...        # resolve, audit, download, compile, install
rin lock <pkg>...           # write rin.lock with pinned versions
rin restore                 # reproduce environment from rin.lock
rin why <pkg>               # trace why a package is in the tree
rin venv [path]             # create a project-isolated R library (default: .rin)
rin venv-info               # show the active venv
rin venv-remove [path]      # delete a virtual environment (default: .rin)
```

**`rin install` flags:**

```bash
--retry                     # resume a previously failed install
--ignore-missing <LIB>      # ignore one missing system lib (repeatable)
--skip-sysreq               # skip the sysreq pre-flight entirely
--strict-sysreq             # hard-fail when sysreqs look missing (CI gate)
```

---

## Honest limitations

Because credibility matters more than marketing:

- **No binary cache today.** rin compiles everything from source. Phase 2 (planned) adds Bioconductor + CRAN binary infrastructure.
- **Linux and macOS only.** Windows support is on the roadmap but not trivial due to Rtools and different sysreq tooling.
- **Not every edge case of CRAN's PACKAGES format is handled.** The resolver works on the DESeq2 / Seurat-scale dependency trees (tested with 55- and 142-package installs); rarer metadata quirks may surface.
- **System dependency mappings are curated.** rin flags a package's `SystemRequirements:` even when it isn't in `SYSREQ_MAP`. But if a package needs a system lib it doesn't declare anywhere rin can see, rin can't pre-warn and the compiler surfaces the error instead of rin's friendly one. Adding the package to `SYSREQ_MAP` fixes it — PRs welcome.

If any of those blockers hit you, file an issue. rin is production-used at small scale today; corner cases surface with wider adoption.

---

## Roadmap

### Shipped
- Constraint-aware resolver (CRAN + Bioconductor, cross-registry)
- Cross-platform sysreq audit (Debian/Ubuntu, RHEL/Rocky/Fedora, macOS)
- Parallel tier-based compilation
- Virtual environments with activate/deactivate
- TOML lockfiles with `rin lock` / `rin restore`
- CRAN Archive fallback for older versions
- One-line installer

### Phase 2 — binary cache infrastructure
- Hetzner build farm for Linux x86_64 + aarch64 binaries
- Cloudflare R2 for storage (CDN-backed)
- Coverage priorities: Bioconductor first, popular CRAN second
- Expected: seconds-not-minutes installs for the common case

### Phase 3 — ecosystem parity
- Windows support
- Full CRAN coverage in binary cache
- Integration with HPC module systems (auto-suggest `module load`)

### Nice-to-haves
- `rin use R@4.5.0` — manage R versions directly, like rustup for R
- Multi-progress UI (per-package status, like uv)
- `rin compare` — dependency footprint comparison across packages

---

## Why Rust

Bioinformaticians don't care what rin is written in. But the language choice enables things pak and BiocManager can't do:

- **Statically-linked standalone binary.** No "install R first, then install BiocManager, then..." — rin runs on a login node with zero bootstrap.
- **True parallelism via rayon.** pak uses parallel R subprocesses, which is much heavier than rayon's work-stealing scheduler.
- **Fast resolver.** rin loads 26,000+ package metadata records and resolves DESeq2's full tree in under a second.
- **Fewer parser bugs.** Rust's type system eliminates a whole class of memory and concurrency bugs at compile time — useful when the codebase is parsing metadata from two registries, walking dependency graphs, and orchestrating dozens of parallel compilations.

---

## Credits

rin draws heavily on:

- [uv](https://github.com/astral-sh/uv) — for proving the "Rust-based package manager built for correctness" model works at scale
- [pak](https://pak.r-lib.org/) — for years of excellent work on R dependency resolution, and for the Posit Public Package Manager that sets the bar for binary caching
- [renv](https://rstudio.github.io/renv/) — for establishing project-local library conventions in R
- [r-system-requirements](https://github.com/rstudio/r-system-requirements) — the reference database for R → system library mappings

rin tries to take the best ideas from each, package them into a single standalone binary, and make Bioconductor + HPC the first-class use case instead of an afterthought.

---

## License

MIT. See [LICENSE](LICENSE).

## Contributing

Issues and PRs welcome. Especially: adding to `SYSREQ_MAP`, expanding the test matrix to more Bioconductor packages, and reporting environments where rin misbehaves.

---

*Built by [Ajinkya Patil](https://github.com/Ajinkya2910), with a focus on the tools bioinformaticians actually need, not the ones R package management has accumulated over 25 years.*
