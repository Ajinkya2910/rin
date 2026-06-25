#' Install R packages with rin
#'
#' Wraps `rin install`. Runs rin's pre-flight system-dependency audit and
#' parallel install, streaming output to the console. On success the install
#' target library is added to `.libPaths()` so the packages are immediately
#' usable in the current session.
#'
#' @param packages Character vector of package names (CRAN or Bioconductor).
#'   May be omitted when `retry = TRUE`.
#' @param retry Resume a previously failed install instead of starting fresh.
#' @param skip_sysreq Skip the system-requirements pre-flight entirely.
#' @param ignore_missing Character vector of system libraries to ignore in the
#'   audit (maps to repeated `--ignore-missing`).
#' @param strict_sysreq Block the install when sysreqs appear missing
#'   (default is advisory).
#'
#' @return The integer exit status of the rin process, invisibly. `0` on
#'   success.
#' @examples
#' \dontrun{
#' rin::install("DESeq2")
#' rin::install(c("ggplot2", "dplyr"))
#' rin::install("gert", ignore_missing = "libgit2-dev")
#' rin::install(retry = TRUE)
#' }
#' @export
install <- function(packages = character(),
                    retry = FALSE,
                    skip_sysreq = FALSE,
                    ignore_missing = character(),
                    strict_sysreq = FALSE) {
  if (length(packages) == 0 && !retry) {
    stop("Provide package name(s), or call with retry = TRUE.", call. = FALSE)
  }

  args <- "install"
  quote <- FALSE
  add <- function(a, q) {
    args <<- c(args, a)
    quote <<- c(quote, q)
  }

  for (p in packages) add(p, TRUE)
  if (retry) add("--retry", FALSE)
  if (skip_sysreq) add("--skip-sysreq", FALSE)
  if (strict_sysreq) add("--strict-sysreq", FALSE)
  for (lib in ignore_missing) {
    add("--ignore-missing", FALSE)
    add(lib, TRUE)
  }

  status <- .rin_run(args, quote)

  if (identical(status, 0L)) {
    added <- .rin_refresh_libpaths()
    if (!is.null(added)) {
      message("rin: added ", added, " to the library path for this session.")
    }
  }
  invisible(status)
}
