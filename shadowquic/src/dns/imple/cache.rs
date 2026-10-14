use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use quick_cache::sync::Cache;

use simple_dns::{CLASS, Packet, PacketFlag, QTYPE, RCODE, TYPE, rdata::RData};

use super::{Result, dns_error, ptr_names, reverse_name};

const CAPACITY: usize = 4096;
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct Entry {
    packet: Vec<u8>,
    inserted: Instant,
    expires: Instant,
}

/// Shared by non-fake-IP DNS services, keyed by the query with its transaction ID cleared.
pub struct DnsCache {
    entries: Cache<Vec<u8>, Arc<Entry>>,
    last_cleanup: Mutex<Instant>,
}

impl Default for DnsCache {
    fn default() -> Self {
        Self::with_capacity(CAPACITY)
    }
}

fn key(query: &[u8]) -> Vec<u8> {
    let mut key = query.to_vec();
    key[..2].fill(0);
    key
}

pub(super) fn addresses(packet: &Packet<'_>) -> Vec<IpAddr> {
    // Only accept address records belonging to the question or its CNAME chain.
    let Some(question) = packet.questions.first() else {
        return Vec::new();
    };
    let mut name = question.qname.clone();
    for _ in 0..packet.answers.len() {
        let Some(next) = packet.answers.iter().find_map(|rr| {
            if rr.name == name
                && let RData::CNAME(cname) = &rr.rdata
            {
                Some(cname.0.clone())
            } else {
                None
            }
        }) else {
            break;
        };
        name = next;
    }
    packet
        .answers
        .iter()
        .filter(|rr| rr.name == name)
        .filter_map(|rr| match &rr.rdata {
            RData::A(a) => Some(IpAddr::V4(a.address.into())),
            RData::AAAA(a) => Some(IpAddr::V6(a.address.into())),
            _ => None,
        })
        .collect()
}

impl DnsCache {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Cache::new(capacity),
            last_cleanup: Mutex::new(Instant::now()),
        }
    }

    fn cleanup(&self, now: Instant) {
        let Ok(mut last_cleanup) = self.last_cleanup.try_lock() else {
            return;
        };
        if now.saturating_duration_since(*last_cleanup) < CLEANUP_INTERVAL {
            return;
        }
        // retain checks and removes under each shard's write lock, so a
        // concurrent replacement cannot be removed based on an old snapshot.
        self.entries.retain(|_, entry| entry.expires > now);
        *last_cleanup = now;
    }

    pub(super) fn get(&self, query: &[u8]) -> Result<Option<Vec<u8>>> {
        let now = Instant::now();
        self.cleanup(now);
        let Some(entry) = self.entries.get(&key(query)).filter(|e| e.expires > now) else {
            return Ok(None);
        };
        let mut packet = Packet::parse(&entry.packet).map_err(dns_error)?;
        packet.set_id(u16::from_be_bytes([query[0], query[1]]));
        let elapsed = now.duration_since(entry.inserted).as_secs() as u32;
        for rr in packet
            .answers
            .iter_mut()
            .chain(&mut packet.name_servers)
            .chain(&mut packet.additional_records)
        {
            rr.ttl = rr.ttl.saturating_sub(elapsed);
        }
        packet.build_bytes_vec().map(Some).map_err(dns_error)
    }

    pub(super) fn insert(&self, query: &[u8], packet: Packet<'_>) {
        let now = Instant::now();
        self.cleanup(now);
        if packet.rcode() != RCODE::NoError
            || packet.has_flags(PacketFlag::TRUNCATION)
            || packet.answers.is_empty()
        {
            return;
        }
        let ttl = packet
            .answers
            .iter()
            .chain(&packet.name_servers)
            .chain(&packet.additional_records)
            .map(|rr| rr.ttl)
            .min()
            .unwrap_or(0);
        if ttl == 0 {
            return;
        }
        let Ok(packet) = packet.build_bytes_vec() else {
            return;
        };
        self.entries.insert(
            key(query),
            Arc::new(Entry {
                packet,
                inserted: now,
                expires: now + Duration::from_secs(ttl.into()),
            }),
        );
    }

    /// Cached addresses only; never performs network I/O. Names are case insensitive.
    pub fn lookup_cache(&self, domain: &str) -> Vec<IpAddr> {
        let domain = domain.trim_end_matches('.');
        let now = Instant::now();
        self.cleanup(now);
        let mut result = Vec::new();
        for (_, entry) in self.entries.iter().filter(|(_, e)| e.expires > now) {
            let Ok(packet) = Packet::parse(&entry.packet) else {
                continue;
            };
            if packet
                .questions
                .first()
                .is_some_and(|q| q.qname.to_string().eq_ignore_ascii_case(domain))
            {
                for ip in addresses(&packet) {
                    if !result.contains(&ip) {
                        result.push(ip);
                    }
                }
            }
        }
        result.sort();
        result
    }

    /// Returns the most recently cached name from matching A/AAAA or PTR answers.
    /// Follows CNAME chains, ignores expired entries, and never performs I/O.
    pub fn reverse_lookup_cache(&self, ip: IpAddr) -> Option<String> {
        let now = Instant::now();
        self.cleanup(now);
        let reverse_name = reverse_name(ip);
        self.entries
            .iter()
            .filter(|(_, e)| e.expires > now)
            .filter_map(|(_, e)| {
                let packet = Packet::parse(&e.packet).ok()?;
                let question = packet.questions.first()?;
                if question.qclass != CLASS::IN.into() {
                    return None;
                }
                let name = match question.qtype {
                    QTYPE::TYPE(TYPE::PTR)
                        if question
                            .qname
                            .to_string()
                            .eq_ignore_ascii_case(&reverse_name) =>
                    {
                        ptr_names(&packet).ok()?.into_iter().next()?
                    }
                    QTYPE::TYPE(TYPE::A | TYPE::AAAA) if addresses(&packet).contains(&ip) => {
                        question.qname.to_string()
                    }
                    _ => return None,
                };
                Some((e.inserted, name))
            })
            .max_by_key(|(inserted, _)| *inserted)
            .map(|(_, name)| name)
    }
}

#[cfg(test)]
impl DnsCache {
    pub(super) fn age_for_test(&self, duration: Duration) {
        let entries: Vec<_> = self.entries.iter().collect();
        for (key, entry) in entries {
            let mut entry = (*entry).clone();
            entry.inserted -= duration;
            entry.expires -= duration;
            self.entries.insert(key, Arc::new(entry));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_dns::{Name, Question, ResourceRecord};
    use std::{net::Ipv4Addr, sync::Barrier};

    fn query(name: &str, id: u16) -> Vec<u8> {
        let mut packet = Packet::new_query(id);
        packet.questions.push(Question::new(
            Name::new(name).unwrap(),
            TYPE::A.into(),
            CLASS::IN.into(),
            false,
        ));
        packet.build_bytes_vec().unwrap()
    }

    fn insert(cache: &DnsCache, query: &[u8], ttl: u32) {
        let mut packet = super::super::reply_for(Packet::parse(query).unwrap());
        packet.answers.push(ResourceRecord::new(
            packet.questions[0].qname.clone(),
            CLASS::IN,
            ttl,
            RData::A(Ipv4Addr::LOCALHOST.into()),
        ));
        cache.insert(query, packet);
    }

    #[test]
    fn expiry_is_enforced_between_scans_and_cleanup_is_throttled() {
        let cache = DnsCache::default();
        let query = query("expiry.test", 7);
        insert(&cache, &query, 60);
        cache.age_for_test(Duration::from_secs(10));
        let mut other_id = query.clone();
        other_id[..2].copy_from_slice(&42u16.to_be_bytes());
        let reply = cache.get(&other_id).unwrap().unwrap();
        let reply = Packet::parse(&reply).unwrap();
        assert_eq!(reply.id(), 42);
        assert_eq!(reply.answers[0].ttl, 50);
        cache.age_for_test(Duration::from_secs(51));
        assert!(cache.get(&query).unwrap().is_none());
        assert!(cache.lookup_cache("expiry.test").is_empty());
        assert!(
            cache
                .reverse_lookup_cache(Ipv4Addr::LOCALHOST.into())
                .is_none()
        );
        // Reads reject the expired response without running an early scan.
        assert_eq!(cache.entries.len(), 1);
        let last = *cache.last_cleanup.lock().unwrap();
        cache.cleanup(last + CLEANUP_INTERVAL - Duration::from_nanos(1));
        assert_eq!(cache.entries.len(), 1);
        cache.cleanup(last + CLEANUP_INTERVAL);
        assert_eq!(cache.entries.len(), 0);
    }

    #[test]
    fn cleanup_triggers_on_every_public_operation() {
        for operation in 0..4 {
            let cache = DnsCache::default();
            let query = query("cleanup.test", 1);
            insert(&cache, &query, 1);
            cache.age_for_test(Duration::from_secs(2));
            *cache.last_cleanup.lock().unwrap() -= CLEANUP_INTERVAL;
            match operation {
                0 => {
                    cache.get(&query).unwrap();
                }
                // Even an inadmissible response can trigger cleanup.
                1 => insert(&cache, &query, 0),
                2 => {
                    cache.lookup_cache("cleanup.test");
                }
                _ => {
                    cache.reverse_lookup_cache(Ipv4Addr::LOCALHOST.into());
                }
            }
            assert_eq!(cache.entries.len(), 0);
        }
    }

    #[test]
    fn concurrent_cleanup_preserves_a_refreshed_response() {
        let cache = DnsCache::default();
        let query = query("refresh.test", 1);
        for _ in 0..32 {
            insert(&cache, &query, 1);
            cache.age_for_test(Duration::from_secs(2));
            *cache.last_cleanup.lock().unwrap() -= CLEANUP_INTERVAL;
            let barrier = Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    cache.cleanup(Instant::now());
                });
                barrier.wait();
                insert(&cache, &query, 300);
            });
            assert!(cache.get(&query).unwrap().is_some());
        }
    }

    #[test]
    fn a_busy_cleanup_does_not_prevent_reads() {
        let cache = DnsCache::default();
        let query = query("busy.test", 1);
        insert(&cache, &query, 60);
        let _cleanup = cache.last_cleanup.lock().unwrap();
        assert!(cache.get(&query).unwrap().is_some());
    }

    #[test]
    fn capacity_is_bounded_and_replacement_does_not_evict_another_entry() {
        let cache = DnsCache::with_capacity(4);
        for i in 0..4 {
            insert(&cache, &query(&format!("{i}.test"), 1), 60);
        }
        let first = query("0.test", 1);
        insert(&cache, &first, 300);
        assert_eq!(cache.entries.len(), 4);
        for i in 0..4 {
            assert!(
                cache
                    .get(&query(&format!("{i}.test"), 2))
                    .unwrap()
                    .is_some()
            );
        }
        let reply = cache.get(&first).unwrap().unwrap();
        assert_eq!(Packet::parse(&reply).unwrap().answers[0].ttl, 300);
        for i in 4..32 {
            insert(&cache, &query(&format!("{i}.test"), 1), 60);
            assert!(cache.entries.len() <= 4);
        }
    }
}
