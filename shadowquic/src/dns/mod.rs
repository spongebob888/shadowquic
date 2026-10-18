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
        Some(strategy) => crate::direct::outbound::apply_dns_strategy(addresses, strategy),
        None => addresses.next(),
    };
    address.ok_or_else(|| SError::DomainResolveFailed(dst.to_string()))
}
