use super::{ParseState, ProtocolError, SniffProtocol, SniffResult, normalize_host};

fn invalid(reason: &'static str) -> ParseState {
    ParseState::Done(Err(ProtocolError::Invalid {
        protocol: SniffProtocol::Http,
        reason,
    }))
}

pub(super) fn parse(wire: &[u8]) -> ParseState {
    // Reject binary and non-HTTP streams without waiting for the deadline.
    if wire.first().is_some_and(|b| !is_token(*b)) {
        return ParseState::Done(Err(ProtocolError::NoMatch));
    }
    let Some(line_end) = wire.windows(2).position(|w| w == b"\r\n") else {
        return ParseState::NeedMore;
    };
    let Some(version) = request_version(&wire[..line_end]) else {
        return ParseState::Done(Err(ProtocolError::NoMatch));
    };
    let Some(end) = wire.windows(4).position(|w| w == b"\r\n\r\n") else {
        return ParseState::NeedMore;
    };
    if end < line_end {
        return invalid("invalid header boundary");
    }
    let mut host = None;
    let mut seen_host = false;
    for line in wire[line_end + 2..end + 2].split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Some(line) = line.strip_suffix(b"\r") else {
            return invalid("invalid header line ending");
        };
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            return invalid("missing header colon");
        };
        if colon == 0
            || !line[..colon]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b))
        {
            return invalid("invalid header name");
        }
        if line[colon + 1..]
            .iter()
            .any(|b| (*b < 32 && *b != b'\t') || *b == 127)
        {
            return invalid("invalid header value");
        }
        if line[..colon].eq_ignore_ascii_case(b"host") {
            if seen_host {
                return invalid("duplicate Host header");
            }
            seen_host = true;
            let Ok(value) = std::str::from_utf8(&line[colon + 1..]) else {
                return invalid("invalid Host encoding");
            };
            let value = value.trim_matches([' ', '\t']);
            if let Some(literal) = value.strip_prefix('[') {
                let Some((address, suffix)) = literal.split_once(']') else {
                    return invalid("invalid Host IP literal");
                };
                if address.parse::<std::net::Ipv6Addr>().is_err() {
                    return invalid("invalid Host IP literal");
                }
                if !suffix.is_empty() && !suffix.strip_prefix(':').is_some_and(valid_port) {
                    return invalid("invalid Host port");
                }
                continue;
            }
            let domain = if let Some((domain, port)) = value.split_once(':') {
                if !valid_port(port) {
                    return invalid("invalid Host port");
                }
                domain
            } else {
                value
            };
            host = normalize_host(domain.strip_suffix('.').unwrap_or(domain).as_bytes());
        }
    }
    ParseState::Done(
        host.map(|domain| SniffResult {
            domain,
            protocol: SniffProtocol::Http,
            version,
        })
        .ok_or(ProtocolError::NoHostname),
    )
}

fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn valid_port(port: &str) -> bool {
    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok()
}

#[derive(Default)]
pub(super) struct Parser {
    scanned: usize,
    line_checked: bool,
}
impl Parser {
    pub(super) fn matched(&self) -> bool {
        self.line_checked
    }

    pub(super) fn parse(&mut self, wire: &[u8]) -> ParseState {
        if wire.first().is_some_and(|b| !is_token(*b)) {
            return ParseState::Done(Err(ProtocolError::NoMatch));
        }
        if !self.line_checked
            && let Some(end) = wire[self.scanned.saturating_sub(1)..]
                .windows(2)
                .position(|w| w == b"\r\n")
        {
            let end = self.scanned.saturating_sub(1) + end;
            if request_version(&wire[..end]).is_none() {
                return ParseState::Done(Err(ProtocolError::NoMatch));
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
            ParseState::NeedMore
        }
    }
}

fn request_version(line: &[u8]) -> Option<&'static str> {
    let mut parts = std::str::from_utf8(line).ok()?.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if parts.next().is_some()
        || method.is_empty()
        || !method.bytes().all(is_token)
        || target.is_empty()
        || target.bytes().any(|b| b <= 32 || b == 127)
    {
        return None;
    }
    match version {
        "HTTP/1.0" => Some("1.0"),
        "HTTP/1.1" => Some("1.1"),
        _ => None,
    }
}
