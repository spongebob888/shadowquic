use super::{Parse, SniffResult, normalize_host};

pub(super) fn parse(wire: &[u8]) -> Parse {
    // Reject binary and non-HTTP streams without waiting for the deadline.
    if wire.first().is_some_and(|b| !is_token(*b)) {
        return Parse::Done(None);
    }
    let Some(line_end) = wire.windows(2).position(|w| w == b"\r\n") else {
        return Parse::NeedMore;
    };
    let Some(version) = request_version(&wire[..line_end]) else {
        return Parse::Done(None);
    };
    let Some(end) = wire.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Parse::NeedMore;
    };
    if end < line_end {
        return Parse::Done(None);
    }
    let mut host = None;
    for line in wire[line_end + 2..end + 2].split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Some(line) = line.strip_suffix(b"\r") else {
            return Parse::Done(None);
        };
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            return Parse::Done(None);
        };
        if colon == 0
            || !line[..colon]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b))
        {
            return Parse::Done(None);
        }
        if line[colon + 1..]
            .iter()
            .any(|b| (*b < 32 && *b != b'\t') || *b == 127)
        {
            return Parse::Done(None);
        }
        if line[..colon].eq_ignore_ascii_case(b"host") {
            if host.is_some() {
                return Parse::Done(None);
            }
            let Ok(value) = std::str::from_utf8(&line[colon + 1..]) else {
                return Parse::Done(None);
            };
            let value = value.trim_matches([' ', '\t']);
            let domain = if let Some((domain, port)) = value.split_once(':') {
                if port.is_empty()
                    || !port.bytes().all(|b| b.is_ascii_digit())
                    || port.parse::<u16>().is_err()
                {
                    return Parse::Done(None);
                }
                domain
            } else {
                value
            };
            host = normalize_host(domain.strip_suffix('.').unwrap_or(domain).as_bytes());
            if host.is_none() {
                return Parse::Done(None);
            }
        }
    }
    Parse::Done(host.map(|domain| SniffResult {
        domain,
        protocol: "http",
        version: version.into(),
    }))
}

fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

#[derive(Default)]
pub(super) struct Parser {
    scanned: usize,
    line_checked: bool,
}
impl Parser {
    pub(super) fn parse(&mut self, wire: &[u8]) -> Parse {
        if wire.first().is_some_and(|b| !is_token(*b)) {
            return Parse::Done(None);
        }
        if !self.line_checked
            && let Some(end) = wire[self.scanned.saturating_sub(1)..]
                .windows(2)
                .position(|w| w == b"\r\n")
        {
            let end = self.scanned.saturating_sub(1) + end;
            if request_version(&wire[..end]).is_none() {
                return Parse::Done(None);
            }
            self.line_checked = true;
        }
        let complete = wire[self.scanned.saturating_sub(3)..]
            .windows(4)
            .any(|w| w == b"\r\n\r\n");
        self.scanned = wire.len();
        if complete {
            parse(wire)
        } else {
            Parse::NeedMore
        }
    }
}

fn request_version(line: &[u8]) -> Option<&str> {
    let mut parts = std::str::from_utf8(line).ok()?.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some()
        || method.is_empty()
        || !method.bytes().all(is_token)
        || target.is_empty()
        || target.bytes().any(|b| b <= 32 || b == 127)
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
    {
        return None;
    }
    Some(&version[5..])
}
