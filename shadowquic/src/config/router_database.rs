use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A downloaded routing database, converted to an indexed redb file.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct RouterDBCfg {
    /// Unique tag, also used as the inbound tag of download connections.
    pub tag: String,
    #[serde(rename = "type")]
    pub kind: RouterDBKind,
    /// HTTP(S) source URL. Used only when `path` does not exist.
    pub url: String,
    /// Persistent converted redb file, relative to the working directory.
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RouterDBKind {
    Country,
    Geosite,
}
