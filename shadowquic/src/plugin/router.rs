//! Optional per-request routing with a sandboxed Luau script.
//!
//! Enable the `plugin` Cargo feature and put source code directly in `router`:
//!
//! ```yaml
//! router: |
//!   return function(ctx)
//!       return "direct"
//!   end
//! ```
//! Alternatively, set `router-script: router.luau` to read the source from a
//! file. Configure only one of these fields.
//!
//! The script must return a function. ShadowQUIC calls it with one context
//! table containing `dst_domain`, `dst_ip_v4`, `dst_ip_v6`, `dst_port`, and
//! optional `stats_context` fields. IP values are strings. When the request is
//! authenticated, `stats_context` contains `username` and `conn_id`; otherwise
//! it is nil.
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
//! The router script runs in mlua's Luau sandbox. See [`Router`] and
//! [`RouteContext`] for the Rust interfaces.

use std::{
    net::{Ipv4Addr, Ipv6Addr},
    path::Path,
    sync::Mutex,
};

use mlua::{Function, Lua, Table};

use crate::{
    ProxyRequest, StatsContext, TcpSession, UdpSession,
    error::SError,
    msgs::socks5::{AddrOrDomain, SocksAddr},
};

/// Request information exposed to a routing script.
pub struct RouteContext {
    pub dst_domain: Option<String>,
    pub dst_ip_v4: Option<Ipv4Addr>,
    pub dst_ip_v6: Option<Ipv6Addr>,
    pub dst_port: Option<u16>,
    pub stats_context: Option<StatsContext>,
}

impl RouteContext {
    pub(crate) fn from_request(req: &ProxyRequest) -> Self {
        match req {
            ProxyRequest::Tcp(TcpSession {
                dst, user_context, ..
            }) => Self::from_dst(dst, user_context.stats.clone()),
            ProxyRequest::Udp(UdpSession {
                dst, user_context, ..
            }) => Self::from_dst(dst, user_context.stats.clone()),
        }
    }

    fn from_dst(dst: &SocksAddr, stats_context: Option<StatsContext>) -> Self {
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
            stats_context,
        }
    }

    fn to_lua(&self, lua: &Lua) -> mlua::Result<Table> {
        let context = lua.create_table()?;
        context.set("dst_domain", self.dst_domain.as_deref())?;
        context.set("dst_ip_v4", self.dst_ip_v4.map(|addr| addr.to_string()))?;
        context.set("dst_ip_v6", self.dst_ip_v6.map(|addr| addr.to_string()))?;
        context.set("dst_port", self.dst_port)?;

        let stats = if let Some(stats) = &self.stats_context {
            let table = lua.create_table()?;
            table.set("username", stats.username.as_str())?;
            table.set("conn_id", stats.conn_id)?;
            Some(table)
        } else {
            None
        };
        context.set("stats_context", stats)?;
        Ok(context)
    }
}

/// A sandboxed Luau router. The script returns a function that accepts one
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
        let lua = Lua::new();
        lua.sandbox(true)?;
        let route = lua.load(source).eval::<Function>()?;
        Ok(Self {
            inner: Mutex::new(RouterInner { lua, route }),
        })
    }

    pub fn route(&self, context: &RouteContext) -> Result<String, SError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| SError::RouterError("router runtime lock was poisoned".into()))?;
        let context = context
            .to_lua(&inner.lua)
            .map_err(|error| SError::RouterError(error.to_string()))?;
        let (tag, error): (Option<String>, Option<String>) = inner
            .route
            .call(context)
            .map_err(|error| SError::RouterError(error.to_string()))?;
        match (tag, error) {
            (Some(tag), _) if !tag.trim().is_empty() => Ok(tag),
            (_, Some(error)) if !error.trim().is_empty() => Err(SError::RouterError(error)),
            _ => Err(SError::RouterError(
                "router must return an outbound tag or nil and an error message".into(),
            )),
        }
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
            stats_context: None,
        }
    }

    #[test]
    fn lua_router_receives_context_and_returns_outbound_tag() {
        let router = Router::from_source(
            r#"
                return function(ctx)
                    if ctx.dst_domain == "api.example" and ctx.dst_port == 443 then
                        return "secure"
                    end
                    return "direct"
                end
            "#,
        )
        .unwrap();

        assert_eq!(router.route(&context()).unwrap(), "secure");
    }

    #[test]
    fn lua_router_can_return_a_routing_error() {
        let router =
            Router::from_source(r#"return function(_) return nil, "blocked by policy" end"#)
                .unwrap();

        let error = router.route(&context()).unwrap_err();
        assert!(error.to_string().contains("blocked by policy"));
    }

    #[test]
    fn lua_router_rejects_invalid_return_values() {
        let router = Router::from_source(r#"return function(_) return nil end"#).unwrap();

        let error = router.route(&context()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("router must return an outbound tag or nil and an error message")
        );
    }
}
