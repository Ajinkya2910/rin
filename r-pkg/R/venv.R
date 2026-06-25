#' Create or manage a project-local R environment with rin
#'
#' Wraps `rin venv`. Creates a project-local library plus an `activate`
#' script. Note that activation itself is a shell concept (it exports
#' `R_LIBS_USER`); a GUI RStudio launched from the Dock will not pick that up
#' automatically. To use a venv's packages inside the current R session, point
#' R at its library directly, e.g. `.libPaths("<path>/lib")`.
#'
#' @param path Name or path for the environment (default `.rin`).
#' @param r_version Optional R version string (maps to `--r-version`).
#' @return The integer exit status of the rin process, invisibly.
#' @examples
#' \dontrun{
#' rin::venv("myproject")
#' rin::venv("myproject", r_version = "4.4.0")
#' }
#' @export
venv <- function(path = ".rin", r_version = NULL) {
  args <- c("venv", path)
  quote <- c(FALSE, TRUE)
  if (!is.null(r_version)) {
    args <- c(args, "--r-version", r_version)
    quote <- c(quote, FALSE, TRUE)
  }
  .rin_run(args, quote)
}

#' Remove a project-local R environment
#'
#' Wraps `rin venv-remove`.
#'
#' @param path Path to the environment to remove (default `.rin`).
#' @return The integer exit status of the rin process, invisibly.
#' @examples
#' \dontrun{
#' rin::venv_remove("myproject")
#' }
#' @export
venv_remove <- function(path = ".rin") {
  .rin_run(c("venv-remove", path), quote = c(FALSE, TRUE))
}
