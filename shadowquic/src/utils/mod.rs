pub(crate) mod copy;
pub mod dual_socket;
pub mod platform;
pub mod socket_opt;
pub mod user_store;

#[cfg(any(feature = "router-db", feature = "restful-api"))]
pub(crate) mod download;
#[cfg(any(feature = "router-db", feature = "restful-api"))]
pub(crate) mod http;
#[cfg(any(feature = "router-db", feature = "restful-api", feature = "dns-server"))]
pub(crate) mod proxy;
#[cfg(any(feature = "router-db", feature = "dns-server", feature = "restful-api"))]
pub(crate) mod tls;

#[cfg(target_os = "android")]
pub mod protect_socket;

pub mod replay_stream;
