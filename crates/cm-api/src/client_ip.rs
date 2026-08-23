//! Identifying the client for rate limiting and audit.

use http::HeaderMap;
use std::net::SocketAddr;

/// Resolve the client address.
///
/// `X-Forwarded-For` is believed only when **both** conditions hold: the
/// configuration permits it, *and* the immediate socket peer is loopback.
///
/// The second condition is the one that matters. A flag alone trusts the header
/// from whoever connected, so anything that can reach the port — a misplaced
/// firewall rule, a container network, a future change that binds a public
/// interface — can choose its own rate-limit bucket and its own audit trail by
/// inventing a header. The deployment is exactly one Caddy hop on loopback, so
/// that boundary is encoded here rather than left to the firewall to enforce on
/// this code's behalf.
///
/// When the header is believed, the **last** entry is taken, not the first. A
/// proxy appends the peer it actually saw, so with one trusted hop the last
/// entry is the only one the client could not forge.
pub fn resolve(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trust_proxy_headers: bool,
) -> Option<String> {
    let peer_is_local_proxy = peer.is_some_and(|address| address.ip().is_loopback());

    if trust_proxy_headers && peer_is_local_proxy {
        if let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
        {
            if let Some(last) = forwarded
                .split(',')
                .map(str::trim)
                .rfind(|entry| !entry.is_empty())
            {
                return Some(last.to_owned());
            }
        }
    }

    peer.map(|address| address.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(forwarded: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = forwarded {
            headers.insert("x-forwarded-for", value.parse().expect("header"));
        }
        headers
    }

    fn peer(address: &str) -> Option<SocketAddr> {
        Some(address.parse().expect("addr"))
    }

    const LOOPBACK: &str = "127.0.0.1:54321";
    const LOOPBACK_V6: &str = "[::1]:54321";
    const REMOTE: &str = "198.51.100.4:54321";

    #[test]
    fn the_socket_peer_is_used_by_default() {
        assert_eq!(
            resolve(&headers(Some("203.0.113.9")), peer(LOOPBACK), false),
            Some("127.0.0.1".to_owned()),
            "a spoofable header must be ignored unless a proxy is trusted"
        );
    }

    #[test]
    fn the_last_forwarded_entry_is_taken_from_a_loopback_proxy() {
        assert_eq!(
            resolve(&headers(Some("203.0.113.9")), peer(LOOPBACK), true),
            Some("203.0.113.9".to_owned())
        );
        assert_eq!(
            resolve(&headers(Some("203.0.113.9")), peer(LOOPBACK_V6), true),
            Some("203.0.113.9".to_owned()),
            "an IPv6 loopback peer is the same proxy"
        );

        // A client that invents entries only manages to prepend them; the
        // trusted proxy's own observation is still last.
        assert_eq!(
            resolve(
                &headers(Some("1.1.1.1, 2.2.2.2, 203.0.113.9")),
                peer(LOOPBACK),
                true
            ),
            Some("203.0.113.9".to_owned())
        );
    }

    /// The correction that matters: the flag is necessary but not sufficient.
    #[test]
    fn a_forwarded_header_from_a_non_loopback_peer_is_ignored() {
        assert_eq!(
            resolve(&headers(Some("203.0.113.9")), peer(REMOTE), true),
            Some("198.51.100.4".to_owned()),
            "only the local proxy may rewrite the client address"
        );
    }

    /// Without a peer there is no boundary to check, so the header cannot be
    /// believed either.
    #[test]
    fn a_forwarded_header_without_a_peer_is_ignored() {
        assert_eq!(resolve(&headers(Some("203.0.113.9")), None, true), None);
    }

    #[test]
    fn a_missing_or_empty_header_falls_back_to_the_peer() {
        assert_eq!(
            resolve(&headers(None), peer(LOOPBACK), true),
            Some("127.0.0.1".to_owned())
        );
        assert_eq!(
            resolve(&headers(Some("  ")), peer(LOOPBACK), true),
            Some("127.0.0.1".to_owned())
        );
    }

    #[test]
    fn an_unknown_client_is_none_rather_than_a_guess() {
        assert_eq!(resolve(&headers(None), None, true), None);
    }
}
