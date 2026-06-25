# rin (R package)

Use the [`rin`](https://github.com/Ajinkya2910/rin) package manager from inside
R and RStudio. This is a thin wrapper around the `rin` command-line binary — it
finds the binary, runs it with live output, and refreshes the library path so
freshly installed packages are usable immediately, without restarting R.

## Prerequisites

Install the `rin` binary first (one line, no Rust toolchain needed):

```bash
curl -sSf https://raw.githubusercontent.com/Ajinkya2910/rin/main/install.sh | sh
```

## Install this package

```r
# install.packages("remotes")
remotes::install_github("Ajinkya2910/rin", subdir = "r-pkg")
```

## Usage

```r
library(rin)

rin::install("DESeq2")        # install + make usable in this session
rin::install(c("ggplot2", "dplyr"))
rin::install("gert", ignore_missing = "libgit2-dev")
rin::install(retry = TRUE)    # resume a failed install

rin::audit("DESeq2")          # pre-flight system-dependency check only
rin::resolve("DESeq2")        # print the dependency tree
rin::venv("myproject")        # create a project-local environment
rin::info()                   # binary path, version, active venv, libPaths
```

## Finding the binary

The package looks for `rin` in this order:

1. `options(rin.path = "/full/path/to/rin")`
2. the `RIN_BIN` environment variable
3. your `PATH`
4. `~/.rin/bin/rin` (the default install location)

GUI RStudio (launched from the Dock/Start menu) often does **not** inherit your
shell `PATH`, so step 4 is what usually makes it "just work". If `rin::info()`
reports the binary as `NOT FOUND`, set `options(rin.path = ...)`.

## A note on venvs and RStudio

`rin::venv()` creates a project-local library and an `activate` script.
Activation exports `R_LIBS_USER`, which a terminal R session picks up — but a
GUI RStudio launched from the Dock does not. To use a venv's packages in the
current session, point R at it directly:

```r
.libPaths("/path/to/myproject/lib")
```

(Or open the folder as an RStudio Project so a project `.Rprofile` can do this
for you.)
