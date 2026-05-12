use std::net::SocketAddr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

use crate::config::{
    AuthUser, CipherSuitePreference, CongestionControl, HasCipherSuitePreference, default_alpn,
    default_brutal_ack_compensate, default_brutal_bandwidth, default_brutal_cwnd_gain,
    default_brutal_min_ack_rate, default_brutal_min_sample_count, default_brutal_min_window,
    default_congestion_control, default_gso, default_initial_mtu, default_keep_alive_interval,
    default_min_mtu, default_mtu_discovery, default_over_stream, default_zero_rtt,
};

pub fn default_rate_limit() -> u64 {
    u64::MAX
}

pub fn parse_bps(input: &str) -> Result<u64, String> {
    let s = input.trim();

    if s.is_empty() {
        return Err("empty bandwidth string".to_string());
    }

    let (num_str, multiplier) = match s.as_bytes().last().copied() {
        Some(b'K') | Some(b'k') => (&s[..s.len() - 1], 1024f64),
        Some(b'M') | Some(b'm') => (&s[..s.len() - 1], 1024f64 * 1024f64),
        Some(b'G') | Some(b'g') => (&s[..s.len() - 1], 1024f64 * 1024f64 * 1024f64),
        Some(b'0'..=b'9') => (s, 1f64),
        _ => return Err(format!("invalid bandwidth suffix: {input}")),
    };

    let value: f64 = num_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid bandwidth number: {input}"))?;

    if !value.is_finite() {
        return Err(format!("invalid bandwidth number: {input}"));
    }

    if value < 0.0 {
        return Err(format!("bandwidth must be non-negative: {input}"));
    }

    let result = value * multiplier;

    if result > u64::MAX as f64 {
        return Err(format!("bandwidth value overflow: {input}"));
    }

    Ok(result.round() as u64)
}

pub fn deserialize_bps<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct BpsVisitor;

    impl<'de> Visitor<'de> for BpsVisitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an integer bps value or a string like \"30M\" or \"1.5G\"")
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value)
        }

        fn visit_u32<E>(self, value: u32) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value as u64)
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if value < 0 {
                return Err(E::custom("bandwidth must be non-negative"));
            }
            Ok(value as u64)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_bps(value).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_bps(&value).map_err(E::custom)
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if !value.is_finite() {
                return Err(E::custom("bandwidth must be finite"));
            }
            if value < 0.0 {
                return Err(E::custom("bandwidth must be non-negative"));
            }
            Ok(value.round() as u64)
        }
    }

    deserializer.deserialize_any(BpsVisitor)
}

pub fn parse_duration_str(s: &str) -> Result<Option<u32>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }

    let (num_part, mul) = if let Some(stripped) = s.strip_suffix("ms") {
        (stripped, 1u32)
    } else if let Some(stripped) = s.strip_suffix('s') {
        (stripped, 1000u32)
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped, 60_000u32)
    } else if let Some(stripped) = s.strip_suffix('h') {
        (stripped, 3_600_000u32)
    } else if let Some(stripped) = s.strip_suffix('d') {
        (stripped, 86_400_000u32)
    } else {
        (s, 1u32)
    };

    let num_part = num_part.trim();
    let ms = if num_part.contains('.') {
        let f: f64 = num_part
            .parse()
            .map_err(|_| format!("invalid duration: {}", s))?;
        if f < 0.0 {
            return Err("duration cannot be negative".into());
        }
        let ms_f = f * mul as f64;
        if ms_f > u32::MAX as f64 {
            return Err("duration exceeds u32::MAX".into());
        }
        ms_f.round() as u64
    } else {
        let num: u64 = num_part
            .parse()
            .map_err(|_| format!("invalid duration: {}", s))?;
        num.checked_mul(mul as u64)
            .ok_or_else(|| "duration overflow".to_string())?
    };

    if ms > u32::MAX as u64 {
        return Err("duration exceeds u32::MAX".into());
    }

    Ok(Some(ms as u32))
}

pub fn format_duration(ms: u32) -> String {
    if ms >= 86_400_000 && ms.is_multiple_of(86_400_000) {
        format!("{}d", ms / 86_400_000)
    } else if ms >= 3_600_000 && ms.is_multiple_of(3_600_000) {
        format!("{}h", ms / 3_600_000)
    } else if ms >= 60_000 && ms.is_multiple_of(60_000) {
        format!("{}m", ms / 60_000)
    } else if ms >= 1_000 && ms.is_multiple_of(1_000) {
        format!("{}s", ms / 1_000)
    } else {
        format!("{}ms", ms)
    }
}

pub fn deserialize_duration_ms<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    struct OptDurationMs;
    impl<'de> serde::de::Visitor<'de> for OptDurationMs {
        type Value = Option<u32>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("integer or duration string like 30s, 500ms")
        }
        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v > u32::MAX as u64 {
                return Err(E::custom("duration exceeds u32::MAX"));
            }

            Ok(Some(v as u32))
        }
        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            parse_duration_str(v).map_err(E::custom)
        }
        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }
    }
    deserializer.deserialize_any(OptDurationMs)
}

/// Configuration of shadowquic inbound
///
/// Example:
/// ```yaml
/// bind-addr: "0.0.0.0:1443"
/// users:
///   - username: "zhangsan"
///     password: "12345678"
/// jls-upstream:
///   addr: "echo.free.beeceptor.com:443" # domain/ip + port, domain must be the same as client.
///   rate-limit: 1000000 # Limiting forwarding rate in unit of bps. optional, default is disabled
/// server-name: "echo.free.beeceptor.com" # must be the same as client
/// alpn: ["h3"]
/// congestion-control: bbr
/// zero-rtt: true
/// ```
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ShadowQuicServerCfg {
    /// Binding address. e.g. `0.0.0.0:443`, `[::1]:443`
    pub bind_addr: SocketAddr,
    /// Users for client authentication
    pub users: Vec<AuthUser>,
    /// Server name used to check client. Must be the same as client
    /// If empty, server name will be parsed from jls_upstream
    /// If not available, server name check will be skipped
    pub server_name: Option<String>,
    /// Jls upstream, camouflage server, must be address with port. e.g.: `codepn.io:443`,`google.com:443`,`127.0.0.1:443`
    pub jls_upstream: JlsUpstream,
    /// Alpn of tls. Default is `["h3"]`, must have common element with client
    #[serde(default = "default_alpn")]
    pub alpn: Vec<String>,
    /// 0-RTT handshake.
    /// Set to true to enable zero rtt.
    /// Enabled by default
    #[serde(default = "default_zero_rtt")]
    pub zero_rtt: bool,
    /// Congestion control, default to "bbr", supported: "bbr", "new-reno", "cubic"
    #[serde(default = "default_congestion_control")]
    pub congestion_control: CongestionControl,
    /// Initial mtu, must be larger than min mtu, at least to be 1200.
    /// 1400 is recommended for high packet loss network. default to be 1300
    #[serde(default = "default_initial_mtu")]
    pub initial_mtu: u16,
    /// Minimum mtu, must be smaller than initial mtu, at least to be 1200.
    /// 1400 is recommended for high packet loss network. default to be 1290
    #[serde(default = "default_min_mtu")]
    pub min_mtu: u16,
    /// Enable QUIC Generic Segmentation Offload (GSO).
    /// Controls [`quinn::TransportConfig::enable_segmentation_offload`]. When supported, GSO reduces
    /// CPU usage for bulk sends; unsupported environments may see transient startup packet loss.
    /// Enabled by default
    #[serde(default = "default_gso")]
    pub gso: bool,
    /// Enable auto MTU discovery, default to true
    /// For stable udp network, it's better to disable it and set a proper initial mtu
    #[serde(default = "default_mtu_discovery")]
    pub mtu_discovery: bool,
}

/// Jls upstream configuration
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct JlsUpstream {
    /// Jls upstream address, e.g. `codepn.io:443`, `google.com:443`, `127.0.0.1:443`
    pub addr: String,
    /// Maximum rate for JLS forwarding in unit of bps, default is disabled.
    #[serde(default = "default_rate_limit")]
    pub rate_limit: u64,
}

impl Default for JlsUpstream {
    fn default() -> Self {
        Self {
            addr: String::new(),
            rate_limit: u64::MAX,
        }
    }
}
impl Default for ShadowQuicServerCfg {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:443".parse().unwrap(),
            users: Default::default(),
            jls_upstream: Default::default(),
            alpn: Default::default(),
            zero_rtt: Default::default(),
            congestion_control: Default::default(),
            initial_mtu: default_initial_mtu(),
            min_mtu: default_min_mtu(),
            server_name: None,
            gso: default_gso(),
            mtu_discovery: default_mtu_discovery(),
        }
    }
}

impl Default for ShadowQuicClientCfg {
    fn default() -> Self {
        Self {
            password: Default::default(),
            username: Default::default(),
            addr: Default::default(),
            server_name: Default::default(),
            alpn: Default::default(),
            initial_mtu: default_initial_mtu(),
            congestion_control: Default::default(),
            zero_rtt: Default::default(),
            over_stream: Default::default(),
            min_mtu: default_min_mtu(),
            keep_alive_interval: default_keep_alive_interval(),
            gso: default_gso(),
            mtu_discovery: default_mtu_discovery(),
            cipher_suite_preference: None,
            rebind_interval: None,
            min_rebind_interval: None,
            max_rebind_interval: None,
            #[cfg(target_os = "android")]
            protect_path: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct BrutalParams {
    #[serde(deserialize_with = "deserialize_bps")]
    pub bandwidth: u64,
    pub min_window: u64,
    pub cwnd_gain: f64,
    pub min_ack_rate: f64,
    pub min_sample_count: u64,
    pub ack_compensate: bool,
}

impl Default for BrutalParams {
    fn default() -> Self {
        Self {
            bandwidth: default_brutal_bandwidth(),
            min_window: default_brutal_min_window(),
            cwnd_gain: default_brutal_cwnd_gain(),
            min_ack_rate: default_brutal_min_ack_rate(),
            min_sample_count: default_brutal_min_sample_count(),
            ack_compensate: default_brutal_ack_compensate(),
        }
    }
}

/// Shadowquic outbound configuration
///   
/// example:
/// ```yaml
/// addr: "12.34.56.7:1089" # or "[12:ff::ff]:1089" for dualstack
/// password: "12345678"
/// username: "87654321"
/// server-name: "echo.free.beeceptor.com" # must be the same as jls_upstream in server
/// alpn: ["h3"]
/// initial-mtu: 1400
/// congestion-control: bbr
/// zero-rtt: true
/// over-stream: false  # true for udp over stream, false for udp over datagram
/// ```
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ShadowQuicClientCfg {
    /// username, must be the same as the server
    pub username: String,
    /// password, must be the same as the server
    pub password: String,
    /// Shadowquic server address. example: `127.0.0.0.1:443`, `www.server.com:443`, `[ff::f1]:4443`
    pub addr: String,
    /// Server name, must be the same as the server jls_upstream
    /// domain name
    pub server_name: String,
    /// Alpn of tls, default is \["h3"\], must have common element with server
    #[serde(default = "default_alpn")]
    pub alpn: Vec<String>,
    /// Initial mtu, must be larger than min mtu, at least to be 1200.
    /// 1400 is recommended for high packet loss network. default to be 1300
    #[serde(default = "default_initial_mtu")]
    pub initial_mtu: u16,
    /// Congestion control, default to "bbr", supported: "bbr", "new-reno", "cubic"
    #[serde(default = "default_congestion_control")]
    pub congestion_control: CongestionControl,
    /// Set to true to enable zero rtt, default to true
    #[serde(default = "default_zero_rtt")]
    pub zero_rtt: bool,
    /// Transfer udp over stream or over datagram.
    /// If true, use quic stream to send UDP, otherwise use quic datagram
    /// extension, similar to native UDP in TUIC
    #[serde(default = "default_over_stream")]
    pub over_stream: bool,
    #[serde(default = "default_min_mtu")]
    /// Minimum mtu, must be smaller than initial mtu, at least to be 1200.
    /// 1400 is recommended for high packet loss network. default to be 1290
    pub min_mtu: u16,
    /// Keep alive interval in milliseconds
    /// 0 means disable keep alive, should be smaller than 30_000(idle time).
    /// Disabled by default.
    #[serde(default = "default_keep_alive_interval")]
    pub keep_alive_interval: u32,

    /// Enable QUIC Generic Segmentation Offload (GSO).
    /// Controls [`quinn::TransportConfig::enable_segmentation_offload`]. When supported, GSO reduces
    /// CPU usage for bulk sends; unsupported environments may see transient startup packet loss.
    /// Enabled by default
    #[serde(default = "default_gso")]
    pub gso: bool,
    /// Enable auto MTU discovery, default to true
    /// For stable udp network, it's better to disable it and set a proper initial mtu
    #[serde(default = "default_mtu_discovery")]
    pub mtu_discovery: bool,

    /// Optional TLS 1.3 cipher suite preference.
    /// If unset, use rustls/ring default preference order.
    #[serde(default)]
    pub cipher_suite_preference: Option<Vec<CipherSuitePreference>>,

    #[serde(default, deserialize_with = "deserialize_duration_ms")]
    pub rebind_interval: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_duration_ms")]
    pub min_rebind_interval: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_duration_ms")]
    pub max_rebind_interval: Option<u32>,

    /// Android Only. the unix socket path for protecting android socket
    #[cfg(target_os = "android")]
    #[serde(default)]
    pub protect_path: Option<std::path::PathBuf>,
}

impl HasCipherSuitePreference for ShadowQuicClientCfg {
    fn has_cipher_suite_preference(&self) -> bool {
        self.cipher_suite_preference
            .as_ref()
            .is_some_and(|preferences| !preferences.is_empty())
    }
}
