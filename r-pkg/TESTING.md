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

## What to report

For any failure, please include:
- the exact call you made
- the full console output
- `rin::info()` output
- your OS + chip (e.g. macOS Apple Silicon, Ubuntu 22.04) and R version
