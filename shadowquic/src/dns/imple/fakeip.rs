use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Mutex,
};

use ipnet::{Ipv4Net, Ipv6Net};
use quick_cache::{DefaultHashBuilder, Lifecycle, UnitWeighter, unsync::Cache};

use super::{
    Result,
    config::{default_ipv4_range, default_ipv6_range},
};
use crate::{
    error::SError,
    msgs::socks5::{AddrOrDomain, SocksAddr},
};

const CAPACITY: usize = 1 << 16;

/// Mappings are stable while resident. Allocation wraps through the pool,
/// reusing only indices whose mappings have been evicted.
pub struct FakeIp {
    mappings: Mutex<Mappings>,
    ipv4_range: Ipv4Net,
    ipv6_range: Ipv6Net,
    max_index: u32,
}

impl Default for FakeIp {
    fn default() -> Self {
        Self::new(default_ipv4_range(), default_ipv6_range()).unwrap()
    }
}

struct Evictions;

impl Lifecycle<String, u32> for Evictions {
    type RequestState = Vec<u32>;

    fn on_evict(&self, evicted: &mut Vec<u32>, _domain: String, index: u32) {
        evicted.push(index);
    }
}

struct Mappings {
    by_name: Cache<String, u32, UnitWeighter, DefaultHashBuilder, Evictions>,
    by_index: HashMap<u32, String>,
    next_index: u32,
}

impl Mappings {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            by_name: Cache::with(
                capacity,
                capacity as u64,
                UnitWeighter,
                DefaultHashBuilder::default(),
                Evictions,
            ),
            by_index: HashMap::new(),
            next_index: 1,
        }
    }
}

impl FakeIp {
    /// Create paired fake-IP pools. Reserve the first and last address in each
    /// range and use the smaller pool's offsets for both address families.
    pub fn new(ipv4_range: Ipv4Net, ipv6_range: Ipv6Net) -> Result<Self> {
        let max_index = Self::validate_ranges(ipv4_range, ipv6_range)?;
        // Leave at least one unoccupied index for allocation before eviction.
        let capacity = CAPACITY.min((max_index - 1) as usize);
        Ok(Self {
            mappings: Mutex::new(Mappings::with_capacity(capacity)),
            ipv4_range,
            ipv6_range,
            max_index,
        })
    }

    pub(super) fn validate_ranges(ipv4_range: Ipv4Net, ipv6_range: Ipv6Net) -> Result<u32> {
        if ipv4_range.addr() != ipv4_range.network() || ipv6_range.addr() != ipv6_range.network() {
            return Err(SError::InvalidConfig(
                "fake-IP ranges must have zero host bits".into(),
            ));
        }
        if ipv4_range.prefix_len() > 30 || ipv6_range.prefix_len() > 126 {
            return Err(SError::InvalidConfig("fake-IP ranges must contain at least four addresses (IPv4 /30 or larger, IPv6 /126 or larger)".into()));
        }
        let ipv4_usable = (1u64 << (32 - ipv4_range.prefix_len())) - 2;
        let ipv6_host_bits = 128 - ipv6_range.prefix_len();
        let ipv6_usable = if ipv6_host_bits > 32 {
            u64::from(u32::MAX)
        } else {
            (1u64 << ipv6_host_bits) - 2
        };
        Ok(ipv4_usable.min(ipv6_usable) as u32)
    }

    pub fn allocate(&self, domain: &str, ipv6: bool) -> Result<IpAddr> {
        let mut mappings = self.mappings.lock().unwrap();
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        let index = if let Some(index) = mappings.by_name.get(&domain) {
            *index
        } else {
            // The mapping capacity is smaller than the pool, so a free index
            // always exists. Keep resident IPv4/IPv6 pairs intact across wraps.
            let index = loop {
                let index = mappings.next_index;
                mappings.next_index = if index == self.max_index {
                    1
                } else {
                    index + 1
                };
                if !mappings.by_index.contains_key(&index) {
                    break index;
                }
            };
            let mut evicted = Vec::new();
            mappings
                .by_name
                .insert_with_lifecycle(domain.clone(), index, &mut evicted);
            for old_index in evicted {
                mappings.by_index.remove(&old_index);
            }
            mappings.by_index.insert(index, domain);
            index
        };
        Ok(if ipv6 {
            IpAddr::V6(Ipv6Addr::from(
                u128::from(self.ipv6_range.network()) + u128::from(index),
            ))
        } else {
            IpAddr::V4(Ipv4Addr::from(u32::from(self.ipv4_range.network()) + index))
        })
    }

    pub fn restore(&self, addr: &SocksAddr) -> Result<SocksAddr> {
        let offset = match addr.addr {
            AddrOrDomain::V4(bytes) if self.ipv4_range.contains(&Ipv4Addr::from(bytes)) => {
                u128::from(u32::from_be_bytes(bytes) - u32::from(self.ipv4_range.network()))
            }
            AddrOrDomain::V6(bytes) if self.ipv6_range.contains(&Ipv6Addr::from(bytes)) => {
                u128::from_be_bytes(bytes) - u128::from(self.ipv6_range.network())
            }
            _ => return Ok(addr.clone()),
        };
        if offset == 0 || offset > u128::from(self.max_index) {
            return Err(SError::DnsError(format!("unknown fake IP: {addr}")));
        }
        let index = offset as u32;
        let mappings = self.mappings.lock().unwrap();
        let name = mappings
            .by_index
            .get(&index)
            .cloned()
            .ok_or_else(|| SError::DnsError(format!("unknown fake IP: {addr}")))?;
        // Record traffic use as well as DNS use in the eviction policy.
        let resident_index = mappings.by_name.get(&name);
        debug_assert_eq!(resident_index, Some(&index));
        Ok(SocksAddr {
            addr: AddrOrDomain::Domain(name.as_bytes().to_vec().into()),
            port: addr.port,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::SocketAddr, sync::Barrier};

    fn fake(capacity: usize) -> FakeIp {
        FakeIp {
            mappings: Mutex::new(Mappings::with_capacity(capacity)),
            ..FakeIp::default()
        }
    }

    fn address(ip: IpAddr) -> SocksAddr {
        SocketAddr::new(ip, 443).into()
    }

    #[test]
    fn small_pools_wrap_and_evict_without_overwriting_live_indices() {
        for (v4, v6, max_index) in [
            ("198.18.16.0/29", "fd00:1234::/125", 6),
            ("198.18.16.0/29", "fd00:1234::/126", 2),
            ("198.18.16.0/30", "fd00:1234::/125", 2),
        ] {
            let v4: Ipv4Net = v4.parse().unwrap();
            let v6: Ipv6Net = v6.parse().unwrap();
            let fake = FakeIp::new(v4, v6).unwrap();
            assert_eq!(fake.max_index, max_index);
            for i in 0..32 {
                let name = format!("{i}.test");
                let ip4 = fake.allocate(&name, false).unwrap();
                let ip6 = fake.allocate(&name, true).unwrap();
                assert!(v4.contains(&match ip4 {
                    IpAddr::V4(ip) => ip,
                    _ => panic!(),
                }));
                assert!(v6.contains(&match ip6 {
                    IpAddr::V6(ip) => ip,
                    _ => panic!(),
                }));
                let domain = AddrOrDomain::Domain(name.as_bytes().to_vec().into());
                assert_eq!(fake.restore(&address(ip4)).unwrap().addr, domain);
                assert_eq!(fake.restore(&address(ip6)).unwrap().addr, domain);
                let mappings = fake.mappings.lock().unwrap();
                assert!(mappings.by_name.len() <= (max_index - 1) as usize);
                assert_eq!(mappings.by_name.len(), mappings.by_index.len());
                for (name, index) in mappings.by_name.iter() {
                    assert_eq!(mappings.by_index.get(index), Some(name));
                }
            }
        }
    }

    #[test]
    fn restoration_checks_the_whole_configured_range_without_truncating_offsets() {
        let fake = FakeIp::new(
            "198.18.16.0/20".parse().unwrap(),
            "fd00:1234::/64".parse().unwrap(),
        )
        .unwrap();
        fake.allocate("first.test", false).unwrap();
        for ip in [
            "198.18.16.0",
            "198.18.31.255",
            "198.18.16.2",
            "fd00:1234::",
            "fd00:1234::2",
            "fd00:1234::1:0:1",
            "fd00:1234::ffff:ffff:ffff:ffff",
        ] {
            assert!(fake.restore(&address(ip.parse().unwrap())).is_err(), "{ip}");
        }
        for ip in [
            "198.18.0.1",
            "198.18.32.1",
            "fd00::1",
            "fd00:1234:0:1::1",
            "203.0.113.1",
        ] {
            let addr = address(ip.parse().unwrap());
            assert_eq!(fake.restore(&addr).unwrap(), addr, "{ip}");
        }
    }

    #[test]
    fn extreme_prefixes_allocate_without_overflow() {
        let tiny = FakeIp::new(
            "255.255.255.252/30".parse().unwrap(),
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffc/126"
                .parse()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            tiny.allocate("tiny.test", false).unwrap(),
            "255.255.255.253".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            tiny.allocate("tiny.test", true).unwrap(),
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffd"
                .parse::<IpAddr>()
                .unwrap()
        );

        let wide = FakeIp::new("0.0.0.0/0".parse().unwrap(), "::/0".parse().unwrap()).unwrap();
        assert_eq!(wide.max_index, u32::MAX - 1);
        wide.mappings.lock().unwrap().next_index = wide.max_index;
        assert_eq!(
            wide.allocate("wide.test", false).unwrap(),
            "255.255.255.254".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            wide.allocate("wide.test", true).unwrap(),
            "::ffff:fffe".parse::<IpAddr>().unwrap()
        );
        assert_eq!(wide.mappings.lock().unwrap().next_index, 1);
        assert!(wide.restore(&address("::1:0:1".parse().unwrap())).is_err());
    }

    #[test]
    fn eviction_removes_both_families_and_uses_fresh_indices_before_wrap() {
        let fake = fake(4);
        let mut allocated = Vec::new();
        for i in 0..12 {
            let name = format!("{i}.test");
            let v4 = fake.allocate(&name, false).unwrap();
            let v6 = fake.allocate(&name.to_uppercase(), true).unwrap();
            assert_eq!(fake.allocate(&format!("{name}."), false).unwrap(), v4);
            assert_eq!(fake.restore(&address(v4)).unwrap().port, 443);
            allocated.push((name, v4, v6));
            let mappings = fake.mappings.lock().unwrap();
            assert!(mappings.by_name.len() <= 4);
            assert_eq!(mappings.by_name.len(), mappings.by_index.len());
            for (name, index) in mappings.by_name.iter() {
                assert_eq!(mappings.by_index.get(index), Some(name));
            }
        }
        let mut evicted = None;
        for (name, v4, v6) in &allocated {
            let restored = fake.restore(&address(*v4));
            if let Ok(restored) = restored {
                assert_eq!(
                    restored.addr,
                    AddrOrDomain::Domain(name.as_bytes().to_vec().into())
                );
                assert_eq!(fake.restore(&address(*v6)).unwrap().addr, restored.addr);
            } else {
                assert!(fake.restore(&address(*v6)).is_err());
                evicted = Some((name, v4, v6));
            }
        }
        let (name, old_v4, old_v6) = evicted.expect("capacity must evict a mapping");
        let new_v4 = fake.allocate(name, false).unwrap();
        let new_v6 = fake.allocate(name, true).unwrap();
        assert!(
            allocated
                .iter()
                .all(|(_, v4, v6)| *v4 != new_v4 && *v6 != new_v6)
        );
        assert!(fake.restore(&address(*old_v4)).is_err());
        assert!(fake.restore(&address(*old_v6)).is_err());
    }

    #[test]
    fn wrapping_skips_resident_indices_and_preserves_dual_stack_mappings() {
        let fake = fake(4);
        let first = fake.allocate("first.test", false).unwrap();
        let second = fake.allocate("second.test", false).unwrap();
        fake.mappings.lock().unwrap().next_index = fake.max_index;
        let last = fake.allocate("last.test", false).unwrap();
        assert_eq!(last, IpAddr::V4(Ipv4Addr::new(198, 19, 255, 254)));
        assert_eq!(fake.mappings.lock().unwrap().next_index, 1);

        let wrapped = fake.allocate("wrapped.test", false).unwrap();
        assert_eq!(wrapped, IpAddr::V4(Ipv4Addr::new(198, 18, 0, 3)));
        for (name, ip) in [
            ("first.test", first),
            ("second.test", second),
            ("last.test", last),
            ("wrapped.test", wrapped),
        ] {
            assert_eq!(fake.allocate(name, false).unwrap(), ip);
            let v6 = fake.allocate(name, true).unwrap();
            assert_eq!(
                fake.restore(&address(ip)).unwrap().addr,
                fake.restore(&address(v6)).unwrap().addr
            );
        }
        let mappings = fake.mappings.lock().unwrap();
        assert_eq!(mappings.by_name.len(), 4);
        assert_eq!(mappings.by_index.len(), 4);
        assert_eq!(mappings.next_index, 4);
    }

    #[test]
    fn wrapping_reuses_evicted_indices_for_both_address_families() {
        let fake = fake(1);
        let old = fake.allocate("old.test", false).unwrap();
        let old_v6 = fake.allocate("old.test", true).unwrap();
        fake.mappings.lock().unwrap().next_index = fake.max_index;
        let last = fake.allocate("last.test", false).unwrap();
        let last_v6 = fake.allocate("last.test", true).unwrap();
        assert!(fake.restore(&address(old)).is_err());
        assert!(fake.restore(&address(old_v6)).is_err());
        assert_eq!(fake.allocate("LAST.TEST.", false).unwrap(), last);

        assert_eq!(fake.allocate("new.test", false).unwrap(), old);
        assert_eq!(fake.allocate("new.test", true).unwrap(), old_v6);
        assert!(fake.restore(&address(last)).is_err());
        assert!(fake.restore(&address(last_v6)).is_err());
        let new_domain = AddrOrDomain::Domain(b"new.test".to_vec().into());
        assert_eq!(fake.restore(&address(old)).unwrap().addr, new_domain);
        assert_eq!(fake.restore(&address(old_v6)).unwrap().addr, new_domain);
        let mappings = fake.mappings.lock().unwrap();
        assert_eq!(mappings.by_name.len(), 1);
        assert_eq!(mappings.by_index.len(), 1);
        assert_eq!(mappings.next_index, 2);
    }

    #[test]
    fn concurrent_allocations_share_one_dual_stack_mapping_at_wrap() {
        let fake = fake(4);
        fake.mappings.lock().unwrap().next_index = fake.max_index;
        let barrier = Barrier::new(8);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let fake = &fake;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        let ip = fake
                            .allocate(
                                if i % 2 == 0 {
                                    "SAME.TEST."
                                } else {
                                    "same.test"
                                },
                                i % 2 != 0,
                            )
                            .unwrap();
                        assert!(fake.restore(&address(ip)).is_ok());
                        ip
                    })
                })
                .collect();
            for (i, handle) in handles.into_iter().enumerate() {
                assert_eq!(
                    handle.join().unwrap(),
                    fake.allocate("same.test", i % 2 != 0).unwrap()
                );
            }
        });
        let mappings = fake.mappings.lock().unwrap();
        assert_eq!(mappings.by_name.len(), 1);
        assert_eq!(mappings.by_index.len(), 1);
        assert_eq!(mappings.next_index, 1);
    }
}
