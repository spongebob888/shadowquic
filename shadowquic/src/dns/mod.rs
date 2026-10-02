#[cfg(feature = "dns-server")]
mod imple;
#[cfg(feature = "dns-server")]
pub use imple::*;

#[cfg(not(feature = "dns-server"))]
mod unimple;
#[cfg(not(feature = "dns-server"))]
pub use unimple::*;
