use rand::Rng;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortUnion(Vec<PortRange>);

impl PortUnion {
    pub fn min_port(&self) -> Option<u16> {
        self.0.first().map(|r| r.start)
    }

    pub fn max_port(&self) -> Option<u16> {
        self.0.last().map(|r| r.end)
    }

    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.0.iter().flat_map(|r| r.start..=r.end)
    }

    pub fn count(&self) -> usize {
        self.0.iter().map(|r| (r.end - r.start) as usize + 1).sum()
    }

    pub fn random_port<R: Rng + ?Sized>(&self, rng: &mut R) -> Option<u16> {
        let total = self.count();
        if total == 0 {
            return None;
        }

        let mut idx = rng.random_range(0..total);
        for r in &self.0 {
            let len = (r.end - r.start) as usize + 1;
            if idx < len {
                return Some(r.start + idx as u16);
            }
            idx -= len;
        }

        None
    }

    pub fn contains(&self, port: u16) -> bool {
        self.0.iter().any(|r| port >= r.start && port <= r.end)
    }

    pub fn normalize(mut self) -> Self {
        if self.0.is_empty() {
            return self;
        }

        self.0.sort_by(|a, b| {
            if a.start == b.start {
                a.end.cmp(&b.end)
            } else {
                a.start.cmp(&b.start)
            }
        });

        let mut normalized = vec![self.0[0].clone()];
        for current in self.0.into_iter().skip(1) {
            let last = normalized.last_mut().unwrap();
            if current.start as u32 <= last.end as u32 + 1 {
                if current.end > last.end {
                    last.end = current.end;
                }
            } else {
                normalized.push(current);
            }
        }

        PortUnion(normalized)
    }
}

impl FromStr for PortUnion {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s == "all" || s == "*" {
            return Ok(PortUnion(vec![PortRange {
                start: 0,
                end: 65535,
            }]));
        }

        let mut ranges = Vec::new();
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }

            if part.contains('-') {
                let mut bounds = part.splitn(2, '-');
                let start_str = bounds.next().unwrap().trim();
                let end_str = bounds.next().unwrap().trim();

                let mut start = start_str
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port: {}", start_str))?;
                let mut end = end_str
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port: {}", end_str))?;

                if start > end {
                    std::mem::swap(&mut start, &mut end);
                }

                ranges.push(PortRange { start, end });
            } else {
                let port = part
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port: {}", part))?;
                ranges.push(PortRange {
                    start: port,
                    end: port,
                });
            }
        }

        if ranges.is_empty() {
            return Err("empty port union string".to_string());
        }

        Ok(PortUnion(ranges).normalize())
    }
}
