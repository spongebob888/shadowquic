use ipnet::{Ipv4Net, Ipv6Net};
use serde::Deserialize;

use crate::error::SError;

/// Default IPv4 address and prefix for the TUN interface.
pub fn default_ipv4() -> Ipv4Net {
    "172.19.0.1/30".parse().unwrap()
}
/// Default IPv6 address and prefix for the TUN interface.
pub fn default_ipv6() -> Option<Ipv6Net> {
    Some("fdfe:dcba:9876::1/126".parse().unwrap())
}
/// Default TUN interface MTU in bytes.
pub fn default_mtu() -> u16 {
    1500
}
/// Default shared limit for TCP connections and UDP client sessions.
pub fn default_max_sessions() -> usize {
    1024
}
/// Default shared session idle timeout in seconds.
pub fn default_idle_timeout() -> u64 {
    300
}

/// A layer-3 TUN interface. Routing traffic into it is configured by the OS.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TunServerCfg {
    pub tag: String,
    /// Android VPN descriptor, supplied by the embedding application, never YAML.
    #[cfg(target_os = "android")]
    #[serde(skip)]
    pub android_fd: Option<std::sync::Arc<std::os::fd::OwnedFd>>,
    #[serde(default)]
    pub default_outbound: Option<String>,
    /// Leave unset to let the OS choose a name (utunN on macOS).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_ipv4")]
    pub ipv4: Ipv4Net,
    /// IPv6 interface address and prefix. Set to null to disable IPv6;
    /// omitting this field enables IPv6 with the default address.
    #[serde(default = "default_ipv6")]
    pub ipv6: Option<Ipv6Net>,
    #[serde(default = "default_mtu")]
    pub mtu: u16,
    /// Shared limit for TCP connections and UDP client sessions.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// Shared idle timeout in seconds, used when a protocol-specific setting is omitted.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,
    /// TCP socket timeout in seconds, managed by smoltcp. Set to 0 to disable.
    /// Defaults to idle-timeout when omitted.
    pub tcp_idle_timeout: Option<u64>,
    /// UDP session inactivity timeout in seconds, refreshed by either direction.
    /// Defaults to idle-timeout when omitted.
    pub udp_idle_timeout: Option<u64>,
}

impl TunServerCfg {
    pub fn tcp_idle_timeout(&self) -> u64 {
        self.tcp_idle_timeout.unwrap_or(self.idle_timeout)
    }

    pub fn udp_idle_timeout(&self) -> u64 {
        self.udp_idle_timeout.unwrap_or(self.idle_timeout)
    }

    pub fn validate(&self) -> Result<(), SError> {
        if self
            .name
            .as_ref()
            .is_some_and(|name| name.is_empty() || name.contains('\0'))
        {
            return Err(SError::InvalidConfig(
                "TUN name must be nonempty and contain no NUL".into(),
            ));
        }
        if !(1280..=65535).contains(&self.mtu) {
            return Err(SError::InvalidConfig(
                "TUN mtu must be at least 1280 for IPv6".into(),
            ));
        }
        if !(1..=65536).contains(&self.max_sessions) {
            return Err(SError::InvalidConfig(
                "TUN max-sessions must be 1..=65536".into(),
            ));
        }
        for (name, timeout, min) in [
            ("idle-timeout", self.idle_timeout, 1),
            ("tcp-idle-timeout", self.tcp_idle_timeout(), 0),
            ("udp-idle-timeout", self.udp_idle_timeout(), 1),
        ] {
            if !(min..=86400).contains(&timeout) {
                return Err(SError::InvalidConfig(format!(
                    "TUN {name} must be {min}..=86400 seconds"
                )));
            }
        }
        if self.ipv4.addr().is_unspecified()
            || self.ipv4.addr().is_multicast()
            || self.ipv4.addr().is_broadcast()
            || self
                .ipv6
                .is_some_and(|ipv6| ipv6.addr().is_unspecified() || ipv6.addr().is_multicast())
        {
            return Err(SError::InvalidConfig(
                "TUN interface addresses must be unicast".into(),
            ));
        }
        Ok(())
    }
}
