//! Decoders for the v2ray/xray binary `geosite.dat` and `geoip.dat` files.
//!
//! `geosite.dat` is a protobuf `GeoSiteList`:
//!
//! ```proto
//! message Domain {
//!   enum Type { Plain = 0; Regex = 1; Domain = 2; Full = 3; }
//!   message Attribute { string key = 1; oneof { bool bool_value = 2; int64 int_value = 3; } }
//!   Type type = 1;
//!   string value = 2;
//!   repeated Attribute attribute = 3;
//! }
//! message GeoSite { string country_code = 1; repeated Domain domain = 2; }
//! message GeoSiteList { repeated GeoSite entry = 1; }
//! ```
//!
//! Rules are converted to the same `kind:value:@attr,@attr` text as the plain
//! YAML export, so both sources share one import path.
//!
//! `geoip.dat` is a protobuf `GeoIPList`:
//!
//! ```proto
//! message CIDR { bytes ip = 1; uint32 prefix = 2; }
//! message GeoIP { string country_code = 1; repeated CIDR cidr = 2; bool reverse_match = 3; }
//! message GeoIPList { repeated GeoIP entry = 1; }
//! ```
use super::Result;
use super::geosite::{Geosite, SiteList};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const VARINT: u8 = 0;
const I64: u8 = 1;
const LEN: u8 = 2;
const I32: u8 = 5;

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.buf.split_first().ok_or("truncated varint")?;
            self.buf = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("varint is too long".into())
    }

    fn advance(&mut self, len: usize) -> Result<&'a [u8]> {
        if len > self.buf.len() {
            return Err("truncated field".into());
        }
        let (field, rest) = self.buf.split_at(len);
        self.buf = rest;
        Ok(field)
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = usize::try_from(self.varint()?)?;
        self.advance(len)
    }

    fn string(&mut self, wire: u8) -> Result<&'a str> {
        expect(wire, LEN)?;
        Ok(std::str::from_utf8(self.bytes()?)?)
    }

    /// The next field number and wire type, or `None` at the end of the message.
    fn field(&mut self) -> Result<Option<(u64, u8)>> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        if key >> 3 == 0 {
            return Err("invalid field number 0".into());
        }
        Ok(Some((key >> 3, (key & 7) as u8)))
    }

    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            VARINT => {
                self.varint()?;
            }
            I64 => {
                self.advance(8)?;
            }
            LEN => {
                self.bytes()?;
            }
            I32 => {
                self.advance(4)?;
            }
            _ => return Err(format!("unsupported wire type {wire}").into()),
        }
        Ok(())
    }
}

fn expect(wire: u8, expected: u8) -> Result<()> {
    if wire != expected {
        return Err(format!("unexpected wire type {wire}, expected {expected}").into());
    }
    Ok(())
}

fn attribute(buf: &[u8]) -> Result<Option<&str>> {
    let mut reader = Reader::new(buf);
    let mut key = None;
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => key = Some(reader.string(wire)?),
            _ => reader.skip(wire)?,
        }
    }
    Ok(key.filter(|key| !key.is_empty()))
}

fn domain(buf: &[u8]) -> Result<String> {
    let mut reader = Reader::new(buf);
    let mut kind = 0;
    let mut value = "";
    let mut attributes = Vec::new();
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => {
                expect(wire, VARINT)?;
                kind = reader.varint()?;
            }
            2 => value = reader.string(wire)?,
            3 => {
                expect(wire, LEN)?;
                attributes.extend(attribute(reader.bytes()?)?);
            }
            _ => reader.skip(wire)?,
        }
    }
    let kind = match kind {
        0 => "keyword",
        1 => "regexp",
        2 => "domain",
        3 => "full",
        _ => return Err(format!("unsupported geosite.dat domain type {kind}").into()),
    };
    let mut rule = format!("{kind}:{value}");
    let mut separator = ":@";
    for attribute in attributes {
        rule.push_str(separator);
        rule.push_str(attribute);
        separator = ",@";
    }
    Ok(rule)
}

fn site(buf: &[u8]) -> Result<SiteList> {
    let mut reader = Reader::new(buf);
    let mut name = String::new();
    let mut rules = Vec::new();
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => name = reader.string(wire)?.to_owned(),
            2 => {
                expect(wire, LEN)?;
                rules.push(domain(reader.bytes()?)?);
            }
            _ => reader.skip(wire)?,
        }
    }
    Ok(SiteList { name, rules })
}

/// Decode a complete `GeoSiteList`. Unknown fields are skipped.
pub(super) fn decode_geosite(buf: &[u8]) -> Result<Geosite> {
    let mut reader = Reader::new(buf);
    let mut lists = Vec::new();
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => {
                expect(wire, LEN)?;
                lists.push(site(reader.bytes()?)?);
            }
            _ => reader.skip(wire)?,
        }
    }
    Ok(Geosite { lists })
}

/// One `GeoIP` entry: a label and its networks as inclusive address ranges.
pub(super) struct GeoIp {
    pub(super) code: String,
    pub(super) ranges: Vec<(IpAddr, IpAddr)>,
}

fn cidr(buf: &[u8]) -> Result<(IpAddr, IpAddr)> {
    let mut reader = Reader::new(buf);
    let mut ip: &[u8] = &[];
    let mut prefix = 0u64;
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => {
                expect(wire, LEN)?;
                ip = reader.bytes()?;
            }
            2 => {
                expect(wire, VARINT)?;
                prefix = reader.varint()?;
            }
            _ => reader.skip(wire)?,
        }
    }
    let range = match ip.len() {
        4 => {
            if prefix > 32 {
                return Err(format!("invalid IPv4 prefix /{prefix}").into());
            }
            let bits = u32::from_be_bytes(ip.try_into()?);
            let host = u32::MAX.checked_shr(prefix as u32).unwrap_or(0);
            (
                IpAddr::V4(Ipv4Addr::from(bits & !host)),
                IpAddr::V4(Ipv4Addr::from(bits | host)),
            )
        }
        16 => {
            if prefix > 128 {
                return Err(format!("invalid IPv6 prefix /{prefix}").into());
            }
            let bits = u128::from_be_bytes(ip.try_into()?);
            let host = u128::MAX.checked_shr(prefix as u32).unwrap_or(0);
            (
                IpAddr::V6(Ipv6Addr::from(bits & !host)),
                IpAddr::V6(Ipv6Addr::from(bits | host)),
            )
        }
        len => return Err(format!("invalid geoip.dat address length {len}").into()),
    };
    Ok(range)
}

fn geoip(buf: &[u8]) -> Result<Option<GeoIp>> {
    let mut reader = Reader::new(buf);
    let mut code = String::new();
    let mut ranges = Vec::new();
    let mut reverse = false;
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => code = reader.string(wire)?.to_owned(),
            2 => {
                expect(wire, LEN)?;
                ranges.push(cidr(reader.bytes()?)?);
            }
            3 => {
                expect(wire, VARINT)?;
                reverse = reader.varint()? != 0;
            }
            _ => reader.skip(wire)?,
        }
    }
    if reverse {
        // Inverted lists match everything outside their networks, which
        // range tables cannot express. Published files do not use them.
        tracing::warn!(event = "database:geoip_reverse_skipped", code = %code, "skipping reverse-match geoip.dat entry");
        return Ok(None);
    }
    Ok(Some(GeoIp { code, ranges }))
}

/// Decode a complete `GeoIPList`. Unknown fields are skipped.
pub(super) fn decode_geoip(buf: &[u8]) -> Result<Vec<GeoIp>> {
    let mut reader = Reader::new(buf);
    let mut entries = Vec::new();
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => {
                expect(wire, LEN)?;
                entries.extend(geoip(reader.bytes()?)?);
            }
            _ => reader.skip(wire)?,
        }
    }
    Ok(entries)
}

/// A test domain: `(type, value, [attribute])`.
#[cfg(test)]
pub(crate) type TestDomain<'a> = (u64, &'a str, &'a [&'a str]);

/// Encode a `GeoSiteList` for tests: `(list, [domain])`.
#[cfg(test)]
pub(crate) fn encode(lists: &[(&str, &[TestDomain])]) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push(value as u8 | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }
    fn bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
        varint(out, field << 3 | u64::from(LEN));
        varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }
    let mut out = Vec::new();
    for (name, domains) in lists {
        let mut site = Vec::new();
        bytes(&mut site, 1, name.as_bytes());
        for (kind, value, attributes) in *domains {
            let mut domain = Vec::new();
            varint(&mut domain, 1 << 3 | u64::from(VARINT));
            varint(&mut domain, *kind);
            bytes(&mut domain, 2, value.as_bytes());
            for attribute in *attributes {
                let mut encoded = Vec::new();
                bytes(&mut encoded, 1, attribute.as_bytes());
                // bool_value = true
                encoded.extend_from_slice(&[2 << 3 | VARINT, 1]);
                bytes(&mut domain, 3, &encoded);
            }
            bytes(&mut site, 2, &domain);
        }
        bytes(&mut out, 1, &site);
    }
    out
}

/// A test geoip entry: `(code, [(address bytes, prefix)], reverse_match)`.
#[cfg(test)]
pub(crate) type TestGeoIp<'a> = (&'a str, &'a [(&'a [u8], u64)], bool);

/// Encode a `GeoIPList` for tests.
#[cfg(test)]
pub(crate) fn encode_geoip(entries: &[TestGeoIp]) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push(value as u8 | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }
    fn bytes(out: &mut Vec<u8>, field: u64, value: &[u8]) {
        varint(out, field << 3 | u64::from(LEN));
        varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }
    let mut out = Vec::new();
    for (code, cidrs, reverse) in entries {
        let mut entry = Vec::new();
        bytes(&mut entry, 1, code.as_bytes());
        for (ip, prefix) in *cidrs {
            let mut cidr = Vec::new();
            bytes(&mut cidr, 1, ip);
            varint(&mut cidr, 2 << 3 | u64::from(VARINT));
            varint(&mut cidr, *prefix);
            bytes(&mut entry, 2, &cidr);
        }
        if *reverse {
            entry.extend_from_slice(&[3 << 3 | VARINT, 1]);
        }
        bytes(&mut out, 1, &entry);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_types_attributes_and_skips_unknown_fields() {
        let mut buf = encode(&[(
            "CN",
            &[
                (0, "needle", &[]),
                (1, r"^rx[0-9]+\.example$", &[]),
                (2, "tree.example", &["ads", "cn"]),
                (3, "exact.example", &[]),
            ],
        )]);
        // Unknown top-level fields of every wire type.
        buf.extend_from_slice(&[2 << 3 | VARINT, 0x96, 0x01]);
        buf.extend_from_slice(&[3 << 3 | I64, 0, 0, 0, 0, 0, 0, 0, 0]);
        buf.extend_from_slice(&[4 << 3 | LEN, 1, b'x']);
        buf.extend_from_slice(&[5 << 3 | I32, 0, 0, 0, 0]);
        let data = decode_geosite(&buf).unwrap();
        assert_eq!(data.lists.len(), 1);
        assert_eq!(data.lists[0].name, "CN");
        assert_eq!(
            data.lists[0].rules,
            [
                "keyword:needle",
                r"regexp:^rx[0-9]+\.example$",
                "domain:tree.example:@ads,@cn",
                "full:exact.example",
            ]
        );
    }

    #[test]
    fn rejects_malformed_input() {
        let valid = encode(&[("CN", &[(3, "exact.example", &[])])]);
        for invalid in [
            &valid[..valid.len() - 1],
            &[0x0a, 0x80][..],
            &[0x00, 0x00][..],
            &[1 << 3 | 7][..],
            &[1 << 3 | VARINT, 1][..],
        ] {
            assert!(decode_geosite(invalid).is_err(), "{invalid:?}");
        }
        let unknown_type = encode(&[("CN", &[(9, "x", &[])])]);
        assert!(decode_geosite(&unknown_type).is_err());
    }

    #[test]
    fn decodes_geoip_ranges_and_skips_reverse_entries() {
        let buf = encode_geoip(&[
            (
                "CN",
                &[
                    (&[1, 0, 1, 7][..], 24),
                    (&[10, 0, 0, 0][..], 8),
                    (&[0, 0, 0, 0][..], 0),
                    (
                        &[0x24, 0x0e, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1][..],
                        32,
                    ),
                ],
                false,
            ),
            ("REVERSED", &[(&[1, 1, 1, 1][..], 32)], true),
        ]);
        let entries = decode_geoip(&buf).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].code, "CN");
        let parse = |a: &str, b: &str| (a.parse().unwrap(), b.parse().unwrap());
        assert_eq!(
            entries[0].ranges,
            [
                parse("1.0.1.0", "1.0.1.255"),
                parse("10.0.0.0", "10.255.255.255"),
                parse("0.0.0.0", "255.255.255.255"),
                parse("240e::", "240e:0:ffff:ffff:ffff:ffff:ffff:ffff"),
            ]
        );
        for invalid in [
            encode_geoip(&[("CN", &[(&[1, 2, 3][..], 8)], false)]),
            encode_geoip(&[("CN", &[(&[1, 2, 3, 4][..], 33)], false)]),
            encode_geoip(&[("CN", &[(&[0; 16][..], 129)], false)]),
        ] {
            assert!(decode_geoip(&invalid).is_err());
        }
    }
}
