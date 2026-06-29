# Testing the `rin` R package

Thanks for testing! This wraps the `rin` command-line package manager so it can
be driven from R / RStudio. Please try the steps below and report anything that
doesn't behave as described.

## 0. Setup

```r
# The rin BINARY must be installed first:
#   curl -sSf https://raw.githubusercontent.com/Ajinkya2910/rin/main/install.sh | sh

# install.packages("remotes")
remotes::install_github("Ajinkya2910/rin", subdir = "r-pkg")
library(rin)

rin::info()   # ALWAYS run this first — shows binary path, version, libPaths
```

If `rin::info()` reports the binary as `NOT FOUND`, set it explicitly and
re-run:

```r
options(rin.path = "/full/path/to/rin")   # e.g. ~/.rin/bin/rin
```

## 1. Packages to install (each exercises a different path)

Work down the list — stop and report at the first one that fails.

| Tier | Try | What it checks |
|------|-----|----------------|
| Pure R (fast)        | `rin::install("R6")`            | basic wrapper, no compiler |
| Pure R (fast)        | `rin::install("praise")`        | + immediate-use test below |
| Compiled CRAN        | `rin::install("jsonlite")`      | C compile streams + succeeds |
| Compiled CRAN        | `rin::install("data.table")`    | heavier C / OpenMP |
| Bigger tree          | `rin::install("ggplot2")`       | many deps in one run |
| Bigger tree          | `rin::install("dplyr")`         | C++ deps |
| System deps          | `rin::install("xml2")`          | audit fires (needs libxml2) |
| System deps          | `rin::install("curl")`          | audit fires (needs libcurl) |
| Bioconductor (small) | `rin::install("S4Vectors")`     | Bioc path |
| Bioconductor (BIG)   | `rin::install("DESeq2")`        | the 56-package cascade |

## 2. The key feature: installed packages work immediately

No restart should be needed:

```r
rin::install("praise")
library(praise)
praise()          # should print praise — if "there is no package", that's a bug
```

## 3. RStudio launched from the Dock / Start menu

Open RStudio by **clicking its icon** (not from a terminal), then:

```r
library(rin)
rin::info()       # must still find the binary (via ~/.rin/bin)
rin::install("R6")
```

If `info()` says `NOT FOUND` here, note your OS and how rin was installed.

## 4. Audit overrides

```r
rin::audit("DESeq2")                              # check only, no install
rin::install("gert", ignore_missing = "libgit2-dev")
rin::install("xml2", skip_sysreq = TRUE)
```

## 5. Environments

```r
rin::venv("testenv")        # creates ./testenv with a library + activate script
rin::resolve("DESeq2")      # prints the dependency tree, installs nothing
```

## 6. Error handling

```r
options(rin.path = "/nope/rin")
rin::install("R6")          # expect a clear "could not find rin" message
options(rin.path = NULL)    # reset
```

## 7. Where does rin install? (isolation & pollution)

rin installs into ONE library per call, chosen in this priority order:

1. An explicit venv — `RIN_VENV` is set, or a `./.rin/lib` exists in the working dir
2. R's default library (`.libPaths()[1]`) **if it is writable**
3. otherwise a folder-local `./.rin/lib` (the read-only-default fallback)

> ⚠️ **There is no automatic per-folder isolation when your default library is
> writable.** If `.libPaths()[1]` is writable (a user library, or even a
> group-writable system/framework library), rin installs THERE for every
> project — one shared library. Per-folder `./.rin/lib` only appears when the
> default is **read-only**.

**Always check which library is active first:**

```r
setwd("~/some-project")
rin::info()                 # note 'install lib' and the first .libPaths() entry
.libPaths()                 # what is [1]? is it writable?
```

### Pollution check — nothing should land in the *system* library

```r
sys_lib <- "/Library/Frameworks/R.framework/Resources/library"  # macOS; adjust for your OS
before  <- rownames(installed.packages(lib.loc = sys_lib))
rin::install("praise")
after   <- rownames(installed.packages(lib.loc = sys_lib))
setdiff(after, before)      # expect character(0) — the system library gained nothing
```

If `praise` (or anything) shows up in the **system** library, report it — rin
should not be writing project packages into the base R install.

### Guaranteed per-project isolation (explicit venv)

Don't rely on the read-only fallback — pin each project to its own venv and
verify there's no cross-leak:

```r
# Project A
dir.create("~/rin-proj-a/.rin/lib", recursive = TRUE)
Sys.setenv(RIN_VENV = path.expand("~/rin-proj-a/.rin"))
rin::info()                          # 'install lib' MUST be ~/rin-proj-a/.rin/lib
rin::install("praise")
list.files("~/rin-proj-a/.rin/lib")  # expect: praise

# Project B
dir.create("~/rin-proj-b/.rin/lib", recursive = TRUE)
Sys.setenv(RIN_VENV = path.expand("~/rin-proj-b/.rin"))
rin::info()                          # 'install lib' MUST be ~/rin-proj-b/.rin/lib
rin::install("R6")
list.files("~/rin-proj-b/.rin/lib")  # expect: R6
list.files("~/rin-proj-a/.rin/lib")  # expect: STILL only praise — no leak from B

Sys.unsetenv("RIN_VENV")
```

**Pass criteria:**
- With `RIN_VENV` set, `rin::info()`'s 'install lib' matches that venv's `lib`.
- Each venv's lib contains only its own packages (no cross-folder leakage).
- The **system** library gains nothing in any scenario.

> Heads-up: if `rin::info()` reports the **same** 'install lib' across different
> project folders, you have **one shared library**, not isolation. That happens
> whenever `.libPaths()[1]` is writable. Report it if you expected per-folder
> isolation.

## 8. Scoped install — one broken/unrelated package shouldn't block others

`rin install X` should install only X + its dependencies, and re-installing
something already present should do no work.

```r
setwd("~/rin-proj-a")
rin::install("R6")          # already installed from section 7
                            # expect: "rin.lock unchanged" + "Already up to date — nothing to install"

rin::install("glue")        # adds ONLY glue (+ its deps), not a full re-resolve
                            # expect: "Resolved N package(s)" where N is small (glue's closure),
                            #         NOT the whole project
```

**Pass criteria:**
- Re-installing an already-present package prints **"rin.lock unchanged"** and
  **"Already up to date — nothing to install"** (it must NOT claim it installed
  anything, and must NOT rebuild).
- Installing a new package reports a count scoped to *that* package's
  dependencies, not your entire `rin.lock`.
- If any package in `rin.lock` is broken, `rin::install(<something-else>)` should
  still succeed for the something-else (and may show an `ℹ … rin restore` hint).

## 9. restore — rebuild the whole project

```r
setwd("~/rin-proj-a")
rin::restore()              # installs/repairs EVERYTHING in this folder's rin.lock
```

**Pass criteria:**
- Restores from the folder's `rin.lock`, builds anything missing, and finishes
  with "Environment restored from rin.lock (N packages)".

## 10. Built-package cache — compile once, link across projects

rin compiles each package once, stores it in a shared cache, and **symlinks** it
into every project — so a second project that needs the same package doesn't
recompile. Use a **compiled** package (jsonlite) so the speedup is obvious.

```r
# Project A — builds jsonlite (compiles), populates the cache
dir.create("~/cache-a/.rin/lib", recursive = TRUE)
Sys.setenv(RIN_VENV = path.expand("~/cache-a/.rin"))
system.time(rin::install("jsonlite"))     # compiles → takes a few seconds

# Project B — same package, should LINK from cache (no compile)
dir.create("~/cache-b/.rin/lib", recursive = TRUE)
Sys.setenv(RIN_VENV = path.expand("~/cache-b/.rin"))
system.time(rin::install("jsonlite"))     # expect: "✓ jsonlite (cached)" + near-instant

Sys.unsetenv("RIN_VENV")
```

Check the cache directly:
```r
# a non-empty result means it's a SYMLINK; it should point into the cache
Sys.readlink(path.expand("~/cache-b/.rin/lib/jsonlite"))   # -> ~/.cache/rin/built/...
```
And in a **terminal**: `rin cache dir` prints the cache path; the package lives under it.

**Pass criteria:**
- Project B prints **`(cached)`** and **`(1 from cache)`**, and is dramatically
  faster than A (no compile).
- Project B's library entry is a **symlink** into the cache.
- Both projects can `library(jsonlite)`.

## 11. GitHub packages

rin installs straight from GitHub using the same spec syntax as pak/remotes.
(Note: GitHub packages are **not** cached yet — that's a known follow-up.)

```r
rin::install("gaborcsardi/praise")     # bare owner/repo (or any small GitHub R pkg)
library(praise); praise()              # should load and work

# pinning variants (try one):
# rin::install("owner/repo@v1.2.0")    # a tag
# rin::install("owner/repo@main")      # a branch
# rin::install("owner/repo@a1b2c3d")   # an exact commit
```

**Pass criteria:**
- rin prints `fetched gh:owner/repo as <name> <version>`, installs, and the
  package loads.
- A pinned tag/commit installs that exact ref.
- (If you hit `HTTP 401 / Bad credentials`, your `GITHUB_TOKEN` is stale —
  `Sys.getenv("GITHUB_TOKEN")` — fix or unset it; report if it persists.)

## 12. Terminal + venv — cache works outside RStudio too

Confirm the cache isn't RStudio-specific. Run in the **Terminal tab** (or any shell):

```bash
mkdir ~/term-a && cd ~/term-a
rin venv .rin && source .rin/activate
rin install jsonlite          # builds (or cached if section 10 already ran)
deactivate

mkdir ~/term-b && cd ~/term-b
rin venv .rin && source .rin/activate
rin install jsonlite          # expect: "✓ jsonlite (cached)"
deactivate
```

**Pass criteria:**
- The second install shows `(cached)` in the terminal, same as in RStudio.
- `rin cache dir` resolves to the same cache used by RStudio.

## 13. End-to-end — a full analysis script

A single script that goes from nothing → installed → a real result. Save as
`analysis.R`, set the working directory to a fresh folder, and run it.

```r
library(rin)

# 1. install everything this analysis needs (compiled + pure-R)
rin::install(c("jsonlite", "glue", "data.table"))

library(jsonlite); library(glue); library(data.table)

# 2. do a tiny "analysis"
raw  <- fromJSON('{"gene": ["BRCA1","TP53","EGFR"], "count": [120, 88, 45]}')
dt   <- as.data.table(raw)
dt[, rank := frank(-count)]
top  <- dt[rank == 1, gene]

# 3. produce a result file
writeLines(glue("Top gene: {top} ({dt[rank==1, count]} counts)"), "result.txt")
cat(readLines("result.txt"), sep = "\n")
```

**Pass criteria:**
- Runs start-to-finish with no manual package installs.
- `result.txt` is written and reads `Top gene: BRCA1 (120 counts)`.
- A second run in a *new* folder installs the same packages **from cache**
  (look for `(cached)`), proving the whole pipeline benefits from the cache.

## 14. Version-safety (the reproducibility promise) — *optional, high value*

Two projects pinned to **different versions** of the same package must each keep
their own — the exact case a shared library would silently break.

```r
# Project OLD — an older version
dir.create("~/ver-old/.rin/lib", recursive = TRUE)
Sys.setenv(RIN_VENV = path.expand("~/ver-old/.rin"))
rin::install("jsonlite")   # then, to force an older pin, edit ~/ver-old/rin.lock
                           # to an earlier jsonlite version and: rin::restore()

# Project NEW — latest
dir.create("~/ver-new/.rin/lib", recursive = TRUE)
Sys.setenv(RIN_VENV = path.expand("~/ver-new/.rin"))
rin::install("jsonlite")
Sys.unsetenv("RIN_VENV")

# Each project must report ITS pinned version:
packageVersion("jsonlite", lib.loc = "~/ver-old/.rin/lib")
packageVersion("jsonlite", lib.loc = "~/ver-new/.rin/lib")
```

**Pass criteria:** the two libraries hold **different** versions, both intact —
installing NEW never changed OLD.

## 15. Reproducibility via rin.lock — *optional, high value*

The headline workflow: commit `rin.lock`, recreate the exact environment elsewhere.

```r
# In project A, after installing your packages:
file.exists("rin.lock")                 # TRUE — commit this to git

# Simulate a fresh machine / clone: copy ONLY rin.lock into an empty folder
dir.create("~/repro")
file.copy("~/project-a/rin.lock", "~/repro/rin.lock")
setwd("~/repro")
rin::restore()                          # rebuilds the exact same packages/versions
```

**Pass criteria:** `~/repro` ends up with the same packages and versions as
project A, built (or linked from cache) purely from `rin.lock`.

## 16. DESeq2 in two projects — the cache payoff

The headline demo: a heavy Bioconductor cascade built once, then linked. Use a
clock so the difference is undeniable.

```r
# Project 1 — full build (downloads + compiles the whole tree, minutes)
dir.create("~/deseq-1/.rin/lib", recursive = TRUE)
Sys.setenv(RIN_VENV = path.expand("~/deseq-1/.rin"))
system.time(rin::install("DESeq2"))    # ~minutes the first time

# Project 2 — same versions, should be almost entirely cache links
dir.create("~/deseq-2/.rin/lib", recursive = TRUE)
Sys.setenv(RIN_VENV = path.expand("~/deseq-2/.rin"))
system.time(rin::install("DESeq2"))    # expect: seconds, most lines "(cached)"
Sys.unsetenv("RIN_VENV")
```

**Pass criteria:**
- Project 2 finishes in **seconds, not minutes**.
- The summary shows a large **`(N from cache)`** and most per-package lines say
  `(cached)`.
- Both projects `library(DESeq2)` successfully; the **system library is untouched**.

> Needs the R-selection fix (≥ v0.2.16): DESeq2's `locfit` requires the R you're
> actually running. If you hit `CC17 not defined`, you're on an older binary or a
> conda R — see §0 and `rin::info()`.

## 17. Dangling-cache recovery

Deleting the cache leaves the project's symlinks dangling. rin must rebuild
cleanly on the next install, not error out.

```r
# Pick a project whose packages are linked from cache (e.g. ~/deseq-2 or ~/cache-b)
proj <- "~/cache-b"

# 1. confirm a package there is a live symlink into the cache
Sys.readlink(path.expand(file.path(proj, ".rin/lib/jsonlite")))   # -> ~/.cache/rin/built/...

# 2. nuke the cache (in a terminal: `rin cache dir` shows the path first)
unlink(path.expand("~/.cache/rin"), recursive = TRUE)

# 3. the link is now dangling
file.exists(path.expand(file.path(proj, ".rin/lib/jsonlite")))    # FALSE

# 4. reinstall — rin should rebuild + re-link, no error
setwd(path.expand(proj))
rin::install("jsonlite")
library(jsonlite)                                                  # loads again
```

**Pass criteria:**
- Step 4 rebuilds and re-links without error (the dangling symlink is replaced,
  not left broken or causing a failure).
- `library(jsonlite)` works afterward.
- (If you used a custom `RIN_CACHE_DIR`, delete that path instead of `~/.cache/rin`.)

## Other test ideas (tell me which to add)

- **`--skip-sysreq` / `--strict-sysreq` behavior** under a deliberately missing lib.
- **HPC cache test** (cache in `$HOME`, project in `scratch`) — coming as its own
  section.

## What to report

For any failure, please include:
- the exact call you made
- the full console output
- `rin::info()` output
- your OS + chip (e.g. macOS Apple Silicon, Ubuntu 22.04) and R version
