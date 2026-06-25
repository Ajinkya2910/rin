#' Audit system dependencies with rin
#'
#' Wraps `rin audit` — runs the pre-flight system-dependency check without
#' installing anything. With no arguments it audits the roots of the current
#' `rin.lock` (useful inside an activated venv).
#'
#' @param packages Optional character vector of package names to audit.
#' @return The integer exit status of the rin process, invisibly.
#' @examples
#' \dontrun{
#' rin::audit("DESeq2")
#' rin::audit()        # audit the current project's lockfile roots
#' }
#' @export
audit <- function(packages = character()) {
  args <- "audit"
  quote <- FALSE
  for (p in packages) {
    args <- c(args, p)
    quote <- c(quote, TRUE)
  }
  .rin_run(args, quote)
}
