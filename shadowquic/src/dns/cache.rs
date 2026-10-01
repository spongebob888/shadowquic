use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use simple_dns::{Packet, PacketFlag, RCODE, rdata::RData};

use super::{Result, dns_error};

const CAPACITY: usize = 4096;

struct Entry {
    packet: Vec<u8>,
    inserted: Instant,
    expires: Instant,
}

/// Shared by all DNS services. Resolver identities partition wire responses so
/// split DNS and synthetic answers never contaminate another resolver's cache.
#[derive(Default)]
pub struct DnsCache {
    entries: Mutex<HashMap<(u64, Vec<u8>), Entry>>,
}

pub fn global_cache() -> &'static DnsCache {
    static CACHE: OnceLock<DnsCache> = OnceLock::new();
    CACHE.get_or_init(DnsCache::default)
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
    pub(super) fn get(&self, resolver: u64, query: &[u8]) -> Result<Option<Vec<u8>>> {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|_, e| e.expires > now);
        let Some(entry) = entries.get(&(resolver, key(query))) else {
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

    pub(super) fn insert(&self, resolver: u64, query: &[u8], packet: Packet<'_>) {
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
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|_, e| e.expires > now);
        if entries.len() >= CAPACITY
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.inserted)
                .map(|(k, _)| k.clone())
        {
            entries.remove(&oldest);
        }
        let Ok(packet) = packet.build_bytes_vec() else {
            return;
        };
        entries.insert(
            (resolver, key(query)),
            Entry {
                packet,
                inserted: now,
                expires: now + Duration::from_secs(ttl.into()),
            },
        );
    }

    /// Cached addresses only; never performs network I/O. Names are case insensitive.
    pub fn lookup_cache(&self, domain: &str) -> Vec<IpAddr> {
        let domain = domain.trim_end_matches('.');
        let now = Instant::now();
        let entries = self.entries.lock().unwrap();
        let mut result = Vec::new();
        for entry in entries.values().filter(|e| e.expires > now) {
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

    /// Returns the most recently cached name associated with this address.
    pub fn reverse_lookup(&self, ip: IpAddr) -> Option<String> {
        let now = Instant::now();
        self.entries
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.expires > now)
            .filter_map(|e| {
                let packet = Packet::parse(&e.packet).ok()?;
                addresses(&packet)
                    .contains(&ip)
                    .then(|| (e.inserted, packet.questions[0].qname.to_string()))
            })
            .max_by_key(|(inserted, _)| *inserted)
            .map(|(_, name)| name)
    }
}

#[cfg(test)]
impl DnsCache {
    pub(super) fn age_for_test(&self, duration: Duration) {
        for entry in self.entries.lock().unwrap().values_mut() {
            entry.inserted -= duration;
            entry.expires -= duration;
        }
    }
}
