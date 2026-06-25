#' Report the installed rin version
#'
#' Runs `rin --version` and returns the reported version string.
#'
#' @return A version string (e.g. "rin 0.2.11"), or `NA_character_` if the
#'   binary could not be run.
#' @export
rin_version <- function() {
  bin <- rin_path()
  out <- tryCatch(
    system2(bin, "--version", stdout = TRUE, stderr = TRUE),
    error = function(e) NA_character_
  )
  if (length(out) == 0) return(NA_character_)
  trimws(out[[1]])
}

#' Show rin status for the current R session
#'
#' Prints where the rin binary lives, its version, the active project
#' environment (if any), the library packages will install into, and the
#' current `.libPaths()`. Use this first when a tester reports "it can't find
#' rin" or "my packages don't load" — it surfaces PATH and library-path
#' problems directly.
#'
#' @return A list with the gathered fields, invisibly.
#' @examples
#' \dontrun{
#' rin::info()
#' }
#' @export
info <- function() {
  bin <- tryCatch(rin_path(), error = function(e) NA_character_)
  ver <- if (!is.na(bin)) rin_version() else NA_character_
  venv <- Sys.getenv("RIN_VENV", unset = "")
  lib <- .rin_active_lib()

  n_pkgs <- if (!is.null(lib) && dir.exists(lib)) {
    length(list.dirs(lib, recursive = FALSE))
  } else {
    NA_integer_
  }

  cat("rin status\n")
  cat("  binary:    ", if (is.na(bin)) "NOT FOUND (see ?rin_path)" else bin, "\n", sep = "")
  cat("  version:   ", if (is.na(ver)) "unknown" else ver, "\n", sep = "")
  cat("  active venv:", if (nzchar(venv)) venv else "(none)", "\n", sep = " ")
  cat("  install lib:", if (is.null(lib)) "R default library" else lib, "\n", sep = " ")
  if (!is.na(n_pkgs)) {
    cat("  packages in lib: ", n_pkgs, "\n", sep = "")
  }
  cat("  .libPaths():\n")
  for (p in .libPaths()) cat("    - ", p, "\n", sep = "")

  invisible(list(
    binary = bin,
    version = ver,
    venv = if (nzchar(venv)) venv else NULL,
    install_lib = lib,
    lib_paths = .libPaths()
  ))
}

#' @rdname info
#' @export
status <- function() info()
