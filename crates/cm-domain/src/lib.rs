//! Business rules.
//!
//! Everything here is testable without HTTP: handlers translate requests into
//! these calls and back. Nothing in this crate writes SQL — that lives in
//! `cm-db`, so "which query touches a restricted column" stays a grep.

pub mod claims;
pub mod contractors;
pub mod geocode_worker;
pub mod geocoder;
pub mod import;
pub mod job_alerts;
pub mod jobs;
pub mod location;
pub mod mail_worker;
pub mod mailer;
pub mod maintenance;
pub mod messaging;
pub mod quality;
pub mod search;
pub mod verification;

/// A URL- and filename-safe slug.
///
/// Deliberately conservative: the database enforces `^[a-z0-9]+(-[a-z0-9]+)*$`,
/// so anything that cannot be reduced to that shape must not reach it.
pub fn slugify(value: &str) -> String {
    let mut slug = String::with_capacity(value.len());
    let mut pending_separator = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            pending_separator = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }

    slug
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_match_the_database_constraint() {
        let pattern = regex_lite();

        for input in [
            "Ibarra & Daughters Construction",
            "  leading and trailing  ",
            "Ünïcödé Builders",
            "many---separators",
            "1047382",
        ] {
            let slug = slugify(input);
            assert!(
                pattern(&slug),
                "{input:?} produced {slug:?}, which the constraint rejects"
            );
        }
    }

    #[test]
    fn an_unslugifiable_name_produces_an_empty_slug_for_the_caller_to_handle() {
        assert_eq!(slugify("!!!"), "");
        assert_eq!(slugify(""), "");
    }

    /// The database's regex, checked without pulling in a regex crate.
    fn regex_lite() -> impl Fn(&str) -> bool {
        |slug: &str| {
            !slug.is_empty()
                && !slug.starts_with('-')
                && !slug.ends_with('-')
                && !slug.contains("--")
                && slug
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        }
    }
}
