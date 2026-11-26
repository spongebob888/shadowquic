//! Buffered GeoIP and GeoSite protobuf decoding. Each entry is scanned for metadata before
//! yielding its CIDRs or domains, since protobuf fields may appear in any order.
use super::Result;
use std::io::{Read, Seek, SeekFrom, Take};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

struct Message<R> {
    input: Take<R>,
}

impl<R: Read> Message<R> {
    fn new(input: R, length: u64) -> Self {
        Self {
            input: input.take(length),
        }
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let mut byte = [0];
            self.input.read_exact(&mut byte)?;
            if shift == 63 && byte[0] > 1 {
                return Err("varint is too long".into());
            }
            value |= u64::from(byte[0] & 0x7f) << shift;
            if byte[0] & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("varint is too long".into())
    }

    fn field(&mut self) -> Result<Option<(u64, u8)>> {
        if self.input.limit() == 0 {
            return Ok(None);
        }
        let key = self.varint()?;
        if key >> 3 == 0 {
            return Err("invalid field number 0".into());
        }
        Ok(Some((key >> 3, (key & 7) as u8)))
    }

    fn length(&mut self, wire: u8) -> Result<u64> {
        expect(wire, 2)?;
        let length = self.varint()?;
        if length > self.input.limit() {
            return Err("truncated field".into());
        }
        Ok(length)
    }

    fn discard(&mut self, length: u64) -> Result<()> {
        if std::io::copy(&mut self.input.by_ref().take(length), &mut std::io::sink())? != length {
            return Err("truncated field".into());
        }
        Ok(())
    }

    fn skip(&mut self, wire: u8) -> Result<()> {
        let length = match wire {
            0 => {
                self.varint()?;
                return Ok(());
            }
            1 => 8,
            2 => self.length(wire)?,
            5 => 4,
            _ => return Err(format!("unsupported wire type {wire}").into()),
        };
        self.discard(length)
    }

    fn next_domain(&mut self) -> Result<Option<String>> {
        while let Some((field, wire)) = self.field()? {
            if field == 2 {
                let length = self.length(wire)?;
                let mut bytes = Vec::new();
                if self.input.by_ref().take(length).read_to_end(&mut bytes)? as u64 != length {
                    return Err("truncated domain".into());
                }
                return super::v2ray_dat::domain(&bytes).map(Some);
            }
            self.skip(wire)?;
        }
        Ok(None)
    }

    fn string(&mut self, wire: u8) -> Result<String> {
        let length = self.length(wire)?;
        let mut value = String::new();
        if self
            .input
            .by_ref()
            .take(length)
            .read_to_string(&mut value)? as u64
            != length
        {
            return Err("truncated field".into());
        }
        Ok(value)
    }
}

fn expect(wire: u8, expected: u8) -> Result<()> {
    if wire != expected {
        return Err(format!("unexpected wire type {wire}, expected {expected}").into());
    }
    Ok(())
}

fn cidr<R: Read>(reader: &mut Message<R>) -> Result<(IpAddr, IpAddr)> {
    let mut ip = [0; 16];
    let mut length = 0;
    let mut prefix = 0;
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => {
                length = reader.length(wire)?;
                match length {
                    4 | 16 => reader.input.read_exact(&mut ip[..length as usize])?,
                    _ => reader.discard(length)?,
                }
            }
            2 => {
                expect(wire, 0)?;
                prefix = reader.varint()?;
            }
            _ => reader.skip(wire)?,
        }
    }
    match length {
        4 => {
            if prefix > 32 {
                return Err(format!("invalid IPv4 prefix /{prefix}").into());
            }
            let bits = u32::from_be_bytes(ip[..4].try_into()?);
            let host = u32::MAX.checked_shr(prefix as u32).unwrap_or(0);
            Ok((
                IpAddr::V4(Ipv4Addr::from(bits & !host)),
                IpAddr::V4(Ipv4Addr::from(bits | host)),
            ))
        }
        16 => {
            if prefix > 128 {
                return Err(format!("invalid IPv6 prefix /{prefix}").into());
            }
            let bits = u128::from_be_bytes(ip);
            let host = u128::MAX.checked_shr(prefix as u32).unwrap_or(0);
            Ok((
                IpAddr::V6(Ipv6Addr::from(bits & !host)),
                IpAddr::V6(Ipv6Addr::from(bits | host)),
            ))
        }
        _ => Err(format!("invalid geoip.dat address length {length}").into()),
    }
}

pub(super) struct GeoIp<R> {
    pub(super) code: String,
    pub(super) reverse: bool,
    pub(super) ranges: Cidrs<R>,
}

pub(super) struct Cidrs<R> {
    reader: Message<R>,
}

impl<R: Read> Cidrs<R> {
    fn next_range(&mut self) -> Result<Option<(IpAddr, IpAddr)>> {
        while let Some((field, wire)) = self.reader.field()? {
            if field == 2 {
                let length = self.reader.length(wire)?;
                let mut message = Message::new(&mut self.reader.input, length);
                return cidr(&mut message).map(Some);
            }
            self.reader.skip(wire)?;
        }
        Ok(None)
    }
}

impl<R: Read> Iterator for Cidrs<R> {
    type Item = Result<(IpAddr, IpAddr)>;

    fn next(&mut self) -> Option<Self::Item> {
        let range = self.next_range();
        if range.is_err() {
            self.reader.input.set_limit(0);
        }
        range.transpose()
    }
}

pub(super) struct DatReader<R> {
    input: R,
    next: u64,
    end: u64,
}

impl<R: Read + Seek> DatReader<R> {
    pub(super) fn new(mut input: R) -> Result<Self> {
        let end = input.seek(SeekFrom::End(0))?;
        input.rewind()?;
        Ok(Self {
            input,
            next: 0,
            end,
        })
    }

    // The CIDR iterator borrows this reader until consumed or dropped. Seeking
    // to the next entry also permits callers to stop iterating a list early.
    pub(super) fn next_entry(&mut self) -> Result<Option<GeoIp<&mut R>>> {
        Ok(self.next_list(true)?.map(|(code, reverse, reader)| GeoIp {
            code,
            reverse,
            ranges: Cidrs { reader },
        }))
    }

    pub(super) fn next_site(
        &mut self,
    ) -> Result<Option<(String, impl Iterator<Item = Result<String>> + '_)>> {
        Ok(self.next_list(false)?.map(|(name, _, mut reader)| {
            let rules = std::iter::from_fn(move || {
                let result = reader.next_domain();
                if result.is_err() {
                    reader.input.set_limit(0);
                }
                result.transpose()
            });
            (name, rules)
        }))
    }

    fn next_list(&mut self, geoip: bool) -> Result<Option<(String, bool, Message<&mut R>)>> {
        self.input.seek(SeekFrom::Start(self.next))?;
        let mut outer = Message::new(&mut self.input, self.end - self.next);
        self.next = self.end; // Stop after an error or EOF.
        while let Some((field, wire)) = outer.field()? {
            if field != 1 {
                outer.skip(wire)?;
                continue;
            }
            let length = outer.length(wire)?;
            let start = self.end - outer.input.limit();
            let mut entry = Message::new(&mut outer.input, length);
            let mut code = String::new();
            let mut reverse = false;
            while let Some((field, wire)) = entry.field()? {
                match field {
                    1 => code = entry.string(wire)?,
                    2 => {
                        expect(wire, 2)?;
                        entry.skip(wire)?;
                    }
                    3 if geoip => {
                        expect(wire, 0)?;
                        reverse = entry.varint()? != 0;
                    }
                    _ => entry.skip(wire)?,
                }
            }
            self.input.seek(SeekFrom::Start(start))?;
            self.next = start + length;
            return Ok(Some((code, reverse, Message::new(&mut self.input, length))));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::super::v2ray_dat::encode_geoip;
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn decodes_geoip_ranges_and_reverse_flag() {
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
        let mut entries = DatReader::new(Cursor::new(&buf)).unwrap();
        let entry = entries.next_entry().unwrap().unwrap();
        assert_eq!(entry.code, "CN");
        let parse = |a: &str, b: &str| (a.parse().unwrap(), b.parse().unwrap());
        assert_eq!(
            entry.ranges.collect::<Result<Vec<_>>>().unwrap(),
            [
                parse("1.0.1.0", "1.0.1.255"),
                parse("10.0.0.0", "10.255.255.255"),
                parse("0.0.0.0", "255.255.255.255"),
                parse("240e::", "240e:0:ffff:ffff:ffff:ffff:ffff:ffff"),
            ]
        );
        let reversed = entries.next_entry().unwrap().unwrap();
        assert!(reversed.reverse);
        assert_eq!(reversed.ranges.count(), 1);
        assert!(entries.next_entry().unwrap().is_none());
        for invalid in [
            encode_geoip(&[("CN", &[(&[1, 2, 3][..], 8)], false)]),
            encode_geoip(&[("CN", &[(&[1, 2, 3, 4][..], 33)], false)]),
            encode_geoip(&[("CN", &[(&[0; 16][..], 129)], false)]),
        ] {
            let mut entries = DatReader::new(Cursor::new(invalid)).unwrap();
            let mut entry = entries.next_entry().unwrap().unwrap();
            assert!(entry.ranges.next().unwrap().is_err());
            assert!(entry.ranges.next().is_none());
        }
    }

    #[test]
    fn geoip_yields_valid_entries_before_a_decode_error() {
        let mut buf = encode_geoip(&[("CN", &[], false)]);
        buf.push(0); // Invalid field number.
        buf.extend(encode_geoip(&[("US", &[], false)]));

        let mut entries = DatReader::new(Cursor::new(&buf)).unwrap();
        assert_eq!(entries.next_entry().unwrap().unwrap().code, "CN");
        assert!(entries.next_entry().is_err());
        assert!(entries.next_entry().unwrap().is_none());
    }

    #[test]
    fn metadata_after_cidrs_and_partial_iteration_preserve_entry_boundaries() {
        // CIDRs precede both the name and the inverse flag. Unknown fields at
        // each message level must be skipped within their enclosing boundary.
        let cidr = [0x0a, 4, 1, 2, 3, 4, 0x10, 32, 0x28, 7];
        let mut entry = vec![0x12, cidr.len() as u8];
        entry.extend(cidr);
        entry.extend([0x0a, 2, b'U', b'S', 0x18, 1, 0x20, 1]);
        // Singular protobuf fields use the last value.
        entry.extend([0x0a, 2, b'C', b'N', 0x18, 0]);
        let mut data = vec![0x20, 1, 0x0a, entry.len() as u8];
        data.extend(entry);
        data.extend(encode_geoip(&[("NEXT", &[(&[8, 8, 8, 8], 32)], false)]));
        let mut entries = DatReader::new(BufReader::with_capacity(3, Cursor::new(data))).unwrap();
        {
            let mut entry = entries.next_entry().unwrap().unwrap();
            assert_eq!(entry.code, "CN");
            assert!(!entry.reverse);
            let ip = "1.2.3.4".parse().unwrap();
            assert_eq!(entry.ranges.next().unwrap().unwrap(), (ip, ip));
            // Drop the iterator before it consumes the trailing metadata.
        }
        let entry = entries.next_entry().unwrap().unwrap();
        assert_eq!(entry.code, "NEXT");
        let ip = "8.8.8.8".parse().unwrap();
        assert_eq!(
            entry.ranges.collect::<Result<Vec<_>>>().unwrap(),
            [(ip, ip)]
        );
        assert!(entries.next_entry().unwrap().is_none());
    }

    #[test]
    fn cidrs_are_decoded_lazily_and_stop_after_an_error() {
        let data = encode_geoip(&[(
            "CN",
            &[
                (&[1, 2, 3, 4], 32),
                (&[5, 6, 7, 8], 33),
                (&[9, 0, 0, 1], 32),
            ],
            false,
        )]);
        let mut entries = DatReader::new(Cursor::new(data)).unwrap();
        let mut entry = entries.next_entry().unwrap().unwrap();
        assert!(entry.ranges.next().unwrap().is_ok());
        assert!(entry.ranges.next().unwrap().is_err());
        assert!(entry.ranges.next().is_none());
    }

    #[test]
    fn rejects_truncated_messages_and_overflowing_lengths() {
        for data in [
            vec![0x0a, 10, 0x0a, 2, b'C', b'N'],
            vec![0x0a, 2, 0x12, 10],
            vec![0x0a, 2, 0x25, 0], // Fixed32 crosses the entry boundary.
            [vec![0x0a], vec![0xff; 9], vec![2]].concat(),
        ] {
            let mut entries = DatReader::new(Cursor::new(data)).unwrap();
            assert!(entries.next_entry().is_err());
            assert!(entries.next_entry().unwrap().is_none());
        }
        // The IP length would consume bytes outside its CIDR message.
        let data = [0x0a, 8, 0x12, 2, 0x0a, 4, 0x0a, 2, b'C', b'N'];
        let mut entries = DatReader::new(Cursor::new(data)).unwrap();
        let mut entry = entries.next_entry().unwrap().unwrap();
        assert!(entry.ranges.next().unwrap().is_err());
    }

    #[test]
    fn large_entry_uses_bounded_reads() {
        struct BoundedReads(Cursor<Vec<u8>>);
        impl Read for BoundedReads {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                assert!(buffer.len() <= 8192, "attempted to read a whole entry");
                self.0.read(buffer)
            }
        }
        impl Seek for BoundedReads {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                self.0.seek(position)
            }
        }
        let networks = vec![(&[1, 2, 3, 4][..], 32); 100_000];
        let data = encode_geoip(&[("CN", &networks, false)]);
        let input = BufReader::new(BoundedReads(Cursor::new(data)));
        let mut entries = DatReader::new(input).unwrap();
        let entry = entries.next_entry().unwrap().unwrap();
        assert_eq!(
            entry.ranges.inspect(|range| assert!(range.is_ok())).count(),
            networks.len()
        );
        assert!(entries.next_entry().unwrap().is_none());
    }
}
