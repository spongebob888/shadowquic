use super::SniffResult;
pub(super) enum Parse {
    NeedMore,
    Done(Option<SniffResult>),
}

#[derive(Default)]
pub(super) struct Parser {
    offset: usize,
    /// Only allocated when the handshake spans TLS records.
    fragmented: Vec<u8>,
}

impl Parser {
    pub(super) fn parse(&mut self, wire: &[u8], limit: usize) -> Parse {
        loop {
            let record = &wire[self.offset..];
            if record.first().is_some_and(|byte| *byte != 22)
                || record.get(1).is_some_and(|byte| *byte != 3)
                || record.get(2).is_some_and(|byte| !(1..=3).contains(byte))
            {
                return Parse::Done(None);
            }
            if record.len() < 5 {
                return Parse::NeedMore;
            }
            let len = usize::from(u16::from_be_bytes([record[3], record[4]]));
            if len == 0 || len > 16384 || self.offset + 5 + len > limit {
                return Parse::Done(None);
            }
            if record.len() < 5 + len {
                return Parse::NeedMore;
            }
            let payload = &record[5..5 + len];
            if self.fragmented.is_empty() && payload.len() >= 4 {
                let Some(hello_len) = handshake_len(payload, limit) else {
                    return Parse::Done(None);
                };
                if payload.len() >= hello_len {
                    return Parse::Done(client_hello(&payload[4..hello_len]));
                }
            }
            let needed = self.fragmented.len() + payload.len();
            if needed > limit {
                return Parse::Done(None);
            }
            if needed > self.fragmented.capacity() {
                let capacity = needed.max(self.fragmented.capacity() * 2).min(limit);
                self.fragmented
                    .reserve_exact(capacity - self.fragmented.len());
            }
            self.fragmented.extend_from_slice(payload);
            if self.fragmented[0] != 1 {
                return Parse::Done(None);
            }
            if self.fragmented.len() >= 4 {
                let Some(hello_len) = handshake_len(&self.fragmented, limit) else {
                    return Parse::Done(None);
                };
                if self.fragmented.len() >= hello_len {
                    return Parse::Done(client_hello(&self.fragmented[4..hello_len]));
                }
            }
            self.offset += 5 + len;
        }
    }
}

pub(super) fn handshake_len(bytes: &[u8], limit: usize) -> Option<usize> {
    let len = ((bytes[1] as usize) << 16) | ((bytes[2] as usize) << 8) | bytes[3] as usize;
    (bytes[0] == 1 && len + 4 <= limit).then_some(len + 4)
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<usize> {
        Some(self.take(1)?[0] as usize)
    }

    fn u16(&mut self) -> Option<usize> {
        let bytes = self.take(2)?;
        Some(u16::from_be_bytes([bytes[0], bytes[1]]) as usize)
    }

    fn vector16(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()?;
        self.take(n)
    }
}

/// Parse only once a complete handshake is available; borrow all intermediate fields.
pub(super) fn client_hello(bytes: &[u8]) -> Option<SniffResult> {
    let mut hello = Cursor(bytes);
    let mut version = hello.u16()?;
    if !(0x0301..=0x0303).contains(&version) {
        return None;
    }
    hello.take(32)?; // random
    let session_len = hello.u8()?;
    if session_len > 32 {
        return None;
    }
    hello.take(session_len)?;
    let ciphers = hello.vector16()?;
    if ciphers.len() < 2 || ciphers.len() % 2 != 0 {
        return None;
    }
    let compression_len = hello.u8()?;
    if compression_len == 0 {
        return None;
    }
    hello.take(compression_len)?;
    let mut extensions = Cursor(hello.vector16()?);
    if !hello.0.is_empty() {
        return None;
    }
    let mut name = None;
    let mut seen_sni = false;
    while !extensions.0.is_empty() {
        let kind = extensions.u16()?;
        let extension = extensions.vector16()?;
        if kind == 43 {
            let mut supported = Cursor(extension);
            let len = supported.u8()?;
            if len == 0 || len % 2 != 0 || supported.0.len() != len {
                return None;
            }
            while !supported.0.is_empty() {
                let v = supported.u16()?;
                if (0x0301..=0x0304).contains(&v) {
                    version = version.max(v);
                }
            }
        }
        if kind != 0 {
            continue;
        }
        if seen_sni {
            return None;
        }
        seen_sni = true;
        let mut extension = Cursor(extension);
        let mut names = Cursor(extension.vector16()?);
        if !extension.0.is_empty() || names.0.is_empty() {
            return None;
        }
        while !names.0.is_empty() {
            // Only host_name (0) has a defined encoding in ServerNameList.
            if names.u8()? != 0 || name.is_some() {
                return None;
            }
            name = Some(names.vector16()?);
        }
    }
    Some(SniffResult {
        domain: normalize_host(name?)?,
        protocol: "tls",
        version: format!("1.{}", version - 0x0301),
    })
}

pub(super) fn normalize_host(name: &[u8]) -> Option<String> {
    if name.is_empty() || name.len() > 253 {
        return None;
    }
    for label in name.split(|byte| *byte == b'.') {
        if label.is_empty()
            || label.len() > 63
            || !label[0].is_ascii_alphanumeric()
            || !label[label.len() - 1].is_ascii_alphanumeric()
            || !label
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            return None;
        }
    }
    let name = std::str::from_utf8(name).ok()?;
    if name.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    Some(name.to_ascii_lowercase())
}
