//! Lua routing runtime. See [`crate::config::RouterCfg`] for configuration and script examples.

use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
};

use notify::{RecursiveMode, Watcher};
use tracing::{info, info_span, warn};

use mlua::{Function, Lua, LuaOptions, StdLib, UserData, UserDataFields, chunk::ChunkMode};

#[cfg(feature = "router-db")]
use super::database::Databases;
use crate::dns::ResolverManager;
use crate::{
    DnsQuery, ProxyRequest, StatsContext, TcpSession, UdpSession,
    error::SError,
    msgs::socks5::{AddrOrDomain, SocksAddr},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkType {
    Tcp,
    Udp,
}
/// Request information exposed to a routing script.
#[derive(Clone)]
pub struct RouteContext {
    /// Destination domain. Scripts may update these for TCP requests only.
    /// Setting one of these clears the others.
    pub dst_domain: Option<String>,
    /// Destination ipv4 address. Scripts may update these for TCP requests only.
    /// Setting one of these clears the others.
    pub dst_ip_v4: Option<Ipv4Addr>,
    /// Destination ipv6 address. Scripts may update these for TCP requests only.
    /// Setting one of these clears the others.
    pub dst_ip_v6: Option<Ipv6Addr>,
    /// Destination port. Scripts may update this for TCP requests only.
    pub dst_port: Option<u16>,
    pub src_addr: Option<SocketAddr>,
    pub src_ip_v4: Option<Ipv4Addr>,
    pub src_ip_v6: Option<Ipv6Addr>,
    pub src_port: Option<u16>,
    pub inbound_tag: String,
    /// Outbound the accepting inbound prefers; a routing hint the script may
    /// honor or override. `None` means no preference was declared.
    pub preferred_outbound: Option<String>,
    /// DNS questions attached to the request; empty for requests without DNS metadata.
    pub dns_query: Vec<DnsQuery>,
    /// Only valid for shadowquic/sunnyquic inbound requests.
    pub stats_context: Option<StatsContext>,
    /// tcp or udp.
    pub network_type: NetworkType,
}
impl UserData for RouteContext {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("inbound_tag", |_, this| Ok(this.inbound_tag.clone()));
        fields.add_field_method_get("preferred_outbound", |_, this| {
            Ok(this.preferred_outbound.clone())
        });
        fields.add_field_method_get("dns_query", |lua, this| {
            let queries = lua.create_table()?;
            for (index, query) in this.dns_query.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("name", query.name.as_str())?;
                entry.set("record_type", query.record_type)?;
                queries.raw_set(index + 1, entry)?;
            }
            Ok(queries)
        });
        fields.add_field_method_get("network_type", |_, this| {
            Ok(match this.network_type {
                NetworkType::Tcp => "tcp",
                NetworkType::Udp => "udp",
            })
        });
        fields.add_field_method_get("dst_domain", |_, this| Ok(this.dst_domain.clone()));
        fields.add_field_method_get("dst_ip_v4", |_, this| {
            Ok(this.dst_ip_v4.map(|addr| addr.to_string()))
        });
        fields.add_field_method_get("dst_ip_v6", |_, this| {
            Ok(this.dst_ip_v6.map(|addr| addr.to_string()))
        });
        fields.add_field_method_get("dst_port", |_, this| Ok(this.dst_port));
        fields.add_field_method_set("dst_domain", |_, this, value: Option<String>| {
            this.ensure_destination_writable("dst_domain")?;
            this.dst_domain = value;
            if this.dst_domain.is_some() {
                this.dst_ip_v4 = None;
                this.dst_ip_v6 = None;
            }
            Ok(())
        });
        fields.add_field_method_set("dst_ip_v4", |_, this, value: Option<String>| {
            this.ensure_destination_writable("dst_ip_v4")?;
            this.dst_ip_v4 = value
                .map(|value| value.parse())
                .transpose()
                .map_err(mlua::Error::external)?;
            if this.dst_ip_v4.is_some() {
                this.dst_domain = None;
                this.dst_ip_v6 = None;
            }
            Ok(())
        });
        fields.add_field_method_set("dst_ip_v6", |_, this, value: Option<String>| {
            this.ensure_destination_writable("dst_ip_v6")?;
            this.dst_ip_v6 = value
                .map(|value| value.parse())
                .transpose()
                .map_err(mlua::Error::external)?;
            if this.dst_ip_v6.is_some() {
                this.dst_domain = None;
                this.dst_ip_v4 = None;
            }
            Ok(())
        });
        fields.add_field_method_set("dst_port", |_, this, value: Option<u16>| {
            this.ensure_destination_writable("dst_port")?;
            this.dst_port = value;
            Ok(())
        });
        fields.add_field_method_get("src_addr", |_, this| {
            Ok(this.src_addr.map(|addr| addr.to_string()))
        });
        fields.add_field_method_get("src_ip_v4", |_, this| {
            Ok(this.src_ip_v4.map(|addr| addr.to_string()))
        });
        fields.add_field_method_get("src_ip_v6", |_, this| {
            Ok(this.src_ip_v6.map(|addr| addr.to_string()))
        });
        fields.add_field_method_get("src_port", |_, this| Ok(this.src_port));
        fields.add_field_method_get("stats_context", |lua, this| {
            let Some(stats) = &this.stats_context else {
                return Ok(None);
            };
            let table = lua.create_table()?;
            table.set("username", stats.username.as_str())?;
            table.set("conn_id", stats.conn_id)?;
            Ok(Some(table))
        });
    }
}

impl RouteContext {
    fn ensure_destination_writable(&self, field: &str) -> mlua::Result<()> {
        if self.network_type == NetworkType::Udp {
            return Err(mlua::Error::runtime(format!(
                "cannot write {field} when network_type is udp"
            )));
        }
        Ok(())
    }

    pub(crate) fn from_request(req: &ProxyRequest) -> Self {
        match req {
            ProxyRequest::Tcp(TcpSession {
                dst,
                src_addr,
                user_context,
                ..
            }) => Self::from_dst(
                dst,
                *src_addr,
                &user_context.inbound_tag,
                user_context.preferred_outbound.clone(),
                NetworkType::Tcp,
                user_context.stats.clone(),
                &user_context.dns_query,
            ),
            ProxyRequest::Udp(UdpSession {
                dst,
                src_addr,
                user_context,
                ..
            }) => Self::from_dst(
                dst,
                *src_addr,
                &user_context.inbound_tag,
                user_context.preferred_outbound.clone(),
                NetworkType::Udp,
                user_context.stats.clone(),
                &user_context.dns_query,
            ),
        }
    }

    fn from_dst(
        dst: &SocksAddr,
        src_addr: Option<SocketAddr>,
        inbound_tag: &str,
        preferred_outbound: Option<String>,
        network_type: NetworkType,
        stats_context: Option<StatsContext>,
        dns_query: &[DnsQuery],
    ) -> Self {
        let (dst_domain, dst_ip_v4, dst_ip_v6) = match &dst.addr {
            AddrOrDomain::Domain(domain) => (
                std::str::from_utf8(&domain.contents)
                    .ok()
                    .map(str::to_owned),
                None,
                None,
            ),
            AddrOrDomain::V4(bytes) => (None, Some(Ipv4Addr::from(*bytes)), None),
            AddrOrDomain::V6(bytes) => (None, None, Some(Ipv6Addr::from(*bytes))),
        };
        Self {
            dst_domain,
            dst_ip_v4,
            dst_ip_v6,
            dst_port: Some(dst.port),
            src_ip_v4: src_addr.and_then(|addr| match addr.ip() {
                std::net::IpAddr::V4(ip) => Some(ip),
                std::net::IpAddr::V6(_) => None,
            }),
            src_ip_v6: src_addr.and_then(|addr| match addr.ip() {
                std::net::IpAddr::V6(ip) => Some(ip),
                std::net::IpAddr::V4(_) => None,
            }),
            src_port: src_addr.map(|addr| addr.port()),
            src_addr,
            inbound_tag: inbound_tag.to_owned(),
            preferred_outbound,
            dns_query: dns_query.to_vec(),
            stats_context,
            network_type,
        }
    }
}

/// A restricted Lua router. The script returns a function that accepts one
/// RouteContext table and returns an outbound tag or nil plus an error message.
pub struct Router {
    // Dropping the router stops watching the directory. The callback holds only
    // a weak reference to the runtime so it cannot keep the router alive.
    _watcher: Option<notify::RecommendedWatcher>,
    inner: Arc<Mutex<RouterInner>>,
}

struct RouterInner {
    #[cfg(feature = "router-db")]
    databases: Arc<Databases>,
    #[cfg(feature = "router-dhcp-lease")]
    leases: Arc<super::dhcp_lease::LeaseStore>,
    source: String,
    lua: Lua,
    route: Function,
}

impl Router {
    /// Load a script and watch its parent directory, including atomic file replacements.
    pub fn load(path: &Path) -> Result<Self, SError> {
        Self::load_with_databases(
            path,
            Arc::new(ResolverManager::new()),
            #[cfg(feature = "router-db")]
            Arc::default(),
            #[cfg(feature = "router-dhcp-lease")]
            Arc::default(),
        )
    }

    pub(crate) fn load_with_databases(
        path: &Path,
        resolver_manager: Arc<ResolverManager>,
        #[cfg(feature = "router-db")] databases: Arc<Databases>,
        #[cfg(feature = "router-dhcp-lease")] leases: Arc<super::dhcp_lease::LeaseStore>,
    ) -> Result<Self, SError> {
        let path = watched_script_path(path)?;
        let script = read_script(&path)?;
        let mut router = Self::from_source_with_databases(
            &script,
            resolver_manager.clone(),
            #[cfg(feature = "router-db")]
            databases,
            #[cfg(feature = "router-dhcp-lease")]
            leases,
        )
        .map_err(|error| {
            SError::InvalidConfig(format!(
                "failed to load router script {}: {error}",
                path.display()
            ))
        })?;
        let inner = Arc::downgrade(&router.inner);
        let watched_path = path.clone();
        let span = info_span!("router", path = %path.display());
        let callback_span = span.clone();

        let reload_manager = resolver_manager.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                callback_span.in_scope(|| match event {
                    Ok(event)
                        if !event.kind.is_access()
                            && (event.need_rescan()
                                || event.paths.iter().any(|p| p == &watched_path)) =>
                    {
                        if let Some(inner) = inner.upgrade() {
                            reload_script(&inner, &watched_path, reload_manager.clone());
                        }
                    }
                    Ok(_) => {}
                    Err(error) => warn!(%error, "router script watch failed"),
                });
            })
            .map_err(|error| {
                SError::InvalidConfig(format!(
                    "failed to watch router script {}: {error}",
                    path.display()
                ))
            })?;
        // Watching the directory survives editors saving by renaming a temporary
        // file over the original, and deletion followed by recreation.
        watcher
            .watch(path.parent().unwrap(), RecursiveMode::NonRecursive)
            .map_err(|error| {
                SError::InvalidConfig(format!(
                    "failed to watch router script {}: {error}",
                    path.display()
                ))
            })?;
        router._watcher = Some(watcher);
        // Close the gap between the initial read and watcher registration.
        span.in_scope(|| reload_script(&router.inner, &path, resolver_manager));
        Ok(router)
    }

    #[cfg(test)]
    pub(crate) fn from_source(source: &str) -> mlua::Result<Self> {
        Self::from_source_with_databases(
            source,
            Arc::new(ResolverManager::new()),
            #[cfg(feature = "router-db")]
            Arc::default(),
            #[cfg(feature = "router-dhcp-lease")]
            Arc::default(),
        )
    }

    #[cfg(all(test, feature = "dns-server"))]
    pub(crate) fn from_source_with_manager(
        source: &str,
        resolver_manager: Arc<ResolverManager>,
    ) -> mlua::Result<Self> {
        Self::from_source_with_databases(
            source,
            resolver_manager,
            #[cfg(feature = "router-db")]
            Arc::default(),
            #[cfg(feature = "router-dhcp-lease")]
            Arc::default(),
        )
    }

    pub(crate) fn from_source_with_databases(
        source: &str,
        resolver_manager: Arc<ResolverManager>,
        #[cfg(feature = "router-db")] databases: Arc<Databases>,
        #[cfg(feature = "router-dhcp-lease")] leases: Arc<super::dhcp_lease::LeaseStore>,
    ) -> mlua::Result<Self> {
        Ok(Self {
            _watcher: None,
            inner: Arc::new(Mutex::new(Self::compile_inner(
                source,
                resolver_manager,
                #[cfg(feature = "router-db")]
                databases,
                #[cfg(feature = "router-dhcp-lease")]
                leases,
            )?)),
        })
    }

    fn compile_inner(
        source: &str,
        resolver_manager: Arc<ResolverManager>,
        #[cfg(feature = "router-db")] databases: Arc<Databases>,
        #[cfg(feature = "router-dhcp-lease")] leases: Arc<super::dhcp_lease::LeaseStore>,
    ) -> mlua::Result<RouterInner> {
        #[cfg(not(feature = "dns-server"))]
        let _ = resolver_manager;
        let libs = StdLib::STRING | StdLib::TABLE | StdLib::MATH;
        let libs = libs | StdLib::BIT;
        let lua = Lua::new_with(libs, LuaOptions::default())?;
        #[cfg(feature = "dns-server")]
        {
            use crate::dns::DnsService;

            let lookup_manager = resolver_manager.clone();
            lua.globals().set(
                "lookup",
                lua.create_async_function(move |_, (tag, domain): (String, String)| {
                    let manager = lookup_manager.clone();
                    async move {
                        let resolver = manager.resolver(&tag).ok_or_else(|| {
                            mlua::Error::runtime(format!("unknown DNS resolver: {tag}"))
                        })?;
                        Ok(resolver
                            .resolve(&domain)
                            .await
                            .map_err(mlua::Error::external)?
                            .into_iter()
                            .map(|ip| ip.to_string())
                            .collect::<Vec<_>>())
                    }
                })?,
            )?;
            let reverse_manager = resolver_manager.clone();
            lua.globals().set(
                "reverse_lookup",
                lua.create_async_function(move |_, (tag, ip): (String, String)| {
                    let manager = reverse_manager.clone();
                    async move {
                        let resolver = manager.resolver(&tag).ok_or_else(|| {
                            mlua::Error::runtime(format!("unknown DNS resolver: {tag}"))
                        })?;
                        let ip = ip.parse().map_err(mlua::Error::external)?;
                        resolver
                            .reverse_lookup(ip)
                            .await
                            .map_err(mlua::Error::external)
                    }
                })?,
            )?;
            let lookup_manager = resolver_manager.clone();
            lua.globals().set(
                "lookup_cache",
                lua.create_function(move |_, domain: String| {
                    Ok(lookup_manager
                        .cache()
                        .lookup_cache(&domain)
                        .into_iter()
                        .map(|ip| ip.to_string())
                        .collect::<Vec<_>>())
                })?,
            )?;
            lua.globals().set(
                "reverse_lookup_cache",
                lua.create_function(move |_, ip: String| {
                    let ip = ip.parse().map_err(mlua::Error::external)?;
                    Ok(resolver_manager.cache().reverse_lookup_cache(ip))
                })?,
            )?;
        }
        #[cfg(feature = "router-db")]
        databases.install(&lua)?;
        #[cfg(feature = "router-dhcp-lease")]
        leases.install(&lua)?;
        Self::compile_common(
            source,
            lua,
            #[cfg(feature = "router-db")]
            databases,
            #[cfg(feature = "router-dhcp-lease")]
            leases,
        )
    }

    fn compile_common(
        source: &str,
        lua: Lua,
        #[cfg(feature = "router-db")] databases: Arc<Databases>,
        #[cfg(feature = "router-dhcp-lease")] leases: Arc<super::dhcp_lease::LeaseStore>,
    ) -> mlua::Result<RouterInner> {
        // The base library is always loaded, including file and code loaders.
        // Remove these before evaluating any user-provided source.
        let globals = lua.globals();
        globals.set(
            "info",
            lua.create_function(|_, message: String| {
                info!("{message}");
                Ok(())
            })?,
        )?;
        for name in ["dofile", "loadfile", "load", "loadstring"] {
            globals.set(name, mlua::Value::Nil)?;
        }
        // Only support on luau
        // lua.sandbox(true)?;
        // luau-jit can't be compiled on aarch64-musl, so we don't use it for now.
        // luau can't be compiled on freebsd
        // luau cost 1mb more bin size(2mb if luau-jit) than luajit
        let route = lua
            .load(source)
            .set_mode(ChunkMode::Text)
            .eval::<Function>()?;
        Ok(RouterInner {
            #[cfg(feature = "router-db")]
            databases,
            #[cfg(feature = "router-dhcp-lease")]
            leases,
            source: source.to_owned(),
            lua,
            route,
        })
    }

    pub async fn route(&self, context: &mut RouteContext) -> Result<String, SError> {
        let started = std::time::Instant::now();
        let result = async {
            // Keep this runtime alive across yields, without blocking other routes
            // or reloads. DNS upstream requests must be able to route concurrently.
            let (lua, route) = {
                let inner = self
                    .inner
                    .lock()
                    .map_err(|_| SError::RouterError("router runtime lock was poisoned".into()))?;
                (inner.lua.clone(), inner.route.clone())
            };
            let userdata = lua
                .create_userdata(context.clone())
                .map_err(|error| SError::RouterError(error.to_string()))?;
            let (tag, error): (Option<String>, Option<String>) = route
                .call_async(userdata.clone())
                .await
                .map_err(|error| SError::RouterError(error.to_string()))?;
            match (tag, error) {
                (Some(tag), _) if !tag.trim().is_empty() => {
                    *context = userdata
                        .borrow::<RouteContext>()
                        .map_err(|error| SError::RouterError(error.to_string()))?
                        .clone();
                    Ok(tag)
                }
                (_, Some(error)) if !error.trim().is_empty() => Err(SError::RouterError(error)),
                _ => Err(SError::RouterError(
                    "router must return an outbound tag or nil and an error message".into(),
                )),
            }
        }
        .await;
        tracing::trace!(
            elapsed = ?started.elapsed(),
            success = result.is_ok(),
            "routing completed",
        );
        result
    }
}

fn watched_script_path(path: &Path) -> Result<std::path::PathBuf, SError> {
    let normalize = || -> std::io::Result<std::path::PathBuf> {
        let absolute = std::path::absolute(path)?;
        let filename = absolute.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "router script must name a file",
            )
        })?;
        // FSEvents reports canonical paths (e.g. /private/var rather than /var
        // on macOS). Normalize the directory for both watching and matching.
        // Keep the filename so replacement/recreation still tracks this entry.
        Ok(absolute.parent().unwrap().canonicalize()?.join(filename))
    };
    normalize().map_err(|error| {
        SError::InvalidConfig(format!(
            "invalid router script path {}: {error}",
            path.display()
        ))
    })
}

fn read_script(path: &Path) -> Result<String, SError> {
    std::fs::read_to_string(path).map_err(|error| {
        SError::InvalidConfig(format!(
            "failed to read router script {}: {error}",
            path.display()
        ))
    })
}

fn reload_script(inner: &Mutex<RouterInner>, path: &Path, resolver_manager: Arc<ResolverManager>) {
    let result = (|| {
        // Serialize reloads and runtime snapshots so a late callback cannot
        // overwrite a newer version. In-flight routes retain their old runtime.
        let mut inner = inner
            .lock()
            .map_err(|_| SError::RouterError("router runtime lock was poisoned".into()))?;
        let source = read_script(path)?;
        if source == inner.source {
            return Ok(false);
        }
        let replacement = Router::compile_inner(
            &source,
            resolver_manager,
            #[cfg(feature = "router-db")]
            inner.databases.clone(),
            #[cfg(feature = "router-dhcp-lease")]
            inner.leases.clone(),
        )
        .map_err(|error| SError::RouterError(error.to_string()))?;
        *inner = replacement;
        Ok::<_, SError>(true)
    })();
    match result {
        Ok(true) => info!("router script reloaded"),
        Ok(false) => {}
        Err(error) => warn!(%error, "failed to reload router script; keeping current router"),
    }
}

impl RouteContext {
    pub(crate) fn destination(&self) -> Result<SocksAddr, SError> {
        let port = self
            .dst_port
            .ok_or_else(|| SError::RouterError("router cleared the destination port".into()))?;
        let addr = match (self.dst_domain.as_deref(), self.dst_ip_v4, self.dst_ip_v6) {
            (Some(domain), None, None)
                if !domain.is_empty() && domain.len() <= u8::MAX as usize =>
            {
                return Ok(SocksAddr::from_domain(domain.to_owned(), port));
            }
            (None, Some(ip), None) => AddrOrDomain::V4(ip.octets()),
            (None, None, Some(ip)) => AddrOrDomain::V6(ip.octets()),
            _ => {
                return Err(SError::RouterError(
                    "router must set exactly one valid destination domain or IP address".into(),
                ));
            }
        };
        Ok(SocksAddr { addr, port })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScriptDir(std::path::PathBuf);

    impl ScriptDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("shadowquic-router-{:x}", rand::random::<u64>()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn script(&self) -> std::path::PathBuf {
            self.0.join("router.lua")
        }
    }

    impl Drop for ScriptDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn wait_for_route(router: &Router, expected: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let actual = router.route(&mut context()).await.unwrap();
            if actual == expected {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "expected {expected}, got {actual}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn file_router_reloads_edits_replacements_and_recreation() {
        let dir = ScriptDir::new();
        let path = dir.script();
        std::fs::write(&path, "return function(_) return 'first' end").unwrap();
        let router = Router::load(&path).unwrap();
        assert_eq!(router.route(&mut context()).await.unwrap(), "first");

        std::fs::write(&path, "return function(_) return 'edited' end").unwrap();
        wait_for_route(&router, "edited").await;

        let replacement = dir.0.join("replacement.lua");
        std::fs::write(&replacement, "return function(_) return 'replaced' end").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        wait_for_route(&router, "replaced").await;

        std::fs::remove_file(&path).unwrap();
        reload_script(
            &router.inner,
            &path,
            std::sync::Arc::new(crate::dns::ResolverManager::new()),
        );
        assert_eq!(router.route(&mut context()).await.unwrap(), "replaced");
        std::fs::write(&path, "return function(_) return 'recreated' end").unwrap();
        wait_for_route(&router, "recreated").await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn file_router_normalizes_symlinked_parent_for_event_matching() {
        let dir = ScriptDir::new();
        let real = dir.0.join("real");
        let alias = dir.0.join("alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let path = alias.join("router.lua");
        let event_path = real.canonicalize().unwrap().join("router.lua");
        // Match the canonical filename even while it is absent, as during an
        // editor's delete/recreate save. Canonicalizing the whole file cannot.
        assert_eq!(watched_script_path(&path).unwrap(), event_path);
        std::fs::write(&path, "return function(_) return 'first' end").unwrap();
        let router = Router::load(&path).unwrap();
        std::fs::write(&event_path, "return function(_) return 'edited' end").unwrap();
        wait_for_route(&router, "edited").await;
        let replacement = real.join("replacement.lua");
        std::fs::write(&replacement, "return function(_) return 'replaced' end").unwrap();
        std::fs::rename(&replacement, &event_path).unwrap();
        wait_for_route(&router, "replaced").await;
    }

    #[tokio::test]
    async fn failed_reload_keeps_runtime_and_unchanged_source_keeps_state() {
        let dir = ScriptDir::new();
        let path = dir.script();
        let source = "local n = 0; return function(_) n = n + 1; return tostring(n) end";
        std::fs::write(&path, source).unwrap();
        let router = Router::load(&path).unwrap();
        assert_eq!(router.route(&mut context()).await.unwrap(), "1");
        reload_script(
            &router.inner,
            &path,
            std::sync::Arc::new(crate::dns::ResolverManager::new()),
        );
        assert_eq!(router.route(&mut context()).await.unwrap(), "2");
        for invalid in ["return function(", "return {}", "error('load failed')"] {
            std::fs::write(&path, invalid).unwrap();
            reload_script(
                &router.inner,
                &path,
                std::sync::Arc::new(crate::dns::ResolverManager::new()),
            );
        }
        assert_eq!(router.route(&mut context()).await.unwrap(), "3");
        std::fs::write(&path, source).unwrap();
        reload_script(
            &router.inner,
            &path,
            std::sync::Arc::new(crate::dns::ResolverManager::new()),
        );
        assert_eq!(router.route(&mut context()).await.unwrap(), "4");
        std::fs::write(
            &path,
            "return function(_) assert(io == nil); return 'fixed' end",
        )
        .unwrap();
        wait_for_route(&router, "fixed").await;
    }

    fn context() -> RouteContext {
        RouteContext {
            dst_domain: Some("api.example".into()),
            dst_ip_v4: None,
            dst_ip_v6: None,
            dst_port: Some(443),
            src_addr: Some("192.0.2.1:54321".parse().unwrap()),
            src_ip_v4: Some("192.0.2.1".parse().unwrap()),
            src_ip_v6: None,
            src_port: Some(54321),
            inbound_tag: "socks-in".into(),
            preferred_outbound: None,
            dns_query: Vec::new(),
            stats_context: None,
            network_type: NetworkType::Tcp,
        }
    }

    #[tokio::test]
    async fn lua_info_logs_during_initialization_and_routing() {
        use std::io::Write;
        use tracing::instrument::WithSubscriber;

        #[derive(Clone)]
        struct LogBuffer(Arc<Mutex<Vec<u8>>>);
        impl Write for LogBuffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(bytes)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogBuffer(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        async {
            let router = Router::from_source(
                r#"
                info("script loaded")
                return function(ctx)
                    info("routing " .. ctx.dst_domain)
                    assert(not pcall(info, {}))
                    assert(not pcall(info))
                    return "direct"
                end
            "#,
            )
            .unwrap();
            assert_eq!(router.route(&mut context()).await.unwrap(), "direct");
        }
        .with_subscriber(subscriber)
        .await;
        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(output.contains("INFO"), "{output}");
        assert!(output.contains("script loaded"), "{output}");
        assert!(output.contains("routing api.example"), "{output}");
    }

    #[tokio::test]
    async fn lua_router_restricts_capabilities_during_load_and_routing() {
        let router = Router::from_source(
            r#"
                local function check()
                    assert(type(print) == "function")
                    for _, name in ipairs({
                        "io", "os", "package", "require", "module", "debug",
                        "ffi", "jit", "dofile", "loadfile", "load", "loadstring"
                    }) do
                        assert(_G[name] == nil, name .. " must not be available")
                    end
                end
                check()
                return function(ctx)
                    check()
                    local tags = {string.lower("DIRECT"), tostring(math.floor(1.5))}
                    assert(string.match(ctx.dst_domain, "%.example$"))
                    return table.concat(tags, "-")
                end
            "#,
        )
        .unwrap();

        assert_eq!(router.route(&mut context()).await.unwrap(), "direct-1");
    }

    #[tokio::test]
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    async fn lua_router_supports_bit_operations() {
        let router = Router::from_source(
            r#"
                return function(ctx)
                    assert(bit.band(ctx.dst_port, 255) == 187)
                    return "direct"
                end
            "#,
        )
        .unwrap();

        assert_eq!(router.route(&mut context()).await.unwrap(), "direct");
    }

    #[tokio::test]
    async fn lua_router_receives_dns_questions_from_tcp_and_udp_requests() {
        let router = Router::from_source(
            r#"
                return function(ctx)
                    local questions = ctx.dns_query
                    if #questions == 0 then return "empty" end
                    assert(#questions == 2)
                    for i, question in ipairs(questions) do
                        assert(question.name == "query.example")
                        assert(question.record_type == (i == 1 and 1 or 28))
                    end
                    questions[1].name = "changed.example"
                    assert(ctx.dns_query[1].name == "query.example")
                    return ctx.network_type
                end
            "#,
        )
        .unwrap();
        for populated in [false, true] {
            let user_context = crate::UserContext {
                dns_query: if populated {
                    [1, 28]
                        .into_iter()
                        .map(|record_type| DnsQuery {
                            name: "query.example".into(),
                            record_type,
                        })
                        .collect()
                } else {
                    Vec::new()
                },
                ..Default::default()
            };
            let (stream, _peer) = tokio::io::duplex(64);
            let (send, recv) = tokio::sync::mpsc::channel(1);
            let dst: SocksAddr = "192.0.2.53:53".parse::<SocketAddr>().unwrap().into();
            let requests: [ProxyRequest; 2] = [
                ProxyRequest::Tcp(TcpSession {
                    stream: Box::new(stream) as crate::AnyTcp,
                    dst: dst.clone(),
                    src_addr: None,
                    user_context: user_context.clone(),
                }),
                ProxyRequest::Udp(UdpSession {
                    recv: Box::new(recv),
                    send: Arc::new(send),
                    stream: None,
                    bind_addr: dst.clone(),
                    dst,
                    src_addr: None,
                    user_context,
                }),
            ];
            for (request, network) in requests.iter().zip(["tcp", "udp"]) {
                let mut context = RouteContext::from_request(request);
                assert_eq!(
                    router.route(&mut context).await.unwrap(),
                    if populated { network } else { "empty" }
                );
                if populated {
                    assert_eq!(context.dns_query[0].name, "query.example");
                }
            }
        }
    }

    #[tokio::test]
    async fn route_context_copies_preferred_outbound_from_tcp_and_udp_requests() {
        let (stream, _peer) = tokio::io::duplex(64);
        let (send, recv) = tokio::sync::mpsc::channel(1);
        let dst: SocksAddr = "192.0.2.53:53".parse::<SocketAddr>().unwrap().into();
        let user_context = crate::UserContext {
            preferred_outbound: Some("proxy-a".into()),
            ..Default::default()
        };
        let requests: [ProxyRequest; 2] = [
            ProxyRequest::Tcp(TcpSession {
                stream: Box::new(stream) as crate::AnyTcp,
                dst: dst.clone(),
                src_addr: None,
                user_context: user_context.clone(),
            }),
            ProxyRequest::Udp(UdpSession {
                recv: Box::new(recv),
                send: Arc::new(send),
                stream: None,
                bind_addr: dst.clone(),
                dst,
                src_addr: None,
                user_context,
            }),
        ];
        for request in &requests {
            assert_eq!(
                RouteContext::from_request(request)
                    .preferred_outbound
                    .as_deref(),
                Some("proxy-a")
            );
        }
    }

    #[tokio::test]
    async fn lua_router_receives_the_inbound_preferred_outbound() {
        let router = Router::from_source(
            r#"
                return function(ctx)
                    if ctx.preferred_outbound == "proxy-a" then
                        return "proxy-a"
                    end
                    return "direct"
                end
            "#,
        )
        .unwrap();

        let mut context = context();
        context.preferred_outbound = Some("proxy-a".into());
        assert_eq!(router.route(&mut context).await.unwrap(), "proxy-a");
        context.preferred_outbound = None;
        assert_eq!(router.route(&mut context).await.unwrap(), "direct");
    }

    #[tokio::test]
    async fn lua_router_receives_context_and_returns_outbound_tag() {
        let router = Router::from_source(
            r#"
                return function(ctx)
                    if ctx.dst_domain == "api.example"
                        and ctx.inbound_tag == "socks-in"
                        and ctx.network_type == "tcp"
                        and ctx.dst_port == 443
                        and ctx.src_addr == "192.0.2.1:54321"
                        and ctx.src_ip_v4 == "192.0.2.1"
                        and ctx.src_port == 54321 then
                        ctx.dst_domain = "mutable.example"
                        ctx.dst_ip_v6 = "2001:db8::1"
                        ctx.dst_ip_v4 = "192.0.2.44"
                        ctx.dst_port = 8443
                        if ctx.dst_domain == nil
                            and ctx.dst_ip_v6 == nil
                            and ctx.dst_ip_v4 == "192.0.2.44"
                            and ctx.dst_port == 8443 then
                            return "secure"
                        end
                    end
                    return "direct"
                end
            "#,
        )
        .unwrap();

        let mut context = context();
        assert_eq!(router.route(&mut context).await.unwrap(), "secure");
        let rewritten = context.destination().unwrap();
        assert_eq!(rewritten.to_string(), "192.0.2.44:8443");
    }

    #[tokio::test]
    async fn lua_router_rejects_udp_destination_writes() {
        for (field, value) in [
            ("dst_domain", "'rewritten.example'"),
            ("dst_ip_v4", "'192.0.2.44'"),
            ("dst_ip_v6", "'2001:db8::1'"),
            ("dst_port", "8443"),
        ] {
            for value in [value, "nil"] {
                let mut context = context();
                context.network_type = NetworkType::Udp;
                if field == "dst_ip_v4" {
                    context.dst_domain = None;
                    context.dst_ip_v4 = Some("192.0.2.10".parse().unwrap());
                } else if field == "dst_ip_v6" {
                    context.dst_domain = None;
                    context.dst_ip_v6 = Some("2001:db8::10".parse().unwrap());
                }
                let lua = Lua::new();
                let userdata = lua.create_userdata(context.clone()).unwrap();
                lua.globals().set("ctx", userdata.clone()).unwrap();
                let error = lua
                    .load(format!("ctx.{field} = {value}"))
                    .exec()
                    .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains(&format!("cannot write {field} when network_type is udp"))
                );
                let unchanged = userdata.borrow::<RouteContext>().unwrap();
                assert_eq!(unchanged.dst_domain, context.dst_domain);
                assert_eq!(unchanged.dst_ip_v4, context.dst_ip_v4);
                assert_eq!(unchanged.dst_ip_v6, context.dst_ip_v6);
                assert_eq!(unchanged.dst_port, context.dst_port);
            }
        }
    }

    #[tokio::test]
    #[cfg(feature = "dns-server")]
    async fn lua_dns_helpers_report_invalid_inputs_and_resolution_errors() {
        for (call, message) in [
            (
                "lookup('missing', 'example.test')",
                "unknown DNS resolver: missing",
            ),
            (
                "reverse_lookup('missing', '192.0.2.7')",
                "unknown DNS resolver: missing",
            ),
            (
                "reverse_lookup('default-system', 'invalid')",
                "invalid IP address",
            ),
            ("lookup('default-system', string.rep('a', 64))", ""),
            (
                "reverse_lookup('default-system', '192.0.2.7')",
                "reverse lookup failed",
            ),
        ] {
            let router =
                Router::from_source(&format!("return function(_) {call}; return 'direct' end"))
                    .unwrap();
            let error = router.route(&mut context()).await.unwrap_err();
            assert!(matches!(error, SError::RouterError(_)));
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[tokio::test]
    async fn reload_preserves_suspended_routes_and_new_routes_use_new_script() {
        let dir = ScriptDir::new();
        let path = dir.script();
        let router =
            Arc::new(Router::from_source("return function(_) pause(); return 'old' end").unwrap());
        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        {
            let inner = router.inner.lock().unwrap();
            let entered = entered.clone();
            let resume = resume.clone();
            let pause = inner
                .lua
                .create_async_function(move |_, ()| {
                    let entered = entered.clone();
                    let resume = resume.clone();
                    async move {
                        entered.notify_one();
                        resume.notified().await;
                        Ok(())
                    }
                })
                .unwrap();
            inner.lua.globals().set("pause", pause).unwrap();
        }
        let pending_router = router.clone();
        let pending = tokio::spawn(async move { pending_router.route(&mut context()).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        std::fs::write(&path, "return function(_) return 'new' end").unwrap();
        reload_script(&router.inner, &path, Arc::new(ResolverManager::new()));
        assert_eq!(router.route(&mut context()).await.unwrap(), "new");
        resume.notify_one();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), pending)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "old"
        );
    }

    #[cfg(not(feature = "router-db"))]
    #[tokio::test]
    async fn database_helpers_are_absent_without_router_db() {
        let router = Router::from_source(
            r#"assert(find_domain == nil and find_ip_v4 == nil and find_ip_v6 == nil)
               return function(_) return "direct" end"#,
        )
        .unwrap();
        assert_eq!(router.route(&mut context()).await.unwrap(), "direct");
    }

    #[cfg(feature = "router-db")]
    #[tokio::test]
    async fn database_helpers_remain_available_after_script_reload() {
        use crate::config::{GeositeDbCfg, RouterDatabaseCfg};
        use crate::plugin::database::RedbDatabase;
        let dir = tempfile::tempdir().unwrap();
        let cfg = RouterDatabaseCfg::Geosite(GeositeDbCfg {
            list_include: Vec::new(),
            tag: "site".into(),
            url: "https://example.test/db".into(),
            path: dir.path().join("site.redb"),
        });
        let source = dir.path().join("source.yml");
        std::fs::write(&source, "lists: [{name: test, rules: ['domain:example']}] ").unwrap();
        drop(RedbDatabase::import(&cfg, &source).unwrap());
        let databases = Databases::build(&[cfg], &mut Default::default()).unwrap();
        let resolver = Arc::new(ResolverManager::new());
        let path = dir.path().join("router.lua");
        std::fs::write(&path, "assert(find_domain('site', 'test', 'api.example')); return function(ctx) return 'old' end").unwrap();
        let router = Router::load_with_databases(
            &path,
            resolver.clone(),
            databases,
            #[cfg(feature = "router-dhcp-lease")]
            Arc::default(),
        )
        .unwrap();
        assert_eq!(router.route(&mut context()).await.unwrap(), "old");
        std::fs::write(&path, "return function(ctx) if find_domain('site', 'test', ctx.dst_domain) then return 'new' end end").unwrap();
        reload_script(&router.inner, &path, resolver);
        assert_eq!(router.route(&mut context()).await.unwrap(), "new");
    }

    #[cfg(not(feature = "router-dhcp-lease"))]
    #[tokio::test]
    async fn dhcp_helpers_are_absent_without_feature() {
        let router = Router::from_source(
            r#"
            assert(find_dhcp_mac_v4 == nil and find_dhcp_host_v4 == nil)
            assert(find_dhcp_mac_v6 == nil and find_dhcp_host_v6 == nil)
            assert(find_dhcp_duid_v6 == nil and find_dhcp_iaid_v6 == nil)
            return function(_) return "direct" end
        "#,
        )
        .unwrap();
        assert_eq!(router.route(&mut context()).await.unwrap(), "direct");
    }

    #[cfg(feature = "router-dhcp-lease")]
    #[tokio::test]
    async fn dhcp_store_survives_script_reload() {
        use crate::config::{DhcpLeaseCfg, DnsmasqLeaseCfg};
        let dir = tempfile::tempdir().unwrap();
        let leases_path = dir.path().join("leases");
        std::fs::write(&leases_path, "0 02:00:00:00:00:01 192.0.2.1 known *").unwrap();
        let leases = super::super::dhcp_lease::LeaseStore::build(&[DhcpLeaseCfg::Dnsmasq(
            DnsmasqLeaseCfg {
                tag: "lan".into(),
                path: leases_path,
            },
        )])
        .unwrap();
        let resolver = Arc::new(ResolverManager::new());
        let path = dir.path().join("router.lua");
        std::fs::write(&path, "assert(find_dhcp_host_v4('lan', '192.0.2.1') == 'known'); return function(_) return 'old' end").unwrap();
        let router = Router::load_with_databases(
            &path,
            resolver.clone(),
            #[cfg(feature = "router-db")]
            Arc::default(),
            leases.clone(),
        )
        .unwrap();
        assert_eq!(router.route(&mut context()).await.unwrap(), "old");
        std::fs::write(
            &path,
            "return function(_) return find_dhcp_host_v4('lan', '192.0.2.1') end",
        )
        .unwrap();
        reload_script(&router.inner, &path, resolver);
        assert_eq!(router.route(&mut context()).await.unwrap(), "known");
        assert!(Arc::ptr_eq(&router.inner.lock().unwrap().leases, &leases));
    }

    #[tokio::test]
    async fn lua_router_can_return_a_routing_error() {
        let router =
            Router::from_source(r#"return function(_) return nil, "blocked by policy" end"#)
                .unwrap();

        let error = router.route(&mut context()).await.unwrap_err();
        assert!(error.to_string().contains("blocked by policy"));
    }

    #[tokio::test]
    async fn lua_router_rejects_invalid_return_values() {
        let router = Router::from_source(r#"return function(_) return nil end"#).unwrap();

        let error = router.route(&mut context()).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("router must return an outbound tag or nil and an error message")
        );
    }
}
