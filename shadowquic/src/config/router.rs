use serde::{Deserialize, Serialize};

use crate::error::SError;
#[cfg(feature = "plugin")]
use crate::plugin::router::Router;

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
///
/// The script must return a function. Each request passes one context userdata:
///
/// | Field | Value |
/// | --- | --- |
/// | `inbound_tag` | Tag of the inbound listener |
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
/// Return a configured outbound tag to route the request, or `nil, error_message`
/// to reject it. Routing errors do not fall back to `router.default-outbound`.
///
/// Scripts have base language functions and string, table, math, and bit helpers.
/// Filesystem, process, module loading, and dynamic code loading are unavailable.
/// Console output through `print` is allowed.
///
/// The exposed router context to script can be seen in [`crate::plugin::router::RouteContext`]
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RouterCfg {
    /// Outbound tag used when no routing script is configured. Defaults to the
    /// first configured outbound. Does not require the `plugin` feature.
    #[serde(default)]
    pub default_outbound: Option<String>,
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
        #[cfg(not(feature = "plugin"))]
        if self.src.is_some() || self.path.is_some() {
            return Err(SError::InvalidConfig(
                "router config requires building with the `plugin` feature".into(),
            ));
        }
        Ok(())
    }

    #[cfg(feature = "plugin")]
    pub(super) fn build(&self) -> Result<Option<Router>, SError> {
        self.validate()?;
        match (self.src.as_deref(), self.path.as_deref()) {
            (Some(source), None) => Router::from_source(source).map(Some).map_err(|error| {
                SError::InvalidConfig(format!("failed to load inline router script: {error}"))
            }),
            (None, Some(path)) => Router::load(path).map(Some),
            (None, None) => Ok(None),
            (Some(_), Some(_)) => unreachable!("validated above"),
        }
    }
}
