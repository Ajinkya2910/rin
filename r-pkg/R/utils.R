# Internal helpers: locating the binary, running it with live output, and
# keeping the R session's library path in sync with what rin installs into.

# Candidate locations for the rin binary, in priority order.
#
# GUI RStudio launched from the Dock/Start menu usually does NOT inherit the
# shell PATH that the install script extends ("~/.rin/bin"), so Sys.which()
# alone is unreliable there. We therefore also probe the known install
# location and an explicit override.
.rin_bin_candidates <- function() {
  exe <- if (.Platform$OS.type == "windows") "rin.exe" else "rin"
  c(
    getOption("rin.path", default = ""),          # explicit override
    Sys.getenv("RIN_BIN", unset = ""),            # env override
    unname(Sys.which("rin")),                     # PATH
    file.path(Sys.getenv("HOME"), ".rin", "bin", exe),
    path.expand(file.path("~", ".rin", "bin", exe))
  )
}

#' Locate the rin binary
#'
#' Returns the absolute path to the `rin` executable, searching (in order)
#' `options(rin.path=)`, the `RIN_BIN` environment variable, the system
#' `PATH`, and the default install location `~/.rin/bin`.
#'
#' @return A single path string. Errors if the binary cannot be found.
#' @export
rin_path <- function() {
  for (cand in .rin_bin_candidates()) {
    if (nzchar(cand) && file.exists(cand)) {
      return(normalizePath(cand, mustWork = TRUE))
    }
  }
  stop(
    "Could not find the 'rin' binary.\n",
    "  Looked on PATH and in ~/.rin/bin.\n",
    "  Install it with:\n",
    "    curl -sSf https://raw.githubusercontent.com/Ajinkya2910/rin/main/install.sh | sh\n",
    "  Or point this package at an existing binary:\n",
    "    options(rin.path = \"/full/path/to/rin\")",
    call. = FALSE
  )
}

# Run the binary with a vector of arguments, streaming output live to the R
# console (stdout = "" / stderr = "" do not buffer — important for multi-minute
# compiles that would otherwise look frozen in RStudio).
#
# Dynamic values (package names, paths) are quoted; literal flags are passed
# through untouched. Returns the integer exit status invisibly.
.rin_run <- function(args, quote = rep(FALSE, length(args))) {
  bin <- rin_path()
  args <- ifelse(quote, shQuote(args), args)
  status <- system2(bin, args = args, stdout = "", stderr = "")
  invisible(status)
}

# Mirror of the Rust-side get_venv_lib(): where will rin install into right now?
#   1. An active venv (RIN_VENV set) -> <venv>/lib
#   2. R_LIBS_USER (what the activate script exports) if it exists
#   3. A project-local ./.rin/lib in the working directory
#   4. NULL -> rin will use R's default library (already on .libPaths())
.rin_active_lib <- function() {
  venv <- Sys.getenv("RIN_VENV", unset = "")
  if (nzchar(venv)) {
    lib <- file.path(venv, "lib")
    if (dir.exists(lib)) return(normalizePath(lib))
  }
  libuser <- Sys.getenv("R_LIBS_USER", unset = "")
  if (nzchar(libuser) && dir.exists(libuser)) return(normalizePath(libuser))
  local <- file.path(getwd(), ".rin", "lib")
  if (dir.exists(local)) return(normalizePath(local))
  NULL
}

# After an install, make sure the target library is on .libPaths() so the
# freshly installed packages are usable in THIS session without a restart.
# Returns the path that was added (or NULL if nothing changed).
.rin_refresh_libpaths <- function() {
  lib <- .rin_active_lib()
  if (is.null(lib)) return(invisible(NULL))
  current <- normalizePath(.libPaths(), mustWork = FALSE)
  if (!(lib %in% current)) {
    .libPaths(c(lib, .libPaths()))
    return(invisible(lib))
  }
  invisible(NULL)
}
