use super::{Parse, SniffProtocol, SniffResult, SnifferError, normalize_host};

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
            if len == 0 || len > 16384 {
                return Parse::Done(None);
            }
            if self.offset + 5 + len > limit {
                return Parse::Error(SnifferError::Limit);
            }
            if record.len() < 5 + len {
                return Parse::NeedMore;
            }
            let payload = &record[5..5 + len];
            if self.fragmented.is_empty() && payload.len() >= 4 {
                let hello_len = match handshake_len(payload, limit) {
                    Ok(len) => len,
                    Err(result) => return result,
                };
                if payload.len() >= hello_len {
                    return Parse::Done(client_hello(&payload[4..hello_len]).map(sniff_result));
                }
            }
            let needed = self.fragmented.len() + payload.len();
            if needed > limit {
                return Parse::Error(SnifferError::Limit);
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
                let hello_len = match handshake_len(&self.fragmented, limit) {
                    Ok(len) => len,
                    Err(result) => return result,
                };
                if self.fragmented.len() >= hello_len {
                    return Parse::Done(
                        client_hello(&self.fragmented[4..hello_len]).map(sniff_result),
                    );
                }
            }
            self.offset += 5 + len;
        }
    }
}

pub(super) fn handshake_len(bytes: &[u8], limit: usize) -> Result<usize, Parse> {
    let len = ((bytes[1] as usize) << 16) | ((bytes[2] as usize) << 8) | bytes[3] as usize;
    if bytes[0] != 1 {
        Err(Parse::Done(None))
    } else if len + 4 > limit {
        Err(Parse::Error(SnifferError::Limit))
    } else {
        Ok(len + 4)
    }
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

/// Fields of a ClientHello shared by TLS and QUIC inspection.
pub(super) struct ClientHello {
    pub(super) domain: String,
    /// Highest recognized TLS version, `"1.0"` through `"1.3"`.
    pub(super) version: &'static str,
}

fn sniff_result(hello: ClientHello) -> SniffResult {
    SniffResult {
        domain: hello.domain,
        protocol: SniffProtocol::Tls,
        version: hello.version,
    }
}

/// Parse only once a complete handshake is available; borrow all intermediate fields.
pub(super) fn client_hello(bytes: &[u8]) -> Option<ClientHello> {
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
    let mut seen_versions = false;
    while !extensions.0.is_empty() {
        let kind = extensions.u16()?;
        let extension = extensions.vector16()?;
        if kind == 43 {
            if seen_versions {
                return None;
            }
            seen_versions = true;
            let mut advertised = None;
            let mut supported = Cursor(extension);
            let len = supported.u8()?;
            if len == 0 || len % 2 != 0 || supported.0.len() != len {
                return None;
            }
            while !supported.0.is_empty() {
                let v = supported.u16()?;
                if (0x0301..=0x0304).contains(&v) {
                    advertised = Some(advertised.map_or(v, |previous: usize| previous.max(v)));
                }
            }
            version = advertised?;
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
    Some(ClientHello {
        domain: normalize_host(name?)?,
        version: ["1.0", "1.1", "1.2", "1.3"][version - 0x0301],
    })
}
