use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Mutex,
};

use quick_cache::{DefaultHashBuilder, Lifecycle, UnitWeighter, unsync::Cache};

use super::Result;
use crate::{
    error::SError,
    msgs::socks5::{AddrOrDomain, SocksAddr},
};

const CAPACITY: usize = 1 << 16;
const MAX_INDEX: u32 = 131070;

/// Mappings are stable while resident. Allocation indices are never reused
/// during this manager's lifetime, including after eviction.
#[derive(Default)]
pub struct FakeIp {
    mappings: Mutex<Mappings>,
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

impl Default for Mappings {
    fn default() -> Self {
        Self::with_capacity(CAPACITY)
    }
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
    pub fn allocate(&self, domain: &str, ipv6: bool) -> Result<IpAddr> {
        let mut mappings = self.mappings.lock().unwrap();
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        let index = if let Some(index) = mappings.by_name.get(&domain) {
            *index
        } else {
            let index = mappings.next_index;
            if index > MAX_INDEX {
                return Err(SError::DnsError("fake IP pool exhausted".into()));
            }
            mappings.next_index += 1;
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
        }
    }

    fn address(ip: IpAddr) -> SocksAddr {
        SocketAddr::new(ip, 443).into()
    }

    #[test]
    fn eviction_removes_both_families_and_never_reuses_indices() {
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
    fn exhaustion_preserves_residents_and_does_not_reclaim_evicted_indices() {
        let fake = fake(1);
        let old = fake.allocate("old.test", false).unwrap();
        fake.mappings.lock().unwrap().next_index = MAX_INDEX;
        let last = fake.allocate("last.test", false).unwrap();
        assert!(fake.restore(&address(old)).is_err());
        assert!(fake.allocate("old.test", false).is_err());
        assert!(fake.allocate("new.test", true).is_err());
        assert_eq!(fake.allocate("LAST.TEST.", false).unwrap(), last);
        let v6 = fake.allocate("last.test", true).unwrap();
        assert!(fake.restore(&address(v6)).is_ok());
        assert_eq!(fake.mappings.lock().unwrap().by_index.len(), 1);
    }

    #[test]
    fn concurrent_allocations_share_one_dual_stack_mapping() {
        let fake = fake(4);
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
        assert_eq!(mappings.next_index, 2);
    }
}
