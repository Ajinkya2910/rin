#' Restore the whole project from rin.lock
#'
#' Wraps `rin restore` — installs and repairs **every** package recorded in the
#' project's `rin.lock`, not just one. Use it to rebuild a project's full
#' environment: on a new machine, after `git clone`, or after fixing a broken
#' system dependency. On success the install library is added to `.libPaths()`
#' so the packages are usable in the current session.
#'
#' This is the project-wide counterpart to [install()]: `install()` adds/installs
#' a single package (and fails only if that package fails), whereas `restore()`
#' builds the entire locked project (and fails if anything in it fails).
#'
#' @return The integer exit status of the rin process, invisibly. `0` on success.
#' @examples
#' \dontrun{
#' rin::restore()
#' }
#' @seealso [install()]
#' @export
restore <- function() {
  status <- .rin_run("restore")
  if (identical(status, 0L)) {
    added <- .rin_refresh_libpaths()
    if (!is.null(added)) {
      message("rin: added ", added, " to the library path for this session.")
    }
  }
  invisible(status)
}
