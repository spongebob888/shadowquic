use std::net::SocketAddr;

use crate::config::DnsStrategy;

#[cfg(feature = "dns-server")]
mod imple;
#[cfg(feature = "dns-server")]
pub use imple::*;

#[cfg(not(feature = "dns-server"))]
mod unimple;
#[cfg(not(feature = "dns-server"))]
pub use unimple::*;

/// Resolve an endpoint with the selected resolver without blocking the runtime.
pub(crate) async fn resolve_destination(
    resolver: &dyn DnsService,
    dst: &crate::msgs::socks5::SocksAddr,
    strategy: Option<&crate::config::DnsStrategy>,
) -> Result<std::net::SocketAddr, crate::error::SError> {
    use crate::{error::SError, msgs::socks5::AddrOrDomain};
    use std::net::{IpAddr, SocketAddr};
    let ips = match &dst.addr {
        AddrOrDomain::V4(ip) => vec![IpAddr::from(*ip)],
        AddrOrDomain::V6(ip) => vec![IpAddr::from(*ip)],
        AddrOrDomain::Domain(domain) => {
            resolver
                .resolve(
                    std::str::from_utf8(&domain.contents)
                        .map_err(|_| SError::DomainResolveFailed(dst.to_string()))?,
                )
                .await?
        }
    };
    let mut addresses = ips.into_iter().map(|ip| SocketAddr::new(ip, dst.port));
    let address = match strategy {
        Some(strategy) => apply_dns_strategy(addresses, strategy),
        None => addresses.next(),
    };
    address.ok_or_else(|| SError::DomainResolveFailed(dst.to_string()))
}

pub(crate) fn apply_dns_strategy<It>(mut ip_list: It, strategy: &DnsStrategy) -> Option<SocketAddr>
where
    It: Iterator<Item = SocketAddr>,
{
    match strategy {
        DnsStrategy::Ipv4Only => ip_list.find(|addr| addr.is_ipv4()),
        DnsStrategy::Ipv6Only => ip_list.find(|addr| addr.is_ipv6()),
        DnsStrategy::PreferIpv4 => {
            let mut first = None;
            for ip in ip_list {
                if ip.is_ipv4() {
                    return Some(ip);
                }
                if first.is_none() {
                    first = Some(ip);
                }
            }
            first
        }
        DnsStrategy::PreferIpv6 => {
            let mut first = None;
            for ip in ip_list {
                if ip.is_ipv6() {
                    return Some(ip);
                }
                if first.is_none() {
                    first = Some(ip);
                }
            }
            first
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn make_addrs() -> Vec<SocketAddr> {
        vec![
            // 127.0.0.1:8080 (IPv4)
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            // ::1:8080 (IPv6)
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8080),
        ]
    }

    #[test]
    fn test_apply_dns_strategy_ipv4_only() {
        let addrs = make_addrs();
        let result = apply_dns_strategy(addrs.clone().into_iter(), &DnsStrategy::Ipv4Only);
        assert_eq!(result, Some(addrs[0]));
    }

    #[test]
    fn test_apply_dns_strategy_ipv6_only() {
        let addrs = make_addrs();
        let result = apply_dns_strategy(addrs.clone().into_iter(), &DnsStrategy::Ipv6Only);
        assert_eq!(result, Some(addrs[1]));
    }

    #[test]
    fn test_apply_dns_strategy_prefer_ipv4() {
        let addrs = make_addrs();
        let result = apply_dns_strategy(addrs.clone().into_iter(), &DnsStrategy::PreferIpv4);
        assert_eq!(result, Some(addrs[0]));
    }

    #[test]
    fn test_apply_dns_strategy_prefer_ipv6() {
        let addrs = make_addrs();
        let result = apply_dns_strategy(addrs.clone().into_iter(), &DnsStrategy::PreferIpv6);
        assert_eq!(result, Some(addrs[1]));
    }

    #[test]
    fn test_apply_dns_strategy_empty() {
        let addrs: Vec<SocketAddr> = vec![];
        let result = apply_dns_strategy(addrs.into_iter(), &DnsStrategy::PreferIpv4);
        assert_eq!(result, None);
    }
}
