use std::{collections::HashMap, net::IpAddr};

#[derive(Clone, Debug)]
pub(super) struct Lease {
    pub expiry: Option<u64>,
    pub mac: Option<String>,
    pub host: Option<Vec<u8>>,
    pub duid: Option<String>,
    pub iaid: Option<u32>,
}

#[derive(Default, Debug)]
pub(super) struct Snapshot(pub HashMap<IpAddr, Vec<Lease>>);

impl Snapshot {
    pub fn lookup(&self, ip: IpAddr, now: u64) -> Option<&Lease> {
        self.0
            .get(&ip)?
            .iter()
            .rev()
            .find(|lease| lease.expiry.is_none_or(|expiry| expiry > now))
    }
}

type ParseResult<T> = Result<T, String>;

fn number(s: &str, radix: u32) -> ParseResult<u64> {
    if s.is_empty() || !s.chars().all(|c| c.is_digit(radix)) {
        return Err(format!("invalid base-{radix} integer"));
    }
    u64::from_str_radix(s, radix).map_err(|_| "integer out of range".into())
}

fn bytes(s: &str, separated: bool) -> ParseResult<Vec<u8>> {
    let parts: Vec<&str> = if separated {
        s.split(':').collect()
    } else {
        if !s.is_ascii() || !s.len().is_multiple_of(2) {
            return Err("invalid hex bytes".into());
        }
        (0..s.len()).step_by(2).map(|i| &s[i..i + 2]).collect()
    };
    if parts.is_empty() {
        return Err("empty hex bytes".into());
    }
    parts
        .into_iter()
        .map(|p| {
            if p.is_empty() || p.len() > 2 {
                return Err("invalid hex byte".into());
            }
            Ok(number(p, 16)? as u8)
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn mac(s: &str) -> ParseResult<Option<String>> {
    if s == "*" {
        return Ok(None);
    }
    let (hardware, addr) = match s.split_once('-') {
        Some((kind, addr)) => (number(kind, 16)?, addr),
        None => (1, s),
    };
    if addr.is_empty() {
        return Ok(None);
    }
    let addr = bytes(addr, true)?;
    Ok((hardware == 1 && addr.len() == 6).then(|| hex(&addr)))
}

fn duid_mac(duid: &[u8]) -> Option<String> {
    let offset = match duid {
        [0, 1, 0, 1, ..] if duid.len() == 14 => 8,
        [0, 3, 0, 1, ..] if duid.len() == 10 => 4,
        _ => return None,
    };
    Some(hex(&duid[offset..]))
}

fn hostname(s: &str, odhcp: bool) -> ParseResult<Option<Vec<u8>>> {
    if s == if odhcp { "-" } else { "*" } || (odhcp && s.starts_with("broken\\x20")) {
        return Ok(None);
    }
    if !odhcp {
        return Ok(Some(s.as_bytes().to_vec()));
    }
    let mut result = Vec::new();
    let mut input = s.as_bytes();
    while !input.is_empty() {
        if input[0] == b'\\' {
            if input.len() < 4 || input[1] != b'x' {
                return Err("invalid hostname escape".into());
            }
            let digits =
                std::str::from_utf8(&input[2..4]).map_err(|_| "invalid hostname escape")?;
            result.push(number(digits, 16)? as u8);
            input = &input[4..];
        } else {
            result.push(input[0]);
            input = &input[1..];
        }
    }
    Ok(Some(result))
}

fn expiry(s: &str, odhcp: bool) -> ParseResult<Option<u64>> {
    if s == if odhcp { "-1" } else { "0" } {
        return Ok(None);
    }
    Ok(Some(number(s, 10)?))
}

fn iaid(s: &str, radix: u32) -> ParseResult<u32> {
    number(s, radix)?
        .try_into()
        .map_err(|_| "IAID out of range".into())
}

fn ip(s: &str) -> ParseResult<IpAddr> {
    s.parse().map_err(|_| "invalid IP address".into())
}

fn dnsmasq(fields: &[&str]) -> ParseResult<Vec<(IpAddr, Lease)>> {
    if matches!(fields[0], "duid" | "vendorclass" | "agent-info") {
        return Ok(Vec::new());
    }
    if fields.len() != 5 {
        return Err("expected five dnsmasq fields".into());
    }
    let addr = ip(fields[2])?;
    let mut lease = Lease {
        expiry: expiry(fields[0], false)?,
        host: hostname(fields[3], false)?,
        mac: None,
        duid: None,
        iaid: None,
    };
    if addr.is_ipv4() {
        lease.mac = mac(fields[1])?;
        if fields[4] != "*" {
            bytes(fields[4], true)?;
        }
    } else {
        lease.iaid = Some(iaid(fields[1].strip_prefix('T').unwrap_or(fields[1]), 10)?);
        if fields[4] != "*" {
            let duid = bytes(fields[4], true)?;
            lease.mac = duid_mac(&duid);
            lease.duid = Some(hex(&duid));
        }
    }
    Ok(vec![(addr, lease)])
}

fn odhcp(fields: &[&str]) -> ParseResult<Vec<(IpAddr, Lease)>> {
    if fields[0] != "#" {
        return Ok(Vec::new());
    }
    let fields = &fields[1..];
    if fields.len() < 8 && fields.get(2) != Some(&"ipv4") {
        return Ok(Vec::new());
    }
    if fields.len() < 7 {
        return Err("incomplete odhcpd record".into());
    }
    let v4 = fields[2] == "ipv4";
    let mut lease = Lease {
        expiry: expiry(fields[4], true)?,
        host: hostname(fields[3], true)?,
        mac: None,
        duid: None,
        iaid: None,
    };
    number(fields[5], 16)?;
    let prefix = number(fields[6], 10)?;
    if v4 {
        if prefix != 32 || fields.len() != 8 {
            return Err("invalid IPv4 lease layout".into());
        }
        lease.mac = mac(fields[1])?;
    } else {
        if prefix > 128 {
            return Err("invalid IPv6 prefix length".into());
        }
        lease.iaid = Some(iaid(fields[2], 16)?);
        let duid = bytes(fields[1], false)?;
        lease.mac = duid_mac(&duid);
        lease.duid = Some(hex(&duid));
    }
    let mut result = Vec::new();
    for address in &fields[7..] {
        let (addr, length) = address
            .split_once('/')
            .ok_or("missing address prefix length")?;
        let addr = ip(addr)?;
        let length = number(length, 10)?;
        if addr.is_ipv4() != v4 || length > if v4 { 32 } else { 128 } {
            return Err("invalid address family or prefix length".into());
        }
        if v4 && length != 32 || !v4 && prefix == 128 && length != 128 {
            return Err("host lease requires a host prefix".into());
        }
        if v4 || prefix == 128 {
            result.push((addr, lease.clone()));
        }
    }
    Ok(result)
}

pub(super) fn parse(source: &str, is_odhcp: bool) -> ParseResult<Snapshot> {
    let mut snapshot = Snapshot::default();
    for (line, text) in source.lines().enumerate() {
        let text = text.trim();
        if text.is_empty() || !is_odhcp && text.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = text.split_whitespace().collect();
        let records = if is_odhcp {
            odhcp(&fields)
        } else {
            dnsmasq(&fields)
        }
        .map_err(|error| format!("line {}: {error}", line + 1))?;
        for (ip, lease) in records {
            snapshot.0.entry(ip).or_default().push(lease);
        }
    }
    Ok(snapshot)
}
