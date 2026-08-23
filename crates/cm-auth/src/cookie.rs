//! Cookie construction and parsing.
//!
//! Both cookies carry the `__Host-` prefix, which a browser only accepts when
//! the cookie is `Secure`, has `Path=/`, and has **no** `Domain` attribute.
//! That combination is the point: a host-only cookie cannot be set by a
//! sibling subdomain, which is the attack a plain double-submit CSRF scheme
//! would otherwise be open to.
//!
//! It also has a deployment consequence worth stating plainly: because the
//! cookie is host-only, the API must be served from the same host as the front
//! end. An API on `api.example.com` could not read a cookie set for
//! `app.example.com`.

use std::time::Duration;

/// The session token. `HttpOnly`: script has no reason to read it, and every
/// reason not to be able to.
pub const SESSION_COOKIE: &str = "__Host-cm_session";
/// The CSRF token. Deliberately *not* `HttpOnly`: the front end has to read it
/// to echo it back in a header.
pub const CSRF_COOKIE: &str = "__Host-cm_csrf";

/// `Lax`, not `Strict`: `Strict` drops the cookie on ordinary inbound links,
/// so arriving from an email lands the user logged out. `Lax` still withholds
/// it from cross-site POSTs, and the CSRF token covers what remains.
const SAME_SITE: &str = "Lax";

/// Build a `Set-Cookie` value.
fn build(name: &str, value: &str, max_age: Duration, http_only: bool) -> String {
    let mut cookie = format!(
        "{name}={value}; Path=/; Secure; SameSite={SAME_SITE}; Max-Age={}",
        max_age.as_secs()
    );
    if http_only {
        cookie.push_str("; HttpOnly");
    }
    cookie
}

pub fn session(token: &str, max_age: Duration) -> String {
    build(SESSION_COOKIE, token, max_age, true)
}

pub fn csrf(token: &str, max_age: Duration) -> String {
    build(CSRF_COOKIE, token, max_age, false)
}

/// Expire a cookie. The value is emptied as well as the age zeroed, so a client
/// that ignores `Max-Age` still holds nothing useful.
fn clear(name: &str, http_only: bool) -> String {
    build(name, "", Duration::from_secs(0), http_only)
}

pub fn clear_session() -> String {
    clear(SESSION_COOKIE, true)
}

pub fn clear_csrf() -> String {
    clear(CSRF_COOKIE, false)
}

/// Pull one cookie out of a `Cookie` header.
///
/// Hand-parsed rather than pulled in as a dependency: the header is a
/// well-defined `name=value` list, and the parsing surface here is smaller than
/// the surface of taking on a crate for it.
pub fn read<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then_some(value.trim())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_cookie_meets_every_host_prefix_requirement() {
        let cookie = session("a-token", Duration::from_secs(3600));

        assert!(cookie.starts_with("__Host-cm_session=a-token"));
        assert!(cookie.contains("; Secure"), "{cookie}");
        assert!(cookie.contains("; Path=/"), "{cookie}");
        assert!(cookie.contains("; HttpOnly"), "{cookie}");
        assert!(cookie.contains("; SameSite=Lax"), "{cookie}");
        assert!(cookie.contains("; Max-Age=3600"), "{cookie}");
        // A Domain attribute makes a browser reject a __Host- cookie outright.
        assert!(
            !cookie.to_lowercase().contains("domain="),
            "__Host- cookies must not set Domain: {cookie}"
        );
    }

    #[test]
    fn the_csrf_cookie_is_readable_by_script_but_otherwise_identical() {
        let cookie = csrf("a-token", Duration::from_secs(3600));

        assert!(cookie.contains("; Secure"), "{cookie}");
        assert!(cookie.contains("; Path=/"), "{cookie}");
        assert!(
            !cookie.contains("HttpOnly"),
            "the front end has to read this one: {cookie}"
        );
    }

    #[test]
    fn clearing_empties_the_value_as_well_as_the_age() {
        let cookie = clear_session();
        assert!(cookie.starts_with("__Host-cm_session=;"), "{cookie}");
        assert!(cookie.contains("Max-Age=0"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
    }

    #[test]
    fn cookies_are_read_out_of_a_realistic_header() {
        let header = "other=1; __Host-cm_session=abc123; __Host-cm_csrf=xyz789";

        assert_eq!(read(header, SESSION_COOKIE), Some("abc123"));
        assert_eq!(read(header, CSRF_COOKIE), Some("xyz789"));
        assert_eq!(read(header, "missing"), None);
    }

    #[test]
    fn reading_tolerates_spacing_and_a_single_pair() {
        assert_eq!(
            read("  __Host-cm_session = abc  ", SESSION_COOKIE),
            Some("abc")
        );
        assert_eq!(read("__Host-cm_session=abc", SESSION_COOKIE), Some("abc"));
        assert_eq!(read("", SESSION_COOKIE), None);
        assert_eq!(read("novalue", SESSION_COOKIE), None);
    }

    /// A cookie whose name merely ends with ours must not be mistaken for it.
    #[test]
    fn a_similarly_named_cookie_is_not_confused_with_ours() {
        assert_eq!(read("evil__Host-cm_session=bad", SESSION_COOKIE), None);
    }
}
