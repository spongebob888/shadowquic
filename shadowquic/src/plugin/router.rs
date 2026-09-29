//! Optional per-request routing with a restricted Lua script.
//!
//! Enable the `plugin` Cargo feature and put source code directly in `router`:
//!
//! ```yaml
//! router: |
//!   return function(ctx)
//!       return "direct"
//!   end
//! ```
//! Alternatively, set `router-script: router.lua` to read the source from a
//! file. Configure only one of these fields.
//!
//! The script must return a function. ShadowQUIC calls it with one context
//! userdata containing `inbound_tag`, `network_type` (`"tcp"` or `"udp"`), `dst_domain`,
//! `dst_ip_v4`, `dst_ip_v6`, `dst_port`, and
//! source address fields `src_addr`, `src_ip_v4`, `src_ip_v6`, and `src_port`,
//! plus optional `stats_context`. Address values are strings. Source fields
//! are nil when the inbound does not provide a source address. When the request
//! is authenticated, `stats_context` contains `username` and `conn_id`; otherwise
//! it is nil. Scripts may update `dst_domain`, `dst_ip_v4`, `dst_ip_v6`, and
//! `dst_port`; setting one destination name or IP clears the other name/IP fields.
//!
//! Return an outbound tag to route the request, or `nil, error_message` to
//! reject routing. The tag must match an outbound in the config.
//!
//! ```lua
//! return function(ctx)
//!     if ctx.dst_domain and string.match(ctx.dst_domain, "%.example$") then
//!         return "special-proxy"
//!     end
//!     return "direct"
//! end
//! ```
//!
//! Routing scripts have base language functions and string, table, and math
//! helpers, without filesystem, process, or module loading access. Console
//! output through `print` is allowed.
//! LuaJIT builds also provide the bit library.
//! See [`Router`] and [`RouteContext`] for the Rust interfaces.

use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    sync::Mutex,
};

use mlua::{Function, Lua, LuaOptions, StdLib, UserData, UserDataFields, chunk::ChunkMode};

use crate::{
    ProxyRequest, StatsContext, TcpSession, UdpSession,
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
    pub dst_domain: Option<String>,
    pub dst_ip_v4: Option<Ipv4Addr>,
    pub dst_ip_v6: Option<Ipv6Addr>,
    pub dst_port: Option<u16>,
    pub src_addr: Option<SocketAddr>,
    pub src_ip_v4: Option<Ipv4Addr>,
    pub src_ip_v6: Option<Ipv6Addr>,
    pub src_port: Option<u16>,
    pub inbound_tag: String,
    /// Only valid for shadowquic/sunnyquic inbound requests.
    pub stats_context: Option<StatsContext>,
    pub network_type: NetworkType,
}
impl UserData for RouteContext {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("inbound_tag", |_, this| Ok(this.inbound_tag.clone()));
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
            this.dst_domain = value;
            if this.dst_domain.is_some() {
                this.dst_ip_v4 = None;
                this.dst_ip_v6 = None;
            }
            Ok(())
        });
        fields.add_field_method_set("dst_ip_v4", |_, this, value: Option<String>| {
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
                NetworkType::Tcp,
                user_context.stats.clone(),
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
                NetworkType::Udp,
                user_context.stats.clone(),
            ),
        }
    }

    fn from_dst(
        dst: &SocksAddr,
        src_addr: Option<SocketAddr>,
        inbound_tag: &str,
        network_type: NetworkType,
        stats_context: Option<StatsContext>,
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
            stats_context,
            network_type,
        }
    }
}

/// A restricted Lua router. The script returns a function that accepts one
/// RouteContext table and returns an outbound tag or nil plus an error message.
pub struct Router {
    inner: Mutex<RouterInner>,
}

struct RouterInner {
    lua: Lua,
    route: Function,
}

impl Router {
    pub fn load(path: &Path) -> Result<Self, SError> {
        let script = std::fs::read_to_string(path).map_err(|error| {
            SError::InvalidConfig(format!(
                "failed to read router script {}: {error}",
                path.display()
            ))
        })?;
        Self::from_source(&script).map_err(|error| {
            SError::InvalidConfig(format!(
                "failed to load router script {}: {error}",
                path.display()
            ))
        })
    }

    pub(crate) fn from_source(source: &str) -> mlua::Result<Self> {
        let libs = StdLib::STRING | StdLib::TABLE | StdLib::MATH;
        let libs = libs | StdLib::BIT;
        let lua = Lua::new_with(libs, LuaOptions::default())?;
        // The base library is always loaded, including file and code loaders.
        // Remove these before evaluating any user-provided source.
        let globals = lua.globals();
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
        Ok(Self {
            inner: Mutex::new(RouterInner { lua, route }),
        })
    }

    pub fn route(&self, context: &mut RouteContext) -> Result<String, SError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SError::RouterError("router runtime lock was poisoned".into()))?;
        let userdata = inner
            .lua
            .create_userdata(context.clone())
            .map_err(|error| SError::RouterError(error.to_string()))?;
        let (tag, error): (Option<String>, Option<String>) = inner
            .route
            .call(userdata.clone())
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
            stats_context: None,
            network_type: NetworkType::Tcp,
        }
    }

    #[test]
    fn lua_router_restricts_capabilities_during_load_and_routing() {
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

        assert_eq!(router.route(&mut context()).unwrap(), "direct-1");
    }

    #[test]
    #[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
    fn lua_router_supports_bit_operations() {
        let router = Router::from_source(
            r#"
                return function(ctx)
                    assert(bit.band(ctx.dst_port, 255) == 187)
                    return "direct"
                end
            "#,
        )
        .unwrap();

        assert_eq!(router.route(&mut context()).unwrap(), "direct");
    }

    #[test]
    fn lua_router_receives_context_and_returns_outbound_tag() {
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
        assert_eq!(router.route(&mut context).unwrap(), "secure");
        let rewritten = context.destination().unwrap();
        assert_eq!(rewritten.to_string(), "192.0.2.44:8443");
    }

    #[test]
    fn lua_router_can_return_a_routing_error() {
        let router =
            Router::from_source(r#"return function(_) return nil, "blocked by policy" end"#)
                .unwrap();

        let error = router.route(&mut context()).unwrap_err();
        assert!(error.to_string().contains("blocked by policy"));
    }

    #[test]
    fn lua_router_rejects_invalid_return_values() {
        let router = Router::from_source(r#"return function(_) return nil end"#).unwrap();

        let error = router.route(&mut context()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("router must return an outbound tag or nil and an error message")
        );
    }
}
