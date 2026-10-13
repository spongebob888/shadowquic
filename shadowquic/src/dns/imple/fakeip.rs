use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
    sync::Mutex,
};

use super::Result;
use crate::{
    error::SError,
    msgs::socks5::{AddrOrDomain, SocksAddr},
};

/// A bounded LRU mapping between domains and synthetic addresses.
pub struct FakeIp {
    mappings: Mutex<Mappings>,
    ipv4: Range,
    ipv6: Range,
    capacity: u32,
}

#[derive(Clone, Copy)]
struct Range {
    base: u128,
    last_index: u32,
}

#[derive(Default)]
struct Mappings {
    by_name: HashMap<String, u32>,
    by_index: Vec<String>,
    generations: Vec<u64>,
    /// May contain stale generations, which are discarded during eviction.
    lru: VecDeque<(u32, u64)>,
    generation: u64,
}

impl FakeIp {
    pub const MAX_MAPPINGS: u32 = 1 << 16;

    pub fn new(ipv4_range: &str, ipv6_range: &str) -> Result<Self> {
        let ipv4 = parse_range(ipv4_range, false)?;
        let ipv6 = parse_range(ipv6_range, true)?;
        Ok(Self {
            mappings: Mutex::default(),
            ipv4,
            ipv6,
            capacity: Self::MAX_MAPPINGS.min(ipv4.last_index).min(ipv6.last_index),
        })
    }

    pub fn allocate(&self, domain: &str, ipv6: bool) -> Result<IpAddr> {
        let mut mappings = self.mappings.lock().unwrap();
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        let index = if let Some(index) = mappings.by_name.get(&domain).copied() {
            mappings.touch(index);
            index
        } else if mappings.by_index.len() < self.capacity as usize {
            let index = mappings.by_index.len() as u32 + 1;
            mappings.by_index.push(domain.clone());
            mappings.generations.push(0);
            mappings.by_name.insert(domain, index);
            mappings.touch(index);
            index
        } else {
            let index = mappings.least_recently_used();
            let old_domain =
                std::mem::replace(&mut mappings.by_index[index as usize - 1], domain.clone());
            mappings.by_name.remove(&old_domain);
            mappings.by_name.insert(domain, index);
            mappings.touch(index);
            index
        };
        Ok(if ipv6 {
            IpAddr::V6(Ipv6Addr::from(self.ipv6.base + index as u128))
        } else {
            IpAddr::V4(Ipv4Addr::from((self.ipv4.base + index as u128) as u32))
        })
    }

    pub fn restore(&self, addr: &SocksAddr) -> Result<SocksAddr> {
        let index = match addr.addr {
            AddrOrDomain::V4(bytes) => self.index(u32::from_be_bytes(bytes) as u128, self.ipv4),
            AddrOrDomain::V6(bytes) => self.index(u128::from_be_bytes(bytes), self.ipv6),
            _ => return Ok(addr.clone()),
        };
        let Some(index) = index else {
            return Ok(addr.clone());
        };
        let mut mappings = self.mappings.lock().unwrap();
        let name = index
            .checked_sub(1)
            .and_then(|index| mappings.by_index.get(index as usize))
            .cloned()
            .ok_or_else(|| SError::DnsError(format!("unknown fake IP: {addr}")))?;
        mappings.touch(index);
        Ok(SocksAddr {
            addr: AddrOrDomain::Domain(name.into_bytes().into()),
            port: addr.port,
        })
    }

    fn index(&self, address: u128, range: Range) -> Option<u32> {
        let Some(offset) = address.checked_sub(range.base) else {
            return None;
        };
        if offset == 0 || offset > range.last_index as u128 {
            return None;
        }
        Some(offset as u32)
    }
}

impl Mappings {
    fn touch(&mut self, index: u32) {
        self.generation += 1;
        self.generations[index as usize - 1] = self.generation;
        self.lru.push_back((index, self.generation));
        if self.lru.len() > self.by_index.len().saturating_mul(2) {
            let generations = &self.generations;
            self.lru
                .retain(|(index, generation)| generations[*index as usize - 1] == *generation);
        }
    }

    fn least_recently_used(&mut self) -> u32 {
        loop {
            let (index, generation) = self.lru.pop_front().unwrap();
            if self.generations[index as usize - 1] == generation {
                return index;
            }
        }
    }
}

impl Default for FakeIp {
    fn default() -> Self {
        Self::new(
            &super::config::default_ipv4_range(),
            &super::config::default_ipv6_range(),
        )
        .unwrap()
    }
}

fn parse_range(value: &str, ipv6: bool) -> Result<Range> {
    let (address, prefix) = value
        .split_once('/')
        .ok_or_else(|| SError::InvalidConfig(format!("invalid fake IP range: {value}")))?;
    let prefix = prefix
        .parse::<u8>()
        .map_err(|_| SError::InvalidConfig(format!("invalid fake IP range: {value}")))?;
    let (base, bits) = if ipv6 {
        (
            u128::from(Ipv6Addr::from_str(address).map_err(|_| {
                SError::InvalidConfig(format!("invalid IPv6 fake IP range: {value}"))
            })?),
            128,
        )
    } else {
        (
            u32::from(Ipv4Addr::from_str(address).map_err(|_| {
                SError::InvalidConfig(format!("invalid IPv4 fake IP range: {value}"))
            })?) as u128,
            32,
        )
    };
    if prefix >= bits {
        return Err(SError::InvalidConfig(format!(
            "fake IP range must contain at least two usable addresses: {value}"
        )));
    }
    let host_bits = bits - prefix;
    let host_mask = if host_bits == 128 {
        u128::MAX
    } else {
        (1u128 << host_bits) - 1
    };
    if base & host_mask != 0 {
        return Err(SError::InvalidConfig(format!(
            "fake IP range must use its network address: {value}"
        )));
    }
    let last_index = if host_bits > 32 {
        u32::MAX
    } else {
        ((1u128 << host_bits) - 2) as u32
    };
    if last_index == 0 {
        return Err(SError::InvalidConfig(format!(
            "fake IP range must contain at least two usable addresses: {value}"
        )));
    }
    Ok(Range { base, last_index })
}
