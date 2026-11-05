//! Optional hostname resolution above the raw IPv6 transport configuration.
//!
//! Use iphost's hostname types and explicitly selected resolver. Call resolution
//! on a helper thread, then supply IPv6 candidates to the transport owner. Never
//! call it from a packet worker. A failure leaves the active destination intact;
//! selecting a bypassing resolver never triggers a system-resolver fallback.

pub use iphost::{
    ClientId, Dhcpv6ResolverConfig, FQDN, IpHost, IpHostResolver, Ipv6ResolverConfig,
    RaResolverConfig, Resolver, StaticResolverConfig,
};
use std::net::{IpAddr, Ipv6Addr};

/// Resolves through the selected iphost resolver and returns its stable IPv6 pick.
///
/// Honors iphost's cache-on-error policy. IPv4, mapped, multicast and unspecified
/// addresses are unusable. A link-local result still needs an explicit scope ID
/// when supplied to the core. This function may block, allocate and take locks.
pub fn resolve_ipv6(resolver: &IpHostResolver) -> Option<Ipv6Addr> {
    let addresses = resolver.resolve();
    resolver
        .ipv6_addr()
        .filter(|ip| crate::config::unicast(*ip) && addresses.contains(&IpAddr::V6(*ip)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_resolution_only_returns_usable_ipv6() {
        for (host, valid) in [
            ("2001:db8::1", true),
            ("::1", true),
            ("fe80::1", true),
            ("::", false),
            ("ff02::1", false),
            ("::ffff:192.0.2.1", false),
            ("192.0.2.1", false),
        ] {
            let resolver = IpHostResolver::new(host.parse().unwrap());
            assert_eq!(resolve_ipv6(&resolver).is_some(), valid);
            assert_eq!(resolver.resolver(), Resolver::System);
        }
    }

    #[test]
    fn hostname_representation_is_iphost_and_construction_does_not_resolve() {
        let host: IpHost = "test.example.invalid".parse().unwrap();
        assert!(matches!(host, IpHost::Fqdn(_)));
        let resolver = IpHostResolver::new(host.clone());
        assert_eq!(resolver.ip_host(), host);
        assert_eq!(resolver.ipv6_addr(), None);
    }
}
