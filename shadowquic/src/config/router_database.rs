use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn default_country_url() -> String {
    "https://git.io/GeoLite2-Country.mmdb".into()
}
pub fn default_geosite_url() -> String {
    "https://github.com/v2fly/domain-list-community/releases/latest/download/dlc.dat_plain.yml"
        .into()
}

fn default_cache_size() -> usize {
    4 * 1024 * 1024
}
/// A routing database stored as a country MMDB or an indexed redb file.
///
/// Example configuration:
/// ```yaml
/// router:
///   database:
///     # GeoLite2 Country (MMDB) for CN IP ranges. Downloaded on first start.
///     - tag: geoip-db
///       type: country
///       path: data/geolite2-country.mmdb
///     # v2fly domain-list-community for CN domains. Downloaded on first start.
///     - tag: geosite-db
///       type: geosite
///       list-include: ["cn", "geolocation-cn"]
///       path: data/geosite.redb
///
///   src: |
///     return function(ctx)
///       -- CN domain names.
///       if ctx.dst_domain then
///         local ok, cn = pcall(find_domain, "geosite-db", "cn", ctx.dst_domain)
///         if ok and cn then return "direct" end
///       end
///       -- CN IP ranges from the country database (the primary path for tproxy).
///       if ctx.dst_ip_v4 then
///         local ok, cn = pcall(find_ip_v4, "geoip-db", "cn", ctx.dst_ip_v4)
///         if ok and cn then return "direct" end
///       end
///       if ctx.dst_ip_v6 then
///         local ok, cn = pcall(find_ip_v6, "geoip-db", "cn", ctx.dst_ip_v6)
///         if ok and cn then return "direct" end
///       end
///
///       -- Everything else (non-CN public traffic) goes through shadowquic.
///       return "select"
///     end
///
/// ```
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum RouterDatabaseCfg {
    Country(CountryDbCfg),
    Geosite(GeositeDbCfg),
    Custom(CustomDbCfg),
}

/// Country/category IP database sourced from MMDB. Records may contain
/// `country.iso_code`, a single string, or an array of string labels.
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
    /// redb page-cache budget in bytes. Defaults to 4 MiB; zero disables caching.
    #[serde(default = "default_cache_size")]
    pub cache_size: usize,
}

/// Persistent domain and IP lists defined in the router configuration.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct CustomDbCfg {
    pub tag: String,
    #[serde(default = "default_custom_path")]
    pub path: PathBuf,
    pub list: Vec<CustomRuleList>,
    /// redb page-cache budget in bytes. Defaults to 128 KiB. Zero disables caching.
    #[serde(default = "default_custom_cache_size")]
    pub cache_size: usize,
}

fn default_custom_cache_size() -> usize {
    128 * 1024
}

fn default_custom_path() -> PathBuf {
    "data/custom.redb".into()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CustomRuleList {
    pub name: String,
    /// Replace the list's tables at startup. When false, only seed missing tables.
    #[serde(default = "default_force")]
    pub force: bool,
    pub rules: Vec<String>,
}

fn default_force() -> bool {
    true
}

impl RouterDatabaseCfg {
    pub(crate) fn cache_size(&self) -> usize {
        match self {
            Self::Country(_) => default_cache_size(),
            Self::Custom(cfg) => cfg.cache_size,
            Self::Geosite(cfg) => cfg.cache_size,
        }
    }

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
            Self::Custom(cfg) => &cfg.tag,
        }
    }

    /// Download source, or an empty string for inline custom databases.
    pub fn url(&self) -> &str {
        match self {
            Self::Country(cfg) => &cfg.url,
            Self::Geosite(cfg) => &cfg.url,
            Self::Custom(_) => "",
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Country(cfg) => &cfg.path,
            Self::Geosite(cfg) => &cfg.path,
            Self::Custom(cfg) => &cfg.path,
        }
    }

    pub fn kind(&self) -> RouterDBKind {
        match self {
            Self::Country(_) => RouterDBKind::Country,
            Self::Geosite(_) => RouterDBKind::Geosite,
            Self::Custom(_) => RouterDBKind::Custom,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RouterDBKind {
    Country,
    Geosite,
    Custom,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geosite_cache_size_defaults_and_round_trips_in_bytes() {
        for (field, expected) in [
            ("", 4 * 1024 * 1024),
            ("cache-size: 0", 0),
            ("cache-size: 16777216", 16 * 1024 * 1024),
        ] {
            let cfg: RouterDatabaseCfg = serde_saphyr::from_str(&format!(
                "type: geosite\ntag: db\npath: db.redb\n{field}\n"
            ))
            .unwrap();
            assert_eq!(cfg.cache_size(), expected);
            let encoded = serde_saphyr::to_string(&cfg).unwrap();
            let RouterDatabaseCfg::Geosite(decoded) =
                serde_saphyr::from_str::<RouterDatabaseCfg>(&encoded).unwrap()
            else {
                unreachable!()
            };
            assert_eq!(decoded.cache_size, expected);
            assert!(encoded.contains(&format!("cache-size: {expected}")));
        }
        for value in ["-1", "1.5", "8MiB"] {
            assert!(
                serde_saphyr::from_str::<RouterDatabaseCfg>(&format!(
                    "type: geosite\ntag: db\npath: db.redb\ncache-size: {value}\n"
                ))
                .is_err()
            );
        }
        let country: RouterDatabaseCfg =
            serde_saphyr::from_str("type: country\ntag: db\npath: db.redb\n").unwrap();
        assert_eq!(country.cache_size(), 4 * 1024 * 1024);
        assert!(
            serde_saphyr::from_str::<RouterDatabaseCfg>(
                "type: country\ntag: db\npath: db.redb\ncache-size: 1024\n"
            )
            .is_err()
        );
    }

    #[test]
    fn custom_cache_size_defaults_and_round_trips_in_bytes() {
        for (field, expected) in [
            ("", 128 * 1024),
            ("cache-size: 0", 0),
            ("cache-size: 65536", 65536),
        ] {
            let cfg: RouterDatabaseCfg =
                serde_saphyr::from_str(&format!("type: custom\ntag: local\nlist: []\n{field}\n"))
                    .unwrap();
            assert_eq!(cfg.cache_size(), expected);
            let encoded = serde_saphyr::to_string(&cfg).unwrap();
            assert!(encoded.contains(&format!("cache-size: {expected}")));
            let decoded: RouterDatabaseCfg = serde_saphyr::from_str(&encoded).unwrap();
            assert_eq!(decoded.cache_size(), expected);
        }
    }

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
