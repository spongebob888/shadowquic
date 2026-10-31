use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn default_country_url() -> String {
    "https://git.io/GeoLite2-Country.mmdb".into()
}
pub fn default_geosite_url() -> String {
    "https://github.com/v2fly/domain-list-community/releases/latest/download/dlc.dat_plain.yml"
        .into()
}
/// A routing database stored as a country MMDB or an indexed redb file.
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
    /// Persistent file, relative to the working directory. A `.mmdb` suffix
    /// selects direct MMDB lookups; other suffixes retain redb conversion.
    /// Cost about 3mb for mmdb file. Redb format not recommended for country database, as it is slower and larger.
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
    /// Base domain sets to import, matched case insensitively. Empty imports all sets.
    #[serde(default)]
    pub list_include: Vec<String>,
    /// Persistent converted redb file, relative to the working directory.
    /// Cost about 16m for full geosite db. 1Mb for only cn list.
    pub path: PathBuf,
}

impl RouterDatabaseCfg {
    #[cfg(any(test, feature = "router-db"))]
    pub(crate) fn uses_mmdb(&self) -> bool {
        self.kind() == RouterDBKind::Country
            && self
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("mmdb"))
    }

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
    fn geosite_list_defaults_and_round_trips() {
        for (field, expected) in [
            ("", vec![]),
            ("list-include: []", vec![]),
            (
                "list-include: [google, geolocation-cn]",
                vec!["google", "geolocation-cn"],
            ),
        ] {
            let cfg: RouterDatabaseCfg = serde_saphyr::from_str(&format!(
                "type: geosite\ntag: db\npath: db.redb\n{field}\n"
            ))
            .unwrap();
            let encoded = serde_saphyr::to_string(&cfg).unwrap();
            let RouterDatabaseCfg::Geosite(decoded) =
                serde_saphyr::from_str::<RouterDatabaseCfg>(&encoded).unwrap()
            else {
                unreachable!()
            };
            assert_eq!(decoded.list_include, expected);
        }
    }

    #[test]
    fn mmdb_suffix_selects_only_country_backend() {
        for (kind, path, expected) in [
            ("country", "data/country.mmdb", true),
            ("country", "data/country.MMDB", true),
            ("country", "data/country.redb", false),
            ("country", "data/country", false),
            ("country", "data/country.mmdb.redb", false),
            ("geosite", "data/site.mmdb", false),
        ] {
            let cfg: RouterDatabaseCfg =
                serde_saphyr::from_str(&format!("type: {kind}\ntag: db\npath: {path}\n")).unwrap();
            assert_eq!(cfg.uses_mmdb(), expected, "{kind}: {path}");
        }
    }

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
