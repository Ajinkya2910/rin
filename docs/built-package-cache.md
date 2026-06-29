# Design: built-package cache

Status: **proposal** (for review before implementation)

## Goal

Make per-project isolation cheap. Today (v0.3.0) every project installs into its
own `.rin/lib`, which means the same package is **recompiled for every project**.
A shared, content-addressed cache of *built* packages lets each project **link**
to one stored copy instead — isolation without the duplication.

> DESeq2's tree is ~6 min to build. Second project that needs it today: 6 min
> again. With the cache: seconds (link). Same for disk.

## What already exists vs. what's new

| | Caches | Where | Saves |
|---|---|---|---|
| **Source tarball cache** (exists) | downloaded `*.tar.gz` | `/tmp/rin-downloads` (CRAN), `~/.rin/cache/github` (GitHub) | re-**download** |
| **Built-package cache** (this design) | the compiled, installed package dir | `~/.cache/rin/built/...` | re-**compile** ← the big win |

These are complementary. This design adds the second one.

## The cache key (the correctness-critical part)

A built package is only safe to reuse for an *identical* build target. Key on:

```
<platform> / <R-major.minor> / <name> / <version>
```

- **platform** — R's own platform string (`R.version$platform`,
  e.g. `aarch64-apple-darwin20`), queried once per run. Not the Rust target.
- **R-major.minor** — e.g. `4.4`. Built packages are tied to the R minor series.
- **name / version** — for CRAN/Bioc, the resolved version. For **GitHub**, the
  commit SHA *is* the version (`<name>/<sha>`), since a tag/branch can move.

Conservative by construction: anything that changes the built artifact is in the
key. When in doubt, add to the key (a too-specific key only costs a rebuild; a
too-loose key serves a *wrong* build).

Source (`cran`/`bioc`) is **not** in the key — `name+version+platform+Rver`
already identifies the artifact uniquely.

## On-disk layout

```
~/.cache/rin/built/
  aarch64-apple-darwin20/
    4.4/
      R6/2.5.1/R6/              <- the actual installed package directory
      DESeq2/1.46.0/DESeq2/
```

The innermost `<name>/` is exactly what an R library contains. "Linking into a
project" = make `<project>/.rin/lib/R6` resolve to `.../R6/2.5.1/R6`.

Cache location resolution (first hit wins):
1. `RIN_CACHE_DIR` env var (escape hatch / HPC tuning / shared lab cache later)
2. `$XDG_CACHE_HOME/rin/built`
3. `~/.cache/rin/built`

## Install → cache → link flow

Hook point: [`install_single_package`](../src/installer.rs#L304). Today it ends with
`run_r_cmd_install(&tarball, name)` which installs straight into the project lib.
New flow:

```
key  = cache_key(pkg, platform, r_version)
slot = <cache>/built/<key>          # .../R6/2.5.1/R6

1. if slot exists:                  # CACHE HIT
       link slot -> <project-lib>/<name>
       return                       # no download, no compile

2. CACHE MISS:
       (download tarball as today — reuses the source cache)
       staging = <cache>/built/<key-parent>/.<version>.tmp.<pid>
       R CMD INSTALL --library=staging <tarball>
       atomic-rename staging -> slot          # publish only on success
       link slot -> <project-lib>/<name>
```

Only successful builds are published (the temp dir is renamed in only after
`R CMD INSTALL` succeeds), so a failed compile never poisons the cache.

`check_installed_versions` needs **no change**: once a package is linked into the
project lib, R sees it as installed there.

## Link strategy: symlink first, copy fallback

v1 links the **package directory** into the project lib:

1. **symlink** `<project-lib>/<name>` → `<cache>/.../<name>` (default)
2. **copy** the directory (fallback when symlinks are unavailable/disallowed)

Why symlink as the default (this is also what **renv** does):
- One operation per package, no tree walking.
- **Works across filesystems** — so on HPC, cache in `$HOME` + project in
  `scratch` still links (a symlink points by path, no same-mount requirement).
  This is the opposite of hardlinks, which fail cross-mount.

Trade-off (accepted, documented): the cache becomes **load-bearing** — deleting
it leaves dangling symlinks. Mitigations:
- `rin install` / `rin restore` detect a dangling/missing link and rebuild.
- Document "don't hand-delete the cache; use `rin cache clean`" (future).

Future optimization (not v1): on APFS/btrfs/xfs use `clonefile`/reflink for
*independent* cheap copies (via the `reflink-copy` crate), removing the
load-bearing property where the filesystem supports it.

## Concurrency

rin builds in parallel (rayon), and two `rin` processes (different projects) can
build the same package at once. Safety comes from **atomic publish**:
- each build writes to a unique `.<version>.tmp.<pid>` dir
- `rename` into the final slot is atomic on a single filesystem
- if the slot already exists at rename time (another process won), discard the
  temp and just link the existing slot

No global lock needed; worst case is a redundant build, never a corrupt slot.

## Correctness caveats (same class renv accepts)

- **Runtime system libraries.** A built package may dynamically link a system
  lib (e.g. libxml2). The cache assumes the machine's system libs are stable.
  Fine for a per-user cache on one machine; riskier for a *shared/cross-machine*
  cache (future — out of scope for v1).
- **Cross-machine sharing** is explicitly out of scope for v1 (per-user only).

## Diff surface

- **new** `src/cache.rs`: `cache_key`, `cache_slot_path`, `link_into_lib`
  (symlink+copy), `publish_atomically`, cache-dir resolution.
- **modify** `install_single_package`: cache-hit short-circuit; on miss, install
  into staging + publish + link.
- **modify** `run_r_cmd_install`: accept an explicit target-lib argument (today
  it implicitly uses `get_venv_lib()`), so it can install into the cache staging.
- **modify** install summary: distinguish "linked from cache" from "built" in
  the per-package line (e.g. `✓ R6 (cached)`).
- one R query for `R.version$platform` (cache it on `Registry`).

Estimated: a focused v1 in the low-hundreds of lines, concentrated in `cache.rs`.

## HPC behavior

- Works from day one (symlink across mounts, or copy fallback).
- For the *speedup*, the cache should ideally sit where projects can reach it
  cheaply; `RIN_CACHE_DIR` lets a site point it at fast/quota-appropriate
  storage. Tuning, not a blocker.

## Test plan

1. **Unit**: key stability (same inputs → same key; R-version/platform change →
   different key); atomic publish under simulated concurrent builds.
2. **Local (Mac/Linux)**: install pkg in project A (builds, populates cache);
   install same pkg+version in project B → **no compile**, linked; both load;
   deleting the cache + `rin install` rebuilds.
3. **Isolation still holds**: A pinned to vX, B to vY → two cache slots, both
   projects load their own version (the scenario shared libs break).
4. **No system pollution**: system library count unchanged throughout.
5. **HPC (early!)**: cache in `$HOME`, project in `scratch` (different mounts) —
   confirm symlink works; confirm copy fallback triggers where it doesn't;
   confirm R loads a linked package on Lustre/GPFS/NFS.

## Open decisions

1. **Link default**: symlink (renv-style, HPC-friendly, load-bearing) vs.
   clonefile/copy (independent, but local-only benefit). Proposal: **symlink + copy fallback** for v1.
2. **Cache location default**: `~/.cache/rin/built` + `RIN_CACHE_DIR` override. Agreed?
3. **Cache management commands** (`rin cache dir|clean|verify`) — v1 or later?
