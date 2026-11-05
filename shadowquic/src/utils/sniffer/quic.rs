//! RFC 9001 Initial protection; RFC 9369 QUIC v2. No state across datagrams.
use super::{SniffResult, tls};
use rustls::quic::{Suite, Version};

pub(super) fn suite() -> Option<Suite> {
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .or_else(|| {
            #[cfg(feature = "ring")]
            {
                Some(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
            }
            #[cfg(all(not(feature = "ring"), feature = "aws-lc-rs"))]
            {
                Some(std::sync::Arc::new(
                    rustls::crypto::aws_lc_rs::default_provider(),
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
        .find(|s| s.suite() == rustls::CipherSuite::TLS13_AES_128_GCM_SHA256)?
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

pub(super) fn parse(mut datagram: &[u8], limit: usize, suite: &Suite) -> Option<SniffResult> {
    if datagram.len() > limit {
        return None;
    }
    let mut crypto = Vec::new();
    let mut present = Vec::new();
    let mut identity: Option<(u32, Vec<u8>)> = None;
    let mut expected_pn = 0u64;
    // Bound crypto work even for adversarial coalesced packets.
    for _ in 0..32 {
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
        if let Some((v, id)) = &identity {
            if *v != wire_version || id != dcid {
                return None;
            }
        } else {
            identity = Some((wire_version, dcid.to_vec()));
        }
        let keys = suite.keys(dcid, rustls::Side::Server, version).remote;
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
        let mut pn = (expected_pn & !(window - 1)) | truncated;
        if pn + window / 2 <= expected_pn {
            pn += window;
        } else if pn > expected_pn + window / 2 && pn >= window {
            pn -= window;
        }
        let mut decrypted = packet.to_vec();
        decrypted[0] = first;
        decrypted[pn_offset..pn_offset + pn_len].copy_from_slice(&pn_bytes[..pn_len]);
        let (header, payload) = decrypted.split_at_mut(pn_offset + pn_len);
        let payload = keys.packet.decrypt_in_place(pn, header, payload).ok()?;
        expected_pn = expected_pn.max(pn + 1);
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
                        return None;
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
                        return None;
                    }
                    let data = frames.take(len)?;
                    if crypto.is_empty() {
                        crypto = vec![0; limit];
                        present = vec![false; limit];
                    }
                    for (i, byte) in (offset..end).zip(data) {
                        if present[i] && crypto[i] != *byte {
                            return None;
                        }
                        crypto[i] = *byte;
                        present[i] = true;
                    }
                }
                _ => return None,
            }
        }
        if !frames.0.is_empty() {
            return None;
        }
        if present.get(..4).is_some_and(|p| p.iter().all(|b| *b)) {
            let len = tls::handshake_len(&crypto, limit)?;
            if present[..len].iter().all(|b| *b) {
                let mut result = tls::client_hello(&crypto[4..len])?;
                result.protocol = "QUIC";
                result.version = label.into();
                return Some(result);
            }
        }
        datagram = &datagram[packet_len..];
        if datagram.is_empty() {
            break;
        }
    }
    None
}
