use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Mutex,
};

use super::Result;
use crate::{
    error::SError,
    msgs::socks5::{AddrOrDomain, SocksAddr},
};

/// Stable, non-recycled mappings for the lifetime of the manager. Exhaustion is
/// an error rather than silently assigning an active address to another name.
#[derive(Default)]
pub struct FakeIp {
    mappings: Mutex<Mappings>,
}

#[derive(Default)]
struct Mappings {
    by_name: HashMap<String, u32>,
    by_index: Vec<String>,
}

impl FakeIp {
    pub fn allocate(&self, domain: &str, ipv6: bool) -> Result<IpAddr> {
        let mut mappings = self.mappings.lock().unwrap();
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        let next = mappings.by_index.len() as u32 + 1;
        if next >= 131071 && !mappings.by_name.contains_key(&domain) {
            return Err(SError::DnsError("fake IP pool exhausted".into()));
        }
        let index = if let Some(index) = mappings.by_name.get(&domain) {
            *index
        } else {
            mappings.by_index.push(domain.clone());
            mappings.by_name.insert(domain, next);
            next
        };
        Ok(if ipv6 {
            IpAddr::V6(Ipv6Addr::from((0xfd00u128 << 112) | index as u128))
        } else {
            IpAddr::V4(Ipv4Addr::from(
                u32::from(Ipv4Addr::new(198, 18, 0, 0)) + index,
            ))
        })
    }

    pub fn restore(&self, addr: &SocksAddr) -> Result<SocksAddr> {
        let index = match addr.addr {
            AddrOrDomain::V4(bytes) if bytes[0] == 198 && bytes[1] & 0xfe == 18 => {
                u32::from_be_bytes(bytes) - u32::from(Ipv4Addr::new(198, 18, 0, 0))
            }
            AddrOrDomain::V6(bytes) if u128::from_be_bytes(bytes) >> 32 == (0xfd00u128 << 80) => {
                u128::from_be_bytes(bytes) as u32
            }
            _ => return Ok(addr.clone()),
        };
        let mappings = self.mappings.lock().unwrap();
        let name = index
            .checked_sub(1)
            .and_then(|index| mappings.by_index.get(index as usize))
            .ok_or_else(|| SError::DnsError(format!("unknown fake IP: {addr}")))?;
        Ok(SocksAddr {
            addr: AddrOrDomain::Domain(name.as_bytes().to_vec().into()),
            port: addr.port,
        })
    }
}
