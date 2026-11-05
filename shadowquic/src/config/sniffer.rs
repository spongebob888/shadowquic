use crate::error::SError;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SniffProtocol {
    Http,
    Tls,
    Quic,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct SnifferRule {
    /// Empty means every destination port. Overlapping rules combine protocols.
    pub ports: Vec<u16>,
    pub protocols: Vec<SniffProtocol>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct SnifferCfg {
    pub tcp: Vec<SnifferRule>,
    pub udp: Vec<SnifferRule>,
    /// Absolute inspection deadline in milliseconds.
    pub timeout: u64,
    pub max_bytes: usize,
    pub max_concurrent: usize,
    pub max_buffered_bytes: usize,
}
impl Default for SnifferCfg {
    fn default() -> Self {
        Self {
            tcp: vec![],
            udp: vec![],
            timeout: 100,
            max_bytes: 32768,
            max_concurrent: 256,
            max_buffered_bytes: 8 * 1024 * 1024,
        }
    }
}
impl SnifferCfg {
    /// UDP reserves room for the final datagram that can cross the wire-byte limit.
    pub(crate) fn reservation_bytes(&self, udp: bool) -> usize {
        4 * self.max_bytes + if udp { 65536 } else { 0 }
    }

    pub fn validate(&self) -> Result<(), SError> {
        if !(1..=60000).contains(&self.timeout)
            || !(256..=1024 * 1024).contains(&self.max_bytes)
            || !(1..=65536).contains(&self.max_concurrent)
            || self.max_buffered_bytes < self.reservation_bytes(!self.udp.is_empty())
            || self.max_buffered_bytes > 256 * 1024 * 1024
        {
            return Err(SError::InvalidConfig("sniffer requires timeout 1..60000 ms, max-bytes 256..1048576, max-concurrent 1..65536, and max-buffered-bytes between four times max-bytes (plus 65536 for UDP) and 268435456".into()));
        }
        for (rules, udp) in [(&self.tcp, false), (&self.udp, true)] {
            for rule in rules {
                if rule.protocols.is_empty()
                    || rule
                        .protocols
                        .iter()
                        .any(|p| (*p == SniffProtocol::Quic) != udp)
                {
                    return Err(SError::InvalidConfig(
                        "sniffer rules require protocols: TCP supports http/tls; UDP supports quic"
                            .into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sniffer_configuration_validation() {
        for yaml in [
            "timeout: 0",
            "timeout: 60001",
            "max-bytes: 0",
            "max-bytes: 1048577",
            "max-concurrent: 0",
            "max-buffered-bytes: 100",
            "tcp: [{protocols: [quic]}]",
            "udp: [{protocols: [http]}]",
            "tcp: [{}]",
        ] {
            assert!(
                serde_saphyr::from_str::<SnifferCfg>(yaml)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{yaml}"
            );
        }
        assert!(serde_saphyr::from_str::<SnifferCfg>("tcp: [{protocols: [invalid]}]").is_err());
        assert!(serde_saphyr::from_str::<SnifferCfg>("timeuot: 100").is_err());
        let config: super::super::RouterCfg =
            serde_saphyr::from_str("sniffer: {}\nsrc: 'return function() return \"direct\" end'")
                .unwrap();
        assert_eq!(config.validate().is_ok(), cfg!(feature = "sniffer"));
        let config: super::super::RouterCfg = serde_saphyr::from_str("sniffer: {}").unwrap();
        assert!(config.validate().is_err());
    }
}
