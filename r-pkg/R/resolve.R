#' Show the dependency tree with rin
#'
#' Wraps `rin resolve` — prints the full resolved dependency tree for the
#' given packages without installing anything.
#'
#' @param packages Character vector of package names to resolve (required).
#' @return The integer exit status of the rin process, invisibly.
#' @examples
#' \dontrun{
#' rin::resolve(c("DESeq2", "ggplot2"))
#' }
#' @export
resolve <- function(packages) {
  if (missing(packages) || length(packages) == 0) {
    stop("Provide at least one package name to resolve.", call. = FALSE)
  }
  args <- "resolve"
  quote <- FALSE
  for (p in packages) {
    args <- c(args, p)
    quote <- c(quote, TRUE)
  }
  .rin_run(args, quote)
}
