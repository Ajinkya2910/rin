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

## What to report

For any failure, please include:
- the exact call you made
- the full console output
- `rin::info()` output
- your OS + chip (e.g. macOS Apple Silicon, Ubuntu 22.04) and R version
