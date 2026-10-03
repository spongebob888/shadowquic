use crate::{SDecode, SEncode};
use serde::{Deserialize, Serialize};
use shadowquic_macros::{SDecode, SEncode};
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tracing::{Instrument, Level, info_span, warn};

#[cfg(feature = "mixed")]
use crate::mixed::inbound::MixedServer;
use crate::{
    Inbound, Manager, Outbound,
    direct::outbound::DirectOut,
    drop_outbound::DropOutbound,
    error::SError,
    shadowquic::{inbound::ShadowQuicServer, outbound::ShadowQuicClient},
    socks::{inbound::SocksServer, outbound::SocksClient},
    sunnyquic::{inbound::SunnyQuicServer, outbound::SunnyQuicClient},
};

mod serde_utils;
use crate::dns::ResolverManager;
#[cfg(feature = "dns-server")]
pub use crate::dns::config::{
    DnsCfg, DnsFakeIpServerCfg, DnsSystemServerCfg, DnsTcpServerCfg, DnsTlsServerCfg,
    DnsUdpServerCfg,
};
#[cfg(all(feature = "tproxy", target_os = "linux"))]
use crate::tproxy::inbound::TproxyServer;

mod shadowquic;
mod sunnyquic;
pub use crate::config::serde_utils::*;
pub use crate::config::shadowquic::*;
pub use crate::config::sunnyquic::*;
mod router;
pub use router::RouterCfg;

/// Overall configuration of shadowquic.
///
/// Example:
/// ```yaml
/// inbounds:
/// - tag: proxy-in
///   type: xxx
///   xxx: xxx
/// outbounds:
/// - tag: proxy-out
///   type: xxx
///   xxx: xxx
/// router:
///   default-outbound: proxy-out
///   src: |
///     return function(ctx) return "proxy-out" end
///   # Or replace src with a file path:
///   # path: route.lua
/// log-level: trace # or debug, info, warn, error
/// ```
/// Supported inbound types are listed in [`InboundCfg`]
///
/// Supported outbound types are listed in [`OutboundCfg`]
///
/// Legacy `inbound` and `outbound` objects are also accepted. Missing tags on
/// these objects default to `inbound` and `outbound`, respectively.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// Listeners to run concurrently. Tags must be nonempty and must not
    /// collide with DNS or outbound tags.
    #[serde(alias = "inbound", deserialize_with = "deserialize_inbounds")]
    pub inbounds: Vec<InboundCfg>,
    /// Standalone DNS services. Tags must be nonempty and must not collide
    /// with inbound or outbound tags.
    #[cfg(feature = "dns-server")]
    #[serde(default)]
    pub dns: Vec<DnsCfg>,
    /// Explicit outbounds. DNS services are also registered under their own tags.
    #[serde(
        default,
        alias = "outbound",
        deserialize_with = "deserialize_outbounds"
    )]
    pub outbounds: Vec<OutboundCfg>,
    /// Request routing. Omit this section to use the first configured outbound.
    #[serde(default)]
    pub router: RouterCfg,
    #[serde(default)]
    pub log_level: LogLevel,
}
impl Config {
    /// Explicit outbounds come first, followed by automatically registered DNS services.
    fn outbound_tags(&self) -> impl Iterator<Item = &str> {
        let tags = self.outbounds.iter().map(OutboundCfg::tag);
        #[cfg(feature = "dns-server")]
        let tags = tags.chain(self.dns.iter().map(DnsCfg::tag));
        tags
    }

    /// Validate endpoint identities before opening any listeners.
    pub fn validate(&self) -> Result<(), SError> {
        for (kind, tags) in [
            (
                "inbound",
                self.inbounds
                    .iter()
                    .map(InboundCfg::tag)
                    .collect::<Vec<_>>(),
            ),
            ("outbound", self.outbound_tags().collect::<Vec<_>>()),
        ] {
            if tags.is_empty() {
                return Err(SError::InvalidConfig(format!(
                    "at least one {kind} is required"
                )));
            }
            let mut seen = HashSet::new();
            for tag in tags {
                if tag.trim().is_empty() {
                    return Err(SError::InvalidConfig(format!(
                        "{kind} tag must not be empty"
                    )));
                }
                if !seen.insert(tag) {
                    return Err(SError::InvalidConfig(format!(
                        "duplicate {kind} tag: {tag}"
                    )));
                }
            }
        }
        // Tags are unique across all endpoint kinds: inbound, DNS, and outbound.
        let mut seen = self
            .inbounds
            .iter()
            .map(InboundCfg::tag)
            .collect::<HashSet<_>>();
        for tag in self.outbound_tags() {
            if !seen.insert(tag) {
                return Err(SError::InvalidConfig(format!(
                    "inbound tag collides with an outbound or DNS tag: {tag}"
                )));
            }
        }
        if let Some(tag) = &self.router.default_outbound
            && !self.outbound_tags().any(|outbound| outbound == tag)
        {
            return Err(SError::InvalidConfig(format!(
                "default outbound tag does not match a configured outbound: {tag}"
            )));
        }
        #[cfg(feature = "dns-server")]
        {
            let reserved = crate::dns::DEFAULT_SYSTEM_DNS_TAG;
            if self.inbounds.iter().any(|c| c.tag() == reserved)
                || self.outbounds.iter().any(|c| c.tag() == reserved)
                || self.dns.iter().any(|c| c.tag() == reserved)
            {
                return Err(SError::InvalidConfig(format!(
                    "tag {reserved} is reserved for the default system DNS"
                )));
            }
            let mut fake_count = 0;
            for dns in &self.dns {
                dns.validate()?;
                fake_count += usize::from(dns.is_fake_ip());
            }
            if fake_count > 1 {
                return Err(SError::InvalidConfig(
                    "at most one fakeip DNS service is allowed".into(),
                ));
            }
            for outbound in &self.outbounds {
                if let Some(tag) = outbound.dns() {
                    let dns = self
                        .dns
                        .iter()
                        .find(|dns| dns.tag() == tag)
                        .ok_or_else(|| {
                            SError::InvalidConfig(format!("unknown DNS service: {tag}"))
                        })?;
                    if dns.is_fake_ip() {
                        return Err(SError::InvalidConfig(
                            "fakeip cannot resolve outbound destinations".into(),
                        ));
                    }
                }
            }
        }
        self.router.validate()?;
        Ok(())
    }

    pub async fn build_manager(self) -> Result<Manager, SError> {
        self.validate()?;
        let default_outbound = self
            .router
            .default_outbound
            .clone()
            .unwrap_or_else(|| self.outbound_tags().next().unwrap().to_owned());
        let mut inbounds = HashMap::new();
        let mut outbounds = HashMap::new();
        #[cfg(feature = "dns-server")]
        let mut resolver_manager = Arc::new(ResolverManager::new());
        #[cfg(not(feature = "dns-server"))]
        let resolver_manager = Arc::new(ResolverManager::new());
        #[cfg(all(feature = "dns-server", feature = "tproxy", target_os = "linux"))]
        let mut fake_ip = None;
        #[cfg(feature = "dns-server")]
        for cfg in self.dns {
            let tag = cfg.tag().to_owned();
            let server = cfg.build_with_cache(resolver_manager.cache()).await?;
            #[cfg(all(feature = "tproxy", target_os = "linux"))]
            if server.resolver.fake_ip.is_some() {
                fake_ip = server.resolver.fake_ip.clone();
            }
            outbounds.insert(tag.clone(), server.resolver.clone() as Arc<dyn Outbound>);
            Arc::make_mut(&mut resolver_manager).insert(tag.clone(), server.resolver.clone());
            inbounds.insert(tag.clone(), Box::new(server) as Box<dyn Inbound>);
        }
        #[cfg(feature = "plugin")]
        let router = self
            .router
            .build(
                #[cfg(feature = "dns-server")]
                resolver_manager.clone(),
            )?
            .map(Arc::new);
        for cfg in self.outbounds {
            let tag = cfg.tag().to_owned();
            let span = info_span!("outbound", tag = %tag);
            #[cfg(feature = "dns-server")]
            {
                let resolver = cfg
                    .dns()
                    .map(|tag| resolver_manager.resolver(tag).expect("validated DNS tag"));
                let strategy = match &cfg {
                    OutboundCfg::Direct(cfg) => cfg.dns_strategy.clone(),
                    _ => DnsStrategy::default(),
                };
                let inner: Arc<dyn Outbound> = cfg
                    .build_outbound(resolver_manager.clone())
                    .instrument(span)
                    .await?;
                let outbound: Arc<dyn Outbound> = match resolver {
                    Some(resolver) => Arc::new(crate::dns::ResolvingOutbound {
                        inner,
                        resolver,
                        strategy,
                    }),
                    None => inner,
                };
                outbounds.insert(tag, outbound);
            }
            #[cfg(not(feature = "dns-server"))]
            outbounds.insert(
                tag,
                Arc::from(
                    cfg.build_outbound(resolver_manager.clone())
                        .instrument(span)
                        .await?,
                ),
            );
        }
        for cfg in self.inbounds {
            let tag = cfg.tag().to_owned();
            let span = info_span!("inbound", tag = %tag);
            #[cfg(all(feature = "dns-server", feature = "tproxy", target_os = "linux"))]
            let restore_fake = matches!(&cfg, InboundCfg::Tproxy(_));
            let inbound = cfg.build_inbound().instrument(span).await?;
            #[cfg(all(feature = "dns-server", feature = "tproxy", target_os = "linux"))]
            let inbound = if restore_fake && let Some(fake) = fake_ip.clone() {
                Box::new(crate::dns::RestoringInbound {
                    inner: inbound,
                    fake,
                }) as Box<dyn Inbound>
            } else {
                inbound
            };
            inbounds.insert(tag, inbound);
        }
        Ok(Manager {
            inbounds,
            outbounds,
            default_outbound,
            #[cfg(feature = "plugin")]
            router,
            #[cfg(feature = "dns-server")]
            resolver_manager: Some(resolver_manager),
        })
    }
}

/// Inbound configuration
/// example:
/// ```yaml
/// tag: proxy
/// type: socks # or shadowquic
/// bind-addr: "0.0.0.0:443" # "[::]:443"
/// xxx: xxx # other field depending on type
/// ```
/// See [`SocksServerCfg`] and [`ShadowQuicServerCfg`] for configuration field of corresponding type
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum InboundCfg {
    Socks(SocksServerCfg),
    #[cfg(feature = "mixed")]
    Mixed(MixedServerCfg),
    #[serde(rename = "shadowquic")]
    ShadowQuic(ShadowQuicServerCfg),
    #[serde(rename = "sunnyquic")]
    SunnyQuic(SunnyQuicServerCfg),
    #[cfg(all(feature = "tproxy", target_os = "linux"))]
    #[serde(rename = "tproxy")]
    Tproxy(TproxyServerCfg),
}
impl InboundCfg {
    /// Returns the endpoint label.
    pub fn tag(&self) -> &str {
        match self {
            Self::Socks(cfg) => &cfg.tag,
            #[cfg(feature = "mixed")]
            Self::Mixed(cfg) => &cfg.tag,
            Self::ShadowQuic(cfg) => &cfg.tag,
            Self::SunnyQuic(cfg) => &cfg.tag,
            #[cfg(all(feature = "tproxy", target_os = "linux"))]
            Self::Tproxy(cfg) => &cfg.tag,
        }
    }

    async fn build_inbound(self) -> Result<Box<dyn Inbound>, SError> {
        let r: Box<dyn Inbound> = match self {
            InboundCfg::Socks(cfg) => Box::new(SocksServer::new(cfg).await?),
            #[cfg(feature = "mixed")]
            InboundCfg::Mixed(cfg) => Box::new(MixedServer::new(cfg).await?),
            InboundCfg::ShadowQuic(cfg) => Box::new(ShadowQuicServer::new(cfg).await?),
            InboundCfg::SunnyQuic(cfg) => Box::new(SunnyQuicServer::new(cfg).await?),
            #[cfg(all(feature = "tproxy", target_os = "linux"))]
            InboundCfg::Tproxy(cfg) => Box::new(TproxyServer::new(cfg).await?),
        };
        Ok(r)
    }
}

/// Outbound configuration
/// example:
/// ```yaml
/// tag: proxy
/// type: socks # or shadowquic, sunnyquic, direct, or drop
/// addr: "127.0.0.1:443" # "[::1]:443"
/// xxx: xxx # other field depending on type
/// ```
/// See [`SocksClientCfg`] and [`ShadowQuicClientCfg`] for configuration field of corresponding type
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum OutboundCfg {
    Socks(SocksClientCfg),
    #[serde(rename = "shadowquic")]
    ShadowQuic(ShadowQuicClientCfg),
    #[serde(rename = "sunnyquic")]
    SunnyQuic(SunnyQuicClientCfg),
    Direct(DirectOutCfg),
    #[serde(rename = "drop")]
    Drop(DropOutCfg),
}

impl OutboundCfg {
    #[cfg(feature = "dns-server")]
    fn dns(&self) -> Option<&str> {
        match self {
            Self::Direct(cfg) => cfg.dns.as_deref(),
            Self::Socks(cfg) => cfg.dns.as_deref(),
            Self::ShadowQuic(cfg) => cfg.dns.as_deref(),
            Self::SunnyQuic(cfg) => cfg.dns.as_deref(),
            Self::Drop(_) => None,
        }
    }

    /// Returns the endpoint label.
    pub fn tag(&self) -> &str {
        match self {
            Self::Socks(cfg) => &cfg.tag,
            Self::ShadowQuic(cfg) => &cfg.tag,
            Self::SunnyQuic(cfg) => &cfg.tag,
            Self::Direct(cfg) => &cfg.tag,
            Self::Drop(cfg) => &cfg.tag,
        }
    }

    async fn build_outbound(
        self,
        resolver_manager: Arc<ResolverManager>,
    ) -> Result<Arc<dyn Outbound>, SError> {
        let r: Arc<dyn Outbound> = match self {
            OutboundCfg::Socks(cfg) => Arc::new(SocksClient::new(cfg, resolver_manager.clone())),
            OutboundCfg::ShadowQuic(cfg) => {
                Arc::new(ShadowQuicClient::new(cfg, resolver_manager.clone()))
            }
            OutboundCfg::SunnyQuic(cfg) => Arc::new(SunnyQuicClient::new(cfg, resolver_manager)),
            OutboundCfg::Direct(cfg) => Arc::new(DirectOut::new(cfg)),
            OutboundCfg::Drop(_) => Arc::new(DropOutbound),
        };
        Ok(r)
    }
}

/// Socks inbound configuration
///
/// Example:
/// ```yaml
/// tag: proxy
/// type: socks
/// bind-addr: "0.0.0.0:1089" # or "[::]:1089" for dualstack
/// users:
///  - username: "username"
///    password: "password"
/// ```
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SocksServerCfg {
    /// Required label for this endpoint.
    pub tag: String,
    /// Server binding address. e.g. `0.0.0.0:1089`, `[::1]:1089`
    pub bind_addr: SocketAddr,
    /// Socks5 username, optional
    /// Left empty to disable authentication
    #[serde(default = "Vec::new")]
    pub users: Vec<AuthUser>,
}

/// Mixed inbound configuration
///
/// Supports SOCKS5 and HTTP proxy (CONNECT + plain HTTP forwarding) on the same port.
///
/// Example:
/// ```yaml
/// tag: proxy
/// type: mixed
/// bind-addr: "0.0.0.0:1080"
/// ```
#[cfg(feature = "mixed")]
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct MixedServerCfg {
    /// Required label for this endpoint.
    pub tag: String,
    /// Server binding address. e.g. `0.0.0.0:1080`, `[::]:1080`
    pub bind_addr: SocketAddr,
    /// Socks5 username, optional
    /// Left empty to disable authentication
    #[serde(default = "Vec::new")]
    pub users: Vec<AuthUser>,
}

/// Tproxy inbound configuration
///
/// Example:
/// ```yaml
/// tag: proxy
/// type: tproxy
/// bind-addr: "0.0.0.0:1089" # or "[::]:1089" for dualstack
/// ```
#[cfg(all(feature = "tproxy", target_os = "linux"))]
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TproxyServerCfg {
    /// Required label for this endpoint.
    pub tag: String,
    /// Server binding address. e.g. `0.0.0.0:1089`, `[::1]:1089`
    pub bind_addr: SocketAddr,
}

/// user authentication
#[derive(Deserialize, Clone, Debug, PartialEq, Eq, SEncode, SDecode)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct AuthUser {
    pub username: String,
    pub password: String,
}

/// Socks outbound configuration
/// Example:
/// ```yaml
/// tag: proxy
/// type: socks
/// addr: "12.34.56.7:1089" # or "[12:ff::ff]:1089" for dualstack
/// ```
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SocksClientCfg {
    /// DNS service used to resolve destination domains after routing.
    #[cfg(feature = "dns-server")]
    pub dns: Option<String>,
    /// DNS resolver used to resolve the server address.
    #[cfg(feature = "dns-server")]
    pub addr_resolver: Option<String>,
    /// Required label for this endpoint.
    pub tag: String,
    pub addr: String,
    /// SOCKS5 username, optional
    pub username: Option<String>,
    /// SOCKS5 password, optional
    pub password: Option<String>,
    /// Socket options like bind interface and fwmark
    #[serde(flatten)]
    pub socket_opt: SocketOpt,
}

/// Socket options
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SocketOpt {
    /// fw_mark on linux
    pub fw_mark: Option<u32>,
    /// binding interface of this outgoing packet.
    ///
    /// If `bind_interface` is set, the outgoing packet will be sent from the
    /// specified interface. Recommend to use to cooporate with other tun based proxy like sing-box/mihomo
    ///
    /// Example:
    /// ```yaml
    /// # by ip address
    /// bind-interface: "127.0.0.1"
    /// # by interface name
    /// bind-interface: "eth0"
    /// ```
    pub bind_interface: Option<Interface>,
}

/// binding interface of this outgoing packet.
///
/// If `bind_interface` is set, the outgoing packet will be sent from the
/// specified interface. Recommend to use to cooporate with other tun based proxy like sing-box/mihomo
///
/// Example:
/// ```yaml
/// # by ip address
/// bind-interface: "127.0.0.1"
/// # by interface name
/// bind-interface: "eth0"
/// ```
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum Interface {
    Address(IpAddr),
    Device(String),
}

pub fn default_initial_mtu() -> u16 {
    1300
}
pub fn default_min_mtu() -> u16 {
    1290
}
pub fn default_zero_rtt() -> bool {
    true
}
pub fn default_congestion_control() -> CongestionControl {
    CongestionControl::Bbr
}
pub fn default_over_stream() -> bool {
    false
}
pub fn default_alpn() -> Vec<String> {
    vec!["h3".into()]
}
pub fn default_keep_alive_interval() -> u32 {
    0
}

/// Default user-store flush interval in seconds
pub fn default_store_flush_interval() -> u64 {
    60
}

pub fn default_gso() -> bool {
    true
}

pub fn default_mtu_discovery() -> bool {
    true
}

pub fn default_blackhole_detection() -> bool {
    false
}

pub fn default_brutal_bandwidth() -> u64 {
    10_000_000
}

pub fn default_brutal_cwnd_gain() -> f64 {
    1.10
}

pub fn default_brutal_min_window() -> u64 {
    16 * 1024
}

pub fn default_brutal_min_ack_rate() -> f64 {
    0.8
}

pub fn default_brutal_min_sample_count() -> u64 {
    50
}

pub fn default_brutal_ack_compensate() -> bool {
    false
}

/// Congestion control algorithm
/// Example:
/// ```yaml
/// congestion-control: bbr # or cubic, new-reno, brutal
/// ```
/// If `brutal` is used, the configuration is like:
/// ```yaml
/// congestion-control:
///   brutal:
///     bandwidth: 10000000 # default 10000000 bps
/// ```
/// For Brutal, the bandwidth is the uploading bandwidth.
/// If you want to
/// set the downloading bandwidth,  set the bandwidth of the peer(e.g. it's the server for the client)
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum CongestionControl {
    #[default]
    Bbr,
    Cubic,
    NewReno,
    Brutal(BrutalParams),
    Bbr3,
}

impl PartialEq for CongestionControl {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (CongestionControl::Bbr, CongestionControl::Bbr)
                | (CongestionControl::Cubic, CongestionControl::Cubic)
                | (CongestionControl::NewReno, CongestionControl::NewReno)
                | (CongestionControl::Brutal(_), CongestionControl::Brutal(_))
        )
    }
}

/// Configuration of direct outbound
/// Example:
/// ```yaml
/// tag: proxy
/// dns-strategy: prefer-ipv4 # or prefer-ipv6, ipv4-only, ipv6-only
/// ```
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DirectOutCfg {
    /// DNS service used to resolve destination domains after routing.
    #[cfg(feature = "dns-server")]
    pub dns: Option<String>,
    /// Required label for this endpoint.
    pub tag: String,
    #[serde(default)]
    pub dns_strategy: DnsStrategy,
}

/// Outbound that discards every request.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DropOutCfg {
    /// Required label for this endpoint.
    pub tag: String,
}
/// DNS resolution strategy
/// Default is `prefer-ipv4`
///
/// - `prefer-ipv4`: try to use ipv4 first, if no ipv4 address, use ipv6
/// - `prefer-ipv6`: try to use ipv6 first, if no ipv6 address, use ipv4
/// - `ipv4-only`: only use ipv4 address
/// - `ipv6-only`: only use ipv6 address
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DnsStrategy {
    /// try to use ipv4 first, if no ipv4 address, use ipv6
    #[default]
    PreferIpv4,
    /// try to use ipv6 first, if no ipv6 address, use ipv4
    PreferIpv6,
    /// only use ipv4 address
    Ipv4Only,
    /// only use ipv6 address  
    Ipv6Only,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CipherSuitePreference {
    Aes128Gcm,
    Chacha20Poly1305,
    Aes256Gcm,
}

pub trait HasCipherSuitePreference {
    fn has_cipher_suite_preference(&self) -> bool;
}

pub fn maybe_warn_cipher_suite_on_weak_arch<T: HasCipherSuitePreference>(_cfg: &T) {
    #[cfg(any(target_arch = "mips", target_arch = "mips64"))]
    {
        if !_cfg.has_cipher_suite_preference() {
            warn!(
                "No `cipher-suite-preference` configured on MIPS target. \
                 AES-128-GCM may be significantly slower than ChaCha20-Poly1305 on weak MIPS devices. \
                 Consider setting `cipher-suite-preference: [\"chacha20-poly1305\", \"aes128-gcm\", \"aes256-gcm\"]`."
            );
        }
    }
}

pub fn normalize_cipher_suite_preference(
    cipher_suite_preference: &[CipherSuitePreference],
) -> Vec<CipherSuitePreference> {
    let mut out = Vec::new();

    for suite in cipher_suite_preference {
        if !out.contains(suite) {
            out.push(suite.clone());
        }
    }

    if !out.contains(&CipherSuitePreference::Aes128Gcm) {
        warn!(
            "`cipher-suite-preference` does not include `aes128-gcm`; appending it automatically"
        );
        out.push(CipherSuitePreference::Aes128Gcm);
    }

    out
}

/// Log level of shadowquic
/// Default level is info.
#[derive(Deserialize, Clone, Default, Debug)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}
impl LogLevel {
    pub fn as_tracing_level(&self) -> Level {
        match self {
            LogLevel::Trace => Level::TRACE,
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Info => Level::INFO,
            LogLevel::Warn => Level::WARN,
            LogLevel::Error => Level::ERROR,
        }
    }
}

#[cfg(test)]
mod test {
    use crate::config::{CongestionControl, Interface, ShadowQuicClientCfg};

    use super::Config;
    use super::{CipherSuitePreference, normalize_cipher_suite_preference};

    fn multi_config() -> Config {
        serde_saphyr::from_str(
            r#"
inbounds:
  - {tag: one, type: socks, bind-addr: "127.0.0.1:0"}
  - {tag: two, type: socks, bind-addr: "127.0.0.1:0"}
outbounds:
  - {tag: z-first, type: direct}
  - {tag: a-second, type: direct}
"#,
        )
        .unwrap()
    }

    #[test]
    fn main_branch_configs_are_compatible() {
        for yaml in [
            include_str!("../../tests/fixtures/main_config/client.yaml"),
            include_str!("../../tests/fixtures/main_config/client_brutal.yaml"),
            include_str!("../../tests/fixtures/main_config/client_sunnyquic.yaml"),
            include_str!("../../tests/fixtures/main_config/server.yaml"),
            include_str!("../../tests/fixtures/main_config/server_sunnyquic.yaml"),
            include_str!("../../tests/fixtures/main_config/socks2socks.yaml"),
        ] {
            let cfg: Config = serde_saphyr::from_str(yaml).unwrap();
            cfg.validate().unwrap();
            assert_eq!(cfg.inbounds.len(), 1);
            assert_eq!(cfg.outbounds.len(), 1);
            assert_eq!(cfg.inbounds[0].tag(), "inbound");
            assert_eq!(cfg.outbounds[0].tag(), "outbound");
        }
    }

    #[tokio::test]
    async fn legacy_config_builds_manager() {
        let cfg: Config = serde_saphyr::from_str(
            "inbound: {type: socks, bind-addr: '127.0.0.1:0'}\noutbound: {type: direct}\n",
        )
        .unwrap();
        let manager = cfg.build_manager().await.unwrap();
        assert_eq!(manager.inbounds.len(), 1);
        assert_eq!(manager.outbounds.len(), 1);
        assert!(manager.inbounds.contains_key("inbound"));
        assert!(manager.outbounds.contains_key("outbound"));
        assert_eq!(manager.default_outbound, "outbound");
    }

    #[test]
    fn legacy_config_preserves_explicit_tags_and_settings() {
        let cfg: Config = serde_saphyr::from_str(
            r#"
inbound: {type: socks, tag: local, bind-addr: '127.0.0.1:1089', users: [{username: user, password: secret}]}
outbound: {type: direct, tag: remote, dns-strategy: ipv6-only}
router:
  default-outbound: remote
log-level: debug
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.inbounds[0].tag(), "local");
        assert_eq!(cfg.outbounds[0].tag(), "remote");
        assert_eq!(cfg.router.default_outbound.as_deref(), Some("remote"));
        assert_eq!(cfg.log_level.as_tracing_level(), tracing::Level::DEBUG);
        let super::InboundCfg::Socks(inbound) = &cfg.inbounds[0] else {
            panic!("expected socks inbound");
        };
        assert_eq!(inbound.bind_addr.port(), 1089);
        assert_eq!(inbound.users[0].username, "user");
        assert_eq!(inbound.users[0].password, "secret");
        let super::OutboundCfg::Direct(outbound) = &cfg.outbounds[0] else {
            panic!("expected direct outbound");
        };
        assert!(matches!(
            outbound.dns_strategy,
            super::DnsStrategy::Ipv6Only
        ));
    }

    #[test]
    fn compatibility_keeps_config_errors() {
        let yaml = "inbound: {type: socks, bind-addr: '127.0.0.1:0'}\noutbound: {type: direct}\n";
        for extra in [
            "inbounds: []\n",
            "outbounds: []\n",
            "unknown-option: true\n",
        ] {
            assert!(serde_saphyr::from_str::<Config>(&format!("{yaml}{extra}")).is_err());
        }
        for invalid in [
            yaml.replace("type: socks", "type: socks, typo: true"),
            yaml.replace("type: direct", "type: direct, typo: true"),
            yaml.replace("type: direct", "type: direct, tag: a, tag: b"),
            yaml.replace("outbound: {type: direct}", "outbounds: [{type: direct}]"),
            yaml.replace("inbound: {", "inbounds: [{")
                .replace("0'}", "0'}]"),
        ] {
            assert!(
                serde_saphyr::from_str::<Config>(&invalid).is_err(),
                "{invalid}"
            );
        }
        for tag in ["''", "'  '"] {
            let invalid = yaml.replace("type: direct", &format!("type: direct, tag: {tag}"));
            let cfg: Config = serde_saphyr::from_str(&invalid).unwrap();
            assert!(cfg.validate().is_err());
        }
    }

    #[test]
    fn router_accepts_inline_source_or_script_path() {
        let inline: Config = serde_saphyr::from_str(
            r#"
inbounds:
  - {tag: in, type: socks, bind-addr: "127.0.0.1:0"}
outbounds:
  - {tag: out, type: direct}
router:
  src: |
    return function(ctx) return "out" end
"#,
        )
        .unwrap();
        assert_eq!(
            inline.router.src.as_deref(),
            Some("return function(ctx) return \"out\" end\n")
        );
        assert!(inline.router.path.is_none());

        let from_file: Config = serde_saphyr::from_str(
            r#"
inbounds:
  - {tag: in, type: socks, bind-addr: "127.0.0.1:0"}
outbounds:
  - {tag: out, type: direct}
router:
  path: router.lua
"#,
        )
        .unwrap();
        assert_eq!(
            from_file.router.path.as_deref(),
            Some(std::path::Path::new("router.lua"))
        );
        assert!(from_file.router.src.is_none());
    }

    #[test]
    fn router_defaults_to_disabled_and_rejects_unknown_fields() {
        let cfg = multi_config();
        assert!(cfg.router.src.is_none());
        assert!(cfg.router.path.is_none());
        let empty: super::RouterCfg = serde_saphyr::from_str("{}").unwrap();
        empty.validate().unwrap();
        for yaml in [
            "source: return nil",
            "script: router.lua",
            "src: return nil\nscr: typo",
        ] {
            assert!(serde_saphyr::from_str::<super::RouterCfg>(yaml).is_err());
        }
    }

    #[cfg(feature = "plugin")]
    #[tokio::test]
    async fn nested_router_source_builds_and_reports_script_errors() {
        let mut cfg = multi_config();
        cfg.router.src = Some("return function(_) return 'z-first' end".into());
        let manager = cfg.build_manager().await.unwrap();
        assert!(manager.router.is_some());

        let mut cfg = multi_config();
        cfg.router.src = Some("not a valid Lua script".into());
        let Err(error) = cfg.build_manager().await else {
            panic!("invalid router source must fail before building listeners");
        };
        assert!(
            error
                .to_string()
                .contains("failed to load inline router script")
        );
    }

    #[cfg(not(feature = "plugin"))]
    #[test]
    fn nested_router_requires_plugin_feature() {
        for router in [
            super::RouterCfg {
                src: Some("return function(_) return 'out' end".into()),
                ..Default::default()
            },
            super::RouterCfg {
                path: Some("router.lua".into()),
                ..Default::default()
            },
        ] {
            let mut cfg = multi_config();
            cfg.router = router;
            assert!(cfg.validate().unwrap_err().to_string().contains("plugin"));
        }
    }

    #[tokio::test]
    async fn router_default_outbound_selects_a_configured_tag() {
        let mut cfg = multi_config();
        cfg.router = serde_saphyr::from_str("default-outbound: a-second").unwrap();
        let manager = cfg.build_manager().await.unwrap();
        assert_eq!(manager.default_outbound, "a-second");
        #[cfg(feature = "plugin")]
        assert!(manager.router.is_none());
    }

    #[test]
    fn router_default_outbound_rejects_unknown_tags() {
        let mut cfg = multi_config();
        cfg.router.default_outbound = Some("missing".into());
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("default outbound tag does not match a configured outbound: missing")
        );
    }

    #[test]
    fn router_source_and_script_path_are_mutually_exclusive() {
        let mut cfg = multi_config();
        cfg.router.src = Some("return function(_) return 'out' end".into());
        cfg.router.path = Some("router.lua".into());
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("configure either `router.src` or `router.path`, not both")
        );
    }

    #[tokio::test]
    async fn builds_all_endpoints_and_preserves_first_outbound() {
        let manager = multi_config().build_manager().await.unwrap();
        assert_eq!(manager.inbounds.len(), 2);
        assert_eq!(manager.outbounds.len(), 2);
        assert!(manager.inbounds.contains_key("one"));
        assert!(manager.outbounds.contains_key("a-second"));
        assert_eq!(manager.default_outbound, "z-first");
    }

    #[test]
    fn rejects_invalid_endpoint_lists() {
        let mut cfg = multi_config();
        cfg.inbounds.push(cfg.inbounds[0].clone());
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate inbound")
        );
        let mut cfg = multi_config();
        cfg.outbounds.push(cfg.outbounds[0].clone());
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate outbound")
        );
        let mut cfg = multi_config();
        cfg.inbounds.clear();
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("at least one inbound")
        );
        let mut cfg = multi_config();
        cfg.outbounds.clear();
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("at least one outbound")
        );
        for tag in ["", "   "] {
            let mut cfg = multi_config();
            let super::InboundCfg::Socks(inbound) = &mut cfg.inbounds[0] else {
                unreachable!()
            };
            inbound.tag = tag.into();
            assert!(
                cfg.validate()
                    .unwrap_err()
                    .to_string()
                    .contains("inbound tag must not be empty")
            );
            let mut cfg = multi_config();
            let super::OutboundCfg::Direct(outbound) = &mut cfg.outbounds[0] else {
                unreachable!()
            };
            outbound.tag = tag.into();
            assert!(
                cfg.validate()
                    .unwrap_err()
                    .to_string()
                    .contains("outbound tag must not be empty")
            );
        }
    }

    #[test]
    fn rejects_tag_collisions_between_inbounds_and_outbounds() {
        let mut cfg = multi_config();
        let super::OutboundCfg::Direct(outbound) = &mut cfg.outbounds[0] else {
            unreachable!()
        };
        outbound.tag = "one".into();
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("inbound tag collides with an outbound or DNS tag: one")
        );
    }

    #[test]
    fn rejects_reserved_default_system_dns_tag() {
        // DNS services may not claim the reserved default system resolver tag.
        let mut cfg = multi_config();
        cfg.dns.push(crate::dns::config::DnsCfg::System(
            crate::dns::config::DnsSystemServerCfg {
                tag: crate::dns::DEFAULT_SYSTEM_DNS_TAG.to_string(),
                bind_addr: "127.0.0.1:0".parse().unwrap(),
            },
        ));
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("reserved for the default system DNS")
        );

        // Outbound tags share the endpoint namespace and must avoid it too.
        let mut cfg = multi_config();
        let super::OutboundCfg::Direct(outbound) = &mut cfg.outbounds[0] else {
            unreachable!()
        };
        outbound.tag = crate::dns::DEFAULT_SYSTEM_DNS_TAG.into();
        assert!(
            cfg.validate()
                .unwrap_err()
                .to_string()
                .contains("reserved for the default system DNS")
        );
    }

    #[test]
    fn bundled_configs_are_valid() {
        for yaml in [
            include_str!("../../config_examples/client.yaml"),
            include_str!("../../config_examples/client_brutal.yaml"),
            include_str!("../../config_examples/client_sunnyquic.yaml"),
            include_str!("../../config_examples/server.yaml"),
            include_str!("../../config_examples/server_sunnyquic.yaml"),
            include_str!("../../config_examples/socks2socks.yaml"),
        ] {
            serde_saphyr::from_str::<Config>(yaml)
                .unwrap()
                .validate()
                .unwrap();
        }
    }

    #[test]
    fn endpoint_tags() {
        let inbounds = [
            "type: socks\nbind-addr: 127.0.0.1:1080\n",
            #[cfg(feature = "mixed")]
            "type: mixed\nbind-addr: 127.0.0.1:1080\n",
            #[cfg(all(feature = "tproxy", target_os = "linux"))]
            "type: tproxy\nbind-addr: 127.0.0.1:1080\n",
            "type: shadowquic\nbind-addr: 127.0.0.1:443\nusers: []\njls-upstream:\n  addr: localhost:443\n",
            "type: sunnyquic\nbind-addr: 127.0.0.1:443\nusers: []\nserver-name: localhost\ncert-path: cert.pem\nkey-path: key.pem\n",
        ];
        let outbounds = [
            "type: direct\n",
            "type: drop\n",
            "type: socks\naddr: localhost:1080\n",
            "type: shadowquic\naddr: localhost:443\nusername: test\npassword: test\nserver-name: localhost\n",
            "type: sunnyquic\naddr: localhost:443\nusername: test\npassword: test\nserver-name: localhost\n",
        ];
        for yaml in inbounds {
            let err = serde_saphyr::from_str::<super::InboundCfg>(yaml).unwrap_err();
            assert!(err.to_string().contains("missing field `tag`"), "{err}");
            let legacy = format!(
                "inbound:\n  {}\noutbound: {{type: direct}}\n",
                yaml.trim_end().replace('\n', "\n  ")
            );
            let cfg: Config = serde_saphyr::from_str(&legacy).unwrap();
            cfg.validate().unwrap();
            assert_eq!(cfg.inbounds[0].tag(), "inbound");
        }
        for yaml in outbounds {
            let err = serde_saphyr::from_str::<super::OutboundCfg>(yaml).unwrap_err();
            assert!(err.to_string().contains("missing field `tag`"), "{err}");
            let legacy = format!(
                "inbound: {{type: socks, bind-addr: '127.0.0.1:0'}}\noutbound:\n  {}\n",
                yaml.trim_end().replace('\n', "\n  ")
            );
            let cfg: Config = serde_saphyr::from_str(&legacy).unwrap();
            cfg.validate().unwrap();
            assert_eq!(cfg.outbounds[0].tag(), "outbound");
        }
        for (tag_yaml, expected) in [
            ("tag: test-endpoint\n", "test-endpoint"),
            ("tag: \"\"\n", ""),
        ] {
            for yaml in inbounds {
                let cfg: super::InboundCfg =
                    serde_saphyr::from_str(&format!("{yaml}{tag_yaml}")).unwrap();
                assert_eq!(cfg.tag(), expected);
            }
            for yaml in outbounds {
                let cfg: super::OutboundCfg =
                    serde_saphyr::from_str(&format!("{yaml}{tag_yaml}")).unwrap();
                assert_eq!(cfg.tag(), expected);
            }
        }
    }

    #[tokio::test]
    async fn drop_outbound_config_builds() {
        let cfg: super::OutboundCfg =
            serde_saphyr::from_str("type: drop\ntag: blackhole\n").unwrap();
        assert_eq!(cfg.tag(), "blackhole");
        assert!(
            cfg.build_outbound(std::sync::Arc::new(crate::dns::ResolverManager::new()))
                .await
                .is_ok()
        );
    }

    #[test]
    fn test() {
        let cfgstr = r###"
inbounds:
  - tag: socks-in
    type: socks
    bind-addr: 127.0.0.1:1089
outbounds:
  - tag: direct-out
    type: direct
    dns-strategy: prefer-ipv4
"###;
        let _cfg: Config = serde_saphyr::from_str(cfgstr).expect("yaml parsed failed");
    }
    #[test]
    fn test_fail() {
        let cfgstr = r###"
inbounds:
  - tag: socks-in
    type: socks
    bind-addr: 127.0.0.1:1089
    dhjsj: jkj
outbounds:
  - tag: direct-out
    type: direct
    dns-strategy: prefer-ipv4
"###;
        let cfg: Result<Config, _> = serde_saphyr::from_str(cfgstr);
        assert!(cfg.is_err());
    }
    #[test]
    fn test_cc() {
        let cfgstr = r###"
        tag: proxy-out
        username: "test"
        password: "test"
        addr: "127.0.0.1:1080"
        server-name: "localhost"
        congestion-control: 
            brutal:
                bandwidth: 1000

"###;
        let cfg: Result<ShadowQuicClientCfg, _> = serde_saphyr::from_str(cfgstr);
        match cfg.unwrap().congestion_control {
            CongestionControl::Brutal(params) => {
                assert_eq!(params.bandwidth, 1000);
            }
            _ => panic!("expected brutal congestion control"),
        }
    }

    #[test]
    fn test_socketopt() {
        let cfgstr = r###"
        tag: proxy-out
        username: "test"
        password: "test"
        addr: "127.0.0.1:1080"
        server-name: "localhost"
        bind-interface: "eth0"

"###;
        let cfg: Result<ShadowQuicClientCfg, _> = serde_saphyr::from_str(cfgstr);

        assert_eq!(
            cfg.unwrap().socket_opt.bind_interface.unwrap(),
            Interface::Device("eth0".to_string())
        );
    }
    #[test]
    fn normalize_cipher_suite_preference_preserves_first_seen_order_and_removes_duplicates() {
        let input = vec![
            CipherSuitePreference::Chacha20Poly1305,
            CipherSuitePreference::Aes256Gcm,
            CipherSuitePreference::Chacha20Poly1305,
            CipherSuitePreference::Aes128Gcm,
            CipherSuitePreference::Aes256Gcm,
        ];
        let normalized = normalize_cipher_suite_preference(&input);
        assert_eq!(
            normalized,
            vec![
                CipherSuitePreference::Chacha20Poly1305,
                CipherSuitePreference::Aes256Gcm,
                CipherSuitePreference::Aes128Gcm,
            ]
        );
    }
    #[test]
    fn normalize_cipher_suite_preference_appends_aes128_gcm_when_absent() {
        let input = vec![
            CipherSuitePreference::Aes256Gcm,
            CipherSuitePreference::Chacha20Poly1305,
            CipherSuitePreference::Aes256Gcm,
        ];
        let normalized = normalize_cipher_suite_preference(&input);
        assert_eq!(
            normalized,
            vec![
                CipherSuitePreference::Aes256Gcm,
                CipherSuitePreference::Chacha20Poly1305,
                CipherSuitePreference::Aes128Gcm,
            ]
        );
    }
}
