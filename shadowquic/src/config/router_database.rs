use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn default_country_url() -> String {
    "https://git.io/GeoLite2-Country.mmdb".into()
}
pub fn default_geosite_url() -> String {
    "https://github.com/v2fly/domain-list-community/releases/latest/download/dlc.dat_plain.yml".into()
}
/// A downloaded routing database, converted to an indexed redb file.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum RouterDatabaseCfg {
    Country(CountryDbCfg),
    Geosite(GeositeDbCfg),
}

/// Country IP database sourced from MMDB.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct CountryDbCfg {
    /// Unique tag, also used as the inbound tag of download connections.
    pub tag: String,
    /// HTTP(S) source URL. Used only when `path` does not exist.
    /// Defaults to the MaxMind GeoLite2 Country database
    /// MMDB format.
    #[serde(default = "default_country_url")]
    pub url: String,
    /// Persistent converted redb file, relative to the working directory.
    pub path: PathBuf,
}

/// Domain membership database sourced from geosite YAML.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct GeositeDbCfg {
    /// Unique tag, also used as the inbound tag of download connections.
    pub tag: String,
    /// HTTP(S) source URL. Used only when `path` does not exist.
    /// Defaults to the v2fly domain-list-community database in YAML format.
    #[serde(default = "default_geosite_url")]
    pub url: String,
    /// Persistent converted redb file, relative to the working directory.
    pub path: PathBuf,
}

impl RouterDatabaseCfg {
    pub fn tag(&self) -> &str {
        match self {
            Self::Country(cfg) => &cfg.tag,
            Self::Geosite(cfg) => &cfg.tag,
        }
    }

    pub fn url(&self) -> &str {
        match self {
            Self::Country(cfg) => &cfg.url,
            Self::Geosite(cfg) => &cfg.url,
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Country(cfg) => &cfg.path,
            Self::Geosite(cfg) => &cfg.path,
        }
    }

    pub fn kind(&self) -> RouterDBKind {
        match self {
            Self::Country(_) => RouterDBKind::Country,
            Self::Geosite(_) => RouterDBKind::Geosite,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RouterDBKind {
    Country,
    Geosite,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_variants_preserve_yaml_format() {
        for (name, kind) in [
            ("country", RouterDBKind::Country),
            ("geosite", RouterDBKind::Geosite),
        ] {
            let yaml =
                format!("type: {name}\ntag: db\nurl: https://example.test/db\npath: db.redb\n");
            let cfg: RouterDatabaseCfg = serde_saphyr::from_str(&yaml).unwrap();
            assert_eq!(cfg.kind(), kind);
            assert_eq!(cfg.tag(), "db");
            assert_eq!(cfg.url(), "https://example.test/db");
            assert_eq!(cfg.path(), Path::new("db.redb"));
            let encoded = serde_saphyr::to_string(&cfg).unwrap();
            assert!(encoded.contains(&format!("type: {name}")));
            let decoded: RouterDatabaseCfg = serde_saphyr::from_str(&encoded).unwrap();
            assert_eq!(decoded.kind(), kind);
            assert_eq!(decoded.tag(), cfg.tag());
            assert_eq!(decoded.url(), cfg.url());
            assert_eq!(decoded.path(), cfg.path());

            for invalid in [
                format!("{yaml}unknown: value\n"),
                yaml.replace("path: db.redb\n", ""),
                yaml.replace(&format!("type: {name}\n"), ""),
                yaml.replace(&format!("type: {name}"), "type: unknown"),
            ] {
                assert!(serde_saphyr::from_str::<RouterDatabaseCfg>(&invalid).is_err());
            }
        }
    }
}
