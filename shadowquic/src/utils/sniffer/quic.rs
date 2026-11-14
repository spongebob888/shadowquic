//! RFC 9001 Initial protection; RFC 9369 QUIC v2. Bounded ClientHello assembly across datagrams.
use super::{Parse, SniffProtocol, SniffResult, SnifferError, tls};
use rustls_jls::quic::{Suite, Version};

pub(super) fn suite() -> Option<Suite> {
    let provider = rustls_jls::crypto::CryptoProvider::get_default()
        .cloned()
        .or_else(|| {
            #[cfg(feature = "ring")]
            {
                Some(std::sync::Arc::new(
                    rustls_jls::crypto::ring::default_provider(),
                ))
            }
            #[cfg(all(not(feature = "ring"), feature = "aws-lc-rs"))]
            {
                Some(std::sync::Arc::new(
                    rustls_jls::crypto::aws_lc_rs::default_provider(),
                ))
            }
            #[cfg(not(any(feature = "ring", feature = "aws-lc-rs")))]
            {
                None
            }
        })?;
    provider
        .cipher_suites
        .iter()
        .find(|s| s.suite() == rustls_jls::CipherSuite::TLS13_AES_128_GCM_SHA256)?
        .tls13()?
        .quic_suite()
}

struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (a, b) = self.0.split_at_checked(n)?;
        self.0 = b;
        Some(a)
    }
    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn varint(&mut self) -> Option<u64> {
        let first = *self.0.first()?;
        let bytes = self.take(1 << (first >> 6))?;
        Some(
            bytes[1..]
                .iter()
                .fold((first & 63) as u64, |n, b| (n << 8) | u64::from(*b)),
        )
    }
    fn length(&mut self) -> Option<usize> {
        usize::try_from(self.varint()?).ok()
    }
}

#[cfg(test)]
pub(super) fn parse(datagram: &[u8], limit: usize, suite: &Suite) -> Option<SniffResult> {
    match Parser::default().parse(datagram, limit, suite) {
        Parse::Done(result) => result,
        Parse::NeedMore | Parse::Error(_) => None,
    }
}

#[derive(Default)]
pub(super) struct Parser {
    crypto: Vec<u8>,
    present: Vec<bool>,
    identity: Option<(u32, Vec<u8>)>,
    expected_pn: u64,
    wire_bytes: usize,
    packets: usize,
}
impl Parser {
    pub(super) fn parse(&mut self, datagram: &[u8], limit: usize, suite: &Suite) -> Parse {
        self.parse_inner(datagram, limit, suite)
            .unwrap_or(Parse::Done(None))
    }
    fn parse_inner(&mut self, mut datagram: &[u8], limit: usize, suite: &Suite) -> Option<Parse> {
        self.wire_bytes = self.wire_bytes.checked_add(datagram.len())?;
        if datagram.is_empty() {
            return None;
        }
        if self.wire_bytes > limit {
            return Some(Parse::Error(SnifferError::Limit));
        }
        // Bound decryption work even for adversarial coalesced packets.
        while !datagram.is_empty() {
            self.packets += 1;
            if self.packets > 32 {
                return Some(Parse::Error(SnifferError::Limit));
            }
            let mut c = Cursor(datagram);
            let first = c.byte()?;
            if first & 0xc0 != 0xc0 {
                return None;
            }
            let wire_version = u32::from_be_bytes(c.take(4)?.try_into().ok()?);
            let (version, label, initial_type) = match wire_version {
                1 => (Version::V1, "1", 0),
                0x6b3343cf => (Version::V2, "2", 1),
                _ => return None,
            };
            if (first >> 4) & 3 != initial_type {
                return None;
            }
            let dcid_len = c.byte()? as usize;
            if dcid_len > 20 {
                return None;
            }
            let dcid = c.take(dcid_len)?;
            let scid_len = c.byte()? as usize;
            if scid_len > 20 {
                return None;
            }
            c.take(scid_len)?;
            let token_len = c.length()?;
            c.take(token_len)?;
            let length = c.length()?;
            let pn_offset = datagram.len() - c.0.len();
            let packet_len = pn_offset.checked_add(length)?;
            let packet = datagram.get(..packet_len)?;
            // Header plausibility checks precede all key derivation and allocations.
            let sample = packet.get(pn_offset.checked_add(4)?..pn_offset.checked_add(20)?)?;
            if let Some((v, id)) = &self.identity {
                if *v != wire_version || id != dcid {
                    return None;
                }
            } else {
                self.identity = Some((wire_version, dcid.to_vec()));
            }
            let keys = suite.keys(dcid, rustls_jls::Side::Server, version).remote;
            let mut first = first;
            let mut pn_bytes: [u8; 4] = packet.get(pn_offset..pn_offset + 4)?.try_into().ok()?;
            keys.header
                .decrypt_in_place(sample, &mut first, &mut pn_bytes)
                .ok()?;
            if first & 0x0c != 0 {
                return None;
            }
            let pn_len = (first & 3) as usize + 1;
            let truncated = pn_bytes[..pn_len]
                .iter()
                .fold(0u64, |n, b| (n << 8) | u64::from(*b));
            let window = 1u64 << (8 * pn_len);
            let mut pn = (self.expected_pn & !(window - 1)) | truncated;
            if pn + window / 2 <= self.expected_pn {
                pn += window;
            } else if pn > self.expected_pn + window / 2 && pn >= window {
                pn -= window;
            }
            let mut decrypted = packet.to_vec();
            decrypted[0] = first;
            decrypted[pn_offset..pn_offset + pn_len].copy_from_slice(&pn_bytes[..pn_len]);
            let (header, payload) = decrypted.split_at_mut(pn_offset + pn_len);
            let payload = keys.packet.decrypt_in_place(pn, header, payload).ok()?;
            self.expected_pn = self.expected_pn.max(pn + 1);
            let mut frames = Cursor(payload);
            for _ in 0..4096 {
                if frames.0.is_empty() {
                    break;
                }
                match frames.varint()? {
                    0 | 1 => {} // PADDING, PING
                    kind @ (2 | 3) => {
                        frames.varint()?;
                        frames.varint()?;
                        let ranges = frames.varint()?;
                        frames.varint()?;
                        if ranges > 4096 {
                            return Some(Parse::Error(SnifferError::Limit));
                        }
                        for _ in 0..ranges {
                            frames.varint()?;
                            frames.varint()?;
                        }
                        if kind == 3 {
                            for _ in 0..3 {
                                frames.varint()?;
                            }
                        }
                    }
                    6 => {
                        let offset = frames.length()?;
                        let len = frames.length()?;
                        let end = offset.checked_add(len)?;
                        if end > limit {
                            return Some(Parse::Error(SnifferError::Limit));
                        }
                        let data = frames.take(len)?;
                        if self.crypto.is_empty() {
                            self.crypto = vec![0; limit];
                            self.present = vec![false; limit];
                        }
                        for (i, byte) in (offset..end).zip(data) {
                            if self.present[i] && self.crypto[i] != *byte {
                                return None;
                            }
                            self.crypto[i] = *byte;
                            self.present[i] = true;
                        }
                    }
                    _ => return None,
                }
            }
            if !frames.0.is_empty() {
                return Some(Parse::Error(SnifferError::Limit));
            }
            if self.present.get(..4).is_some_and(|p| p.iter().all(|b| *b)) {
                let len = match tls::handshake_len(&self.crypto, limit) {
                    Ok(len) => len,
                    Err(result) => return Some(result),
                };
                if self.present[..len].iter().all(|b| *b) {
                    let hello = tls::client_hello(&self.crypto[4..len])?;
                    return Some(Parse::Done(Some(SniffResult {
                        domain: hello.domain,
                        protocol: SniffProtocol::Quic,
                        version: label,
                    })));
                }
            }
            datagram = &datagram[packet_len..];
            // Some clients append zero padding outside the protected Initial.
            // Only tolerate an all-zero remainder after authenticating a packet
            // in this datagram; preserve its CRYPTO fragments for the next one.
            if datagram.iter().all(|byte| *byte == 0) {
                break;
            }
        }
        Some(Parse::NeedMore)
    }
}
