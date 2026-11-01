//! Upstream checks its GitHub releases at service start and appends an install
//! notice to the first tool result. That notice would replace the fork's build
//! with upstream's, so the fork's builds never contact upstream and never advise
//! installing upstream's build. The service keeps calling these
//! functions, and `--no-update-check` stays accepted, so configurations that
//! pass it keep working.

/// The update notice for the service's responses: always empty in the fork.
pub fn get_update_notice() -> &'static str {
    ""
}

/// Starts no check: the fork makes no update request.
pub fn spawn_update_check() {}
