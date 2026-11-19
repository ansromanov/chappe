//! Where the issues of the forge can be read in a browser.
//!
//! The service does not know which forge its owner runs, so there is no built-in
//! address: `CHAPPE_FORGE_URL` (for example `https://git.example.com`) turns the
//! links on, and without it issues are shown as plain text.

use std::fmt::Display;
use std::sync::OnceLock;

static BASE: OnceLock<String> = OnceLock::new();

/// Sets the base address once, at start. Later calls are ignored.
pub fn set_forge_url(url: &str) {
    let url = url.trim().trim_end_matches('/');
    if !url.is_empty() {
        let _ = BASE.set(url.to_string());
    }
}

#[cfg(not(test))]
fn base() -> Option<&'static str> {
    BASE.get().map(String::as_str)
}

#[cfg(test)]
fn base() -> Option<&'static str> {
    Some("https://forge.example.com")
}

/// The page of an issue (`owner/repo`, number), or `None` when no forge address is set.
pub fn issue_url(repo: &str, issue: impl Display) -> Option<String> {
    base().map(|b| format!("{b}/{repo}/issues/{issue}"))
}
