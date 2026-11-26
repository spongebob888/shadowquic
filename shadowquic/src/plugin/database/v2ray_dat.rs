//! Decoder for the v2ray/xray binary `geosite.dat` file.
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
use super::Result;
#[cfg(test)]
use super::geosite::{Geosite, SiteList};

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

pub(super) fn domain(buf: &[u8]) -> Result<String> {
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

#[cfg(test)]
fn decode_geosite(buf: &[u8]) -> Result<Geosite> {
    let mut reader = super::dat_reader::DatReader::new(std::io::Cursor::new(buf))?;
    let mut lists = Vec::new();
    while let Some((name, rules)) = reader.next_site()? {
        lists.push(SiteList {
            name,
            rules: rules.collect::<Result<_>>()?,
        });
    }
    Ok(Geosite { lists })
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
    fn streams_rules_before_the_list_name_and_reports_late_errors() {
        // The first domain is valid, the second has an unknown type. Field
        // three is an optional length-delimited resource hash in GeoSite.
        let entry = [
            0x12, 3, 0x12, 1, b'x', 0x12, 2, 0x08, 9, 0x1a, 1, 7, 0x0a, 2, b'C', b'N',
        ];
        let data = [vec![0x0a, entry.len() as u8], entry.to_vec()].concat();
        let mut reader =
            super::super::dat_reader::DatReader::new(std::io::Cursor::new(data)).unwrap();
        let (name, mut rules) = reader.next_site().unwrap().unwrap();
        assert_eq!(name, "CN");
        assert_eq!(rules.next().unwrap().unwrap(), "keyword:x");
        assert!(rules.next().unwrap().is_err());
        assert!(rules.next().is_none());
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
}
