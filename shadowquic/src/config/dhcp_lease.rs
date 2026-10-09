use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Local DHCP lease files available to Lua routing helpers.
///
/// DHCP lease helpers require `router-dhcp-lease` (enabled by default) and
/// `router.dhcp-lease` entries with `type: dnsmasq` or `type: odhcp`, a unique
/// lease `tag`, and an optional `path`. Default paths are `/tmp/dhcp.leases` and
/// `/tmp/odhcpd.leases`. Both formats support IPv4 and IPv6 in the same file.
///
/// | DHCP helper | Return value |
/// | --- | --- |
/// | `find_dhcp_mac_v4(tag, ip)` | Lowercase colon-separated Ethernet MAC |
/// | `find_dhcp_host_v4(tag, ip)` | Hostname |
/// | `find_dhcp_duid_v6(tag, ip)` | Lowercase colon-separated client DUID |
/// | `find_dhcp_iaid_v6(tag, ip)` | Numeric unsigned 32-bit IAID |
/// | `find_dhcp_mac_v6(tag, ip)` | Ethernet MAC embedded in DUID-LL/LLT |
/// | `find_dhcp_host_v6(tag, ip)` | Hostname |
///
/// All DHCP helpers return nil for missing/expired leases or missing attributes.
/// Unknown tags and invalid IPs (including the wrong family) raise Lua errors.
/// They are available during script loading and routing without network or file
/// I/O on the lookup path. IPv6 MAC extraction can return nil and does not
/// necessarily identify the client's current interface. Delegated prefixes are
/// ignored. Lease files reload automatically; failures retain the previous data
/// with expiry still enforced. Missing initial files give empty results and are
/// retried. Stores survive Lua reloads, and lease updates do not reset Lua state.

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DhcpLeaseCfg {
    Dnsmasq(DnsmasqLeaseCfg),
    Odhcp(OdhcpLeaseCfg),
}

fn dnsmasq_path() -> PathBuf {
    "/tmp/dhcp.leases".into()
}
fn odhcp_path() -> PathBuf {
    "/tmp/odhcpd.leases".into()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DnsmasqLeaseCfg {
    pub tag: String,
    #[serde(default = "dnsmasq_path")]
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OdhcpLeaseCfg {
    pub tag: String,
    #[serde(default = "odhcp_path")]
    pub path: PathBuf,
}

impl DhcpLeaseCfg {
    pub fn tag(&self) -> &str {
        match self {
            Self::Dnsmasq(c) => &c.tag,
            Self::Odhcp(c) => &c.tag,
        }
    }
    pub fn path(&self) -> &Path {
        match self {
            Self::Dnsmasq(c) => &c.path,
            Self::Odhcp(c) => &c.path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouterCfg;

    #[test]
    fn dhcp_configuration() {
        for (kind, path) in [
            ("dnsmasq", "/tmp/dhcp.leases"),
            ("odhcp", "/tmp/odhcpd.leases"),
        ] {
            let yaml = format!("type: {kind}\ntag: lan\n");
            let cfg: DhcpLeaseCfg = serde_saphyr::from_str(&yaml).unwrap();
            assert_eq!(cfg.path(), Path::new(path));
            let encoded = serde_saphyr::to_string(&cfg).unwrap();
            assert_eq!(
                serde_saphyr::from_str::<DhcpLeaseCfg>(&encoded)
                    .unwrap()
                    .tag(),
                "lan"
            );
            assert!(
                serde_saphyr::from_str::<DhcpLeaseCfg>(&format!("{yaml}unknown: true\n")).is_err()
            );
        }
        assert!(serde_saphyr::from_str::<DhcpLeaseCfg>("type: other\ntag: lan").is_err());
        assert!(serde_saphyr::from_str::<DhcpLeaseCfg>("type: dnsmasq").is_err());
        assert!(
            serde_saphyr::from_str::<RouterCfg>("{}")
                .unwrap()
                .dhcp_lease
                .is_empty()
        );
    }

    #[test]
    fn dhcp_configuration_validation() {
        let parse = |entries: &str| {
            serde_saphyr::from_str::<RouterCfg>(&format!("dhcp-lease:\n{entries}")).unwrap()
        };
        for entries in [
            "  - {type: dnsmasq, tag: ''}",
            "  - {type: dnsmasq, tag: lan, path: ''}",
            "  - {type: dnsmasq, tag: lan}\n  - {type: odhcp, tag: lan}",
        ] {
            assert!(parse(entries).validate().is_err());
        }
        let result = parse("  - {type: dnsmasq, tag: lan}").validate();
        if cfg!(feature = "router-dhcp-lease") {
            assert!(result.is_ok());
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("router-dhcp-lease")
            );
        }
    }
    #[cfg(feature = "router-dhcp-lease")]
    #[test]
    fn documented_configuration_builds_with_initial_lease_data() {
        use std::{fs, sync::Arc};
        let mut config: crate::config::Config =
            serde_saphyr::from_str(include_str!("../../config_examples/router-dhcp-lease.yaml"))
                .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("leases");
        fs::write(&path, "0 02:00:00:00:00:01 192.0.2.1 node *").unwrap();
        config.router.dhcp_lease = vec![DhcpLeaseCfg::Dnsmasq(DnsmasqLeaseCfg {
            tag: "lan".into(),
            path,
        })];
        config.router.src = Some("assert(find_dhcp_host_v4('lan', '192.0.2.1') == 'node'); return function(_) return 'direct' end".into());
        let router = config
            .router
            .build(
                Arc::new(crate::dns::ResolverManager::new()),
                #[cfg(feature = "router-db")]
                Arc::default(),
            )
            .unwrap();
        assert!(router.is_some());
    }
}
