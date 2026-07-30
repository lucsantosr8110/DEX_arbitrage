//! Phase 2D-D execution domain gate. Pure logic — no RPC, no process
//! spawning. Every write-capable RPC endpoint used by the fork campaign must
//! pass [`validate_loopback_endpoint`] before any transaction is sent.
//! Anything that isn't `127.0.0.1` / `localhost` / `::1` is rejected —
//! public URLs, private-network IPs, external hostnames, and redirects are
//! all treated identically as "not loopback".

use thiserror::Error;
use url::Url;

/// Marker for the only execution domain this phase is allowed to write to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionDomain {
    LocalForkOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EndpointRejection {
    #[error("FORK_ENDPOINT_EMPTY")]
    Empty,
    #[error("FORK_ENDPOINT_UNPARSEABLE: {0}")]
    Unparseable(String),
    #[error("FORK_ENDPOINT_NOT_LOOPBACK: host={0}")]
    NotLoopback(String),
    #[error("FORK_ENDPOINT_MISSING_HOST")]
    MissingHost,
}

const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "localhost", "::1", "[::1]"];

/// Validates that `endpoint` is a loopback-only URL suitable for the write
/// client. Rejects public URLs, private-network (non-loopback) IPs, external
/// hostnames, and anything that fails to parse as a URL — fail-closed, no
/// "looks local enough" heuristics.
pub fn validate_loopback_endpoint(endpoint: &str) -> Result<ExecutionDomain, EndpointRejection> {
    if endpoint.trim().is_empty() {
        return Err(EndpointRejection::Empty);
    }
    let url = Url::parse(endpoint)
        .map_err(|e| EndpointRejection::Unparseable(format!("{endpoint}: {e}")))?;
    let host = url
        .host_str()
        .ok_or(EndpointRejection::MissingHost)?
        .to_string();
    if LOOPBACK_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return Ok(ExecutionDomain::LocalForkOnly);
    }
    // IPv4 loopback range 127.0.0.0/8, not just 127.0.0.1.
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        if ip.is_loopback() {
            return Ok(ExecutionDomain::LocalForkOnly);
        }
    }
    if let Ok(ip) = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::Ipv6Addr>()
    {
        if ip.is_loopback() {
            return Ok(ExecutionDomain::LocalForkOnly);
        }
    }
    Err(EndpointRejection::NotLoopback(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_127_0_0_1() {
        assert!(validate_loopback_endpoint("http://127.0.0.1:8545").is_ok());
    }

    #[test]
    fn accepts_localhost() {
        assert!(validate_loopback_endpoint("http://localhost:8545").is_ok());
    }

    #[test]
    fn accepts_ipv6_loopback() {
        assert!(validate_loopback_endpoint("http://[::1]:8545").is_ok());
    }

    #[test]
    fn accepts_other_127_range_addresses() {
        assert!(validate_loopback_endpoint("http://127.0.0.5:8545").is_ok());
    }

    #[test]
    fn rejects_public_url() {
        let err = validate_loopback_endpoint("https://polygon-rpc.com").unwrap_err();
        assert!(matches!(err, EndpointRejection::NotLoopback(_)));
    }

    #[test]
    fn rejects_alchemy_style_url() {
        let err =
            validate_loopback_endpoint("https://polygon-mainnet.g.alchemy.com/v2/abc").unwrap_err();
        assert!(matches!(err, EndpointRejection::NotLoopback(_)));
    }

    #[test]
    fn rejects_private_network_ip() {
        // Private LAN address is NOT loopback — must still be rejected.
        let err = validate_loopback_endpoint("http://192.168.1.50:8545").unwrap_err();
        assert!(matches!(err, EndpointRejection::NotLoopback(_)));
    }

    #[test]
    fn rejects_empty_endpoint() {
        assert_eq!(
            validate_loopback_endpoint(""),
            Err(EndpointRejection::Empty)
        );
    }

    #[test]
    fn rejects_unparseable_endpoint() {
        assert!(matches!(
            validate_loopback_endpoint("not a url"),
            Err(EndpointRejection::Unparseable(_))
        ));
    }

    #[test]
    fn rejects_external_hostname_even_if_it_contains_localhost_substring() {
        let err = validate_loopback_endpoint("https://localhost.evil.example").unwrap_err();
        assert!(matches!(err, EndpointRejection::NotLoopback(_)));
    }
}
