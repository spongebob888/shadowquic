use serde::{Deserialize, Serialize};

#[cfg(feature = "plugin")]
use crate::dns::ResolverManager;
use crate::error::SError;
#[cfg(feature = "plugin")]
use crate::plugin::router::Router;
#[cfg(feature = "plugin")]
use std::sync::Arc;

/// Request routing through a default outbound or a restricted Lua script.
///
/// Without a script, requests use `router.default-outbound`, or the first
/// configured outbound if omitted. This works without the `plugin` feature.
/// Omit `router` or use `router: {}` to use the first outbound directly.
///
/// ```yaml
/// router:
///   default-outbound: direct
/// ```
///
/// Script routing requires the `plugin` Cargo feature. Configure either inline
/// source in `src` or a script file in `path`, never both. A configured script
/// selects the outbound instead of `default-outbound`.
///
/// ```yaml
/// dns:
///   - tag: dns-in
///     type: dns-udp
///     bind-addr: 0.0.0.0:5553
/// router:
///   src: |
///     return function(ctx)
///       if ctx.dst_domain and ctx.dst_domain:match("%.example$") then
///         return "special-proxy"
///       end
///
///       if ctx.dst_ip_v4 and ctx.dst_ip_v4:sub(1, #"192.168") == "192.168" then
///         return "sq-home"
///       end
///
///       if ctx.dst_port == 53 and ctx.network_type == "udp" then
///         return "dns-in" -- dns hijacking
///       end
///
///       return "direct"
///     end
/// ```
///
/// Alternatively, load a file (relative paths resolve from the working directory):
///
/// ```yaml
/// router:
///   path: router.lua
/// ```
///
/// File changes reload automatically for subsequent requests, including when an
/// editor replaces the file. Failed reloads keep the last working script. A
/// successful reload resets Lua state; existing connections are unaffected.
/// Inline scripts are not watched.
/// Routing is asynchronous: suspended calls keep their original script runtime
/// when a reload occurs, while new calls use the replacement.
///
/// The script must return a function. Each request passes one context userdata:
///
/// | Field | Value |
/// | --- | --- |
/// | `inbound_tag` | Tag of the inbound listener |
/// | `dns_query` | Array of tables with `name` and numeric `record_type`; empty when no DNS metadata is attached |
/// | `network_type` | `"tcp"` or `"udp"` |
/// | `dst_domain`, `dst_ip_v4`, `dst_ip_v6` | Destination name or IP string; unused fields are nil |
/// | `dst_port` | Destination port number |
/// | `src_addr`, `src_ip_v4`, `src_ip_v6` | Source address strings, or nil when unavailable |
/// | `src_port` | Source port number, or nil when unavailable |
/// | `stats_context` | Table with `username` and `conn_id` for authenticated QUIC requests, otherwise nil |
///
/// Scripts may update `dst_domain`, `dst_ip_v4`, `dst_ip_v6`, and `dst_port` only
/// for TCP requests. Writing these fields for UDP requests raises an error,
/// including assigning nil.
/// Setting a destination name or IP clears the other destination address fields.
/// `dns_query` is a snapshot: changing its tables does not modify the request.
/// Return a configured outbound tag to route the request, or `nil, error_message`
/// to reject it. Routing errors do not fall back to `router.default-outbound`.
///
/// Scripts have base language functions and string, table, math, and bit helpers.
/// Filesystem, process, module loading, and dynamic code loading are unavailable.
///
/// | API | Behavior | Availability |
/// | --- | --- | --- |
/// | `print(...)` | Writes console output | Script loading and routing |
/// | `info(message)` | Accepts a string and emits a `tracing::info!` log using the application's logging filters | Script loading and routing |
/// | `lookup(dns_tag, domain)` | Returns an array of IP strings and may block router. | Inside the returned routing function; requires `dns-server` |
/// | `reverse_lookup(dns_tag, ip)` | Returns an array of PTR hostname strings and may block router| Inside the returned routing function; requires `dns-server` |
/// | `lookup_cache(domain)` | Returns an array of cached IP strings, or an empty array on a miss; domain names are case insensitive | Script loading and routing; requires `dns-server` |
/// | `reverse_lookup_cache(ip)` | Returns the most recently cached hostname from matching A/AAAA or PTR answers, or nil on a miss; invalid IP strings raise a Lua error | Script loading and routing; requires `dns-server` |
///
/// Cache lookups use the shared DNS cache, ignore expired entries, and perform
/// no network I/O or asynchronous suspension. They do not take a DNS service tag.
/// `lookup` and `reverse_lookup` suspend the routing function and raise Lua errors
/// on failure. Route DNS upstream requests
///
/// Database membership helpers require `router.database` entries (see
/// [`super::RouterDatabaseCfg`]). `find_domain(tag, list, domain)`,
/// `find_ip_v4(tag, list, ip)`, and `find_ip_v6(tag, list, ip)` return booleans.
/// Missing databases download through an internal inbound with the database tag.
/// Route that traffic before calling helpers. Unavailable databases raise Lua
/// errors; use `pcall` for an explicit fallback while downloading. Existing redb
/// files are reused, including across script reloads.
///
/// The exposed router context to script can be seen in [`crate::plugin::router::RouteContext`]
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RouterCfg {
    /// Outbound tag used when no routing script is configured. Defaults to the
    /// first configured outbound. Does not require the `plugin` feature.
    #[serde(default)]
    pub default_outbound: Option<String>,
    /// Persistent databases available to Lua membership helpers.
    #[serde(default)]
    pub database: Vec<super::RouterDatabaseCfg>,
    /// Inline Lua source returning a routing function. Mutually exclusive with `path`.
    #[serde(default)]
    pub src: Option<String>,
    /// Path to a Lua script, watched for changes. Mutually exclusive with `src`.
    #[serde(default)]
    pub path: Option<std::path::PathBuf>,
}

impl RouterCfg {
    pub(super) fn validate(&self) -> Result<(), SError> {
        if self.src.is_some() && self.path.is_some() {
            return Err(SError::InvalidConfig(
                "configure either `router.src` or `router.path`, not both".into(),
            ));
        }
        #[cfg(not(feature = "router-db"))]
        if !self.database.is_empty() {
            return Err(SError::InvalidConfig(
                "router.database requires building with the `router-db` feature".into(),
            ));
        }
        #[cfg(not(feature = "plugin"))]
        if self.src.is_some() || self.path.is_some() || !self.database.is_empty() {
            return Err(SError::InvalidConfig(
                "router config requires building with the `plugin` feature".into(),
            ));
        }
        let mut paths = std::collections::HashSet::new();
        for db in &self.database {
            if db.path().as_os_str().is_empty() || !paths.insert(db.path()) {
                return Err(SError::InvalidConfig(
                    "router database paths must be nonempty and unique".into(),
                ));
            }
            #[cfg(feature = "router-db")]
            crate::plugin::database::validate_url(db.url())?;
        }
        Ok(())
    }

    #[cfg(feature = "plugin")]
    pub(super) fn build(
        &self,
        resolver_manager: Arc<ResolverManager>,
        #[cfg(feature = "router-db")] databases: Arc<crate::plugin::database::Databases>,
    ) -> Result<Option<Router>, SError> {
        self.validate()?;
        match (self.src.as_deref(), self.path.as_deref()) {
            (Some(source), None) => Router::from_source_with_databases(
                source,
                resolver_manager,
                #[cfg(feature = "router-db")]
                databases,
            )
            .map(Some)
            .map_err(|error| {
                SError::InvalidConfig(format!("failed to load inline router script: {error}"))
            }),
            (None, Some(path)) => Router::load_with_databases(
                path,
                resolver_manager,
                #[cfg(feature = "router-db")]
                databases,
            )
            .map(Some),
            (None, None) => Ok(None),
            (Some(_), Some(_)) => unreachable!("validated above"),
        }
    }
}

#[cfg(all(test, not(feature = "router-db")))]
mod tests {
    use super::RouterCfg;

    #[test]
    fn database_configuration_requires_router_db_feature() {
        let config: RouterCfg = serde_saphyr::from_str(
            "database:\n  - type: country\n    tag: country\n    path: country.redb\n",
        )
        .unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("router-db")
        );
    }
}
