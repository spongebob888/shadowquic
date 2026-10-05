//! Disk-backed routing membership databases. Only redb's bounded page cache is
//! retained after import; source records are never retained by the router.
mod download;
#[cfg(test)]
mod tests;

use crate::{
    Inbound,
    config::{RouterDBKind, RouterDatabaseCfg},
    error::SError,
};
use redb::{Database, ReadableDatabase, TableDefinition};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::Path,
    sync::{Arc, RwLock},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const META: TableDefinition<&str, &str> = TableDefinition::new("metadata");
const RULES: TableDefinition<&str, ()> = TableDefinition::new("rules");
const SCHEMA: &str = "1";

/// Unsupported lookup families panic. Lua checks the family before calling.
pub trait RouterDB: Send + Sync {
    fn find_ip(&self, list: &str, ip: IpAddr) -> Result<bool>;
    fn find_domain(&self, list: &str, domain: &str) -> Result<bool>;
}

pub struct RedbDatabase {
    db: Database,
    kind: RouterDBKind,
}

fn kind_name(kind: RouterDBKind) -> &'static str {
    match kind {
        RouterDBKind::Country => "country",
        RouterDBKind::Geosite => "geosite",
    }
}
fn key(list: &str, kind: &str, value: &str) -> String {
    format!("{}\0{kind}\0{value}", list.to_ascii_lowercase())
}
fn ip_key(list: &str, ip: IpAddr, prefix: u8) -> String {
    let (family, bits, value) = match ip {
        IpAddr::V4(ip) => ("4", 32, u32::from(ip) as u128),
        IpAddr::V6(ip) => ("6", 128, u128::from(ip)),
    };
    let value = if prefix == 0 {
        0
    } else {
        value & (u128::MAX << (bits - prefix))
    };
    key(list, family, &format!("{prefix:03}/{value:032x}"))
}
impl RedbDatabase {
    /// Open a converted database, checking format and source identity.
    pub fn open(cfg: &RouterDatabaseCfg) -> Result<Self> {
        let db = Database::builder()
            .set_cache_size(8 * 1024 * 1024)
            .open(cfg.path())?;
        {
            let read = db.begin_read()?;
            let meta = read.open_table(META)?;
            for (name, expected) in [
                ("schema", SCHEMA),
                ("type", kind_name(cfg.kind())),
                ("url", cfg.url()),
            ] {
                let stored = meta.get(name)?;
                let stored = stored.as_ref().map(|value| value.value());
                if stored != Some(expected) {
                    let stored = stored
                        .map(|value| format!("{value:?}"))
                        .unwrap_or_else(|| "<missing>".into());
                    return Err(format!(
                        "database {:?} at {:?} has incompatible {name}: stored {stored}, expected {expected:?}. Remove the database file {:?} and restart Shadowquic to download and rebuild it from {:?}",
                        cfg.tag(), cfg.path(), cfg.path(), cfg.url()
                    )
                    .into());
                }
            }
            if meta.get("sha256")?.is_none() {
                return Err(format!(
                    "database {:?} at {:?} has incomplete metadata (missing sha256). Remove the database file {:?} and restart Shadowquic to download and rebuild it from {:?}",
                    cfg.tag(), cfg.path(), cfg.path(), cfg.url()
                ).into());
            }
            read.open_table(RULES)?;
        }
        Ok(Self {
            db,
            kind: cfg.kind(),
        })
    }

    /// Convert a source into a new redb, then publish the complete file atomically.
    pub fn import(cfg: &RouterDatabaseCfg, source: &Path) -> Result<Self> {
        let parent = cfg
            .path()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let temporary = tempfile::NamedTempFile::new_in(parent)?;
        let db = Database::builder()
            .set_cache_size(8 * 1024 * 1024)
            .create(temporary.path())?;
        let write = db.begin_write()?;
        {
            let mut rules = write.open_table(RULES)?;
            match cfg.kind() {
                RouterDBKind::Geosite => import_geosite(source, &mut rules)?,
                RouterDBKind::Country => import_country(source, &mut rules)?,
            }
            let mut hash = Sha256::new();
            std::io::copy(&mut std::fs::File::open(source)?, &mut hash)?;
            let digest = format!("{:x}", hash.finalize());
            let mut meta = write.open_table(META)?;
            for (name, value) in [
                ("schema", SCHEMA),
                ("version", env!("CARGO_PKG_VERSION")),
                ("sha256", digest.as_str()),
                ("type", kind_name(cfg.kind())),
                ("url", cfg.url()),
            ] {
                meta.insert(name, value)?;
            }
        }
        write.commit()?;
        drop(db);
        temporary.persist_noclobber(cfg.path())?;
        Self::open(cfg)
    }
}

impl RouterDB for RedbDatabase {
    fn find_ip(&self, list: &str, ip: IpAddr) -> Result<bool> {
        assert_eq!(
            self.kind,
            RouterDBKind::Country,
            "geosite does not support IP lookup"
        );
        let read = self.db.begin_read()?;
        let rules = read.open_table(RULES)?;
        let bits = if ip.is_ipv4() { 32 } else { 128 };
        for prefix in (0..=bits).rev() {
            if rules.get(ip_key(list, ip, prefix).as_str())?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn find_domain(&self, list: &str, domain: &str) -> Result<bool> {
        assert_eq!(
            self.kind,
            RouterDBKind::Geosite,
            "country does not support domain lookup"
        );
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        let read = self.db.begin_read()?;
        let rules = read.open_table(RULES)?;
        if rules.get(key(list, "full", &domain).as_str())?.is_some() {
            return Ok(true);
        }
        let mut suffix = domain.as_str();
        loop {
            if rules.get(key(list, "domain", suffix).as_str())?.is_some() {
                return Ok(true);
            }
            match suffix.split_once('.') {
                Some((_, rest)) => suffix = rest,
                None => break,
            }
        }
        for kind in ["keyword", "regexp"] {
            let start = key(list, kind, "");
            for entry in rules.range(start.as_str()..)? {
                let (entry, _) = entry?;
                let Some(pattern) = entry.value().strip_prefix(&start) else {
                    break;
                };
                let found = if kind == "keyword" {
                    domain.contains(pattern)
                } else {
                    regex::Regex::new(pattern)?.is_match(&domain)
                };
                if found {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

#[derive(Deserialize)]
struct Geosite {
    lists: Vec<SiteList>,
}
#[derive(Deserialize)]
struct SiteList {
    name: String,
    rules: Vec<String>,
}
fn import_geosite(source: &Path, table: &mut redb::Table<&str, ()>) -> Result<()> {
    let source = std::fs::read_to_string(source)?;
    let data: Geosite = serde_saphyr::from_str(&source)?;
    if data.lists.is_empty() {
        return Err("geosite contains no lists".into());
    }
    for list in data.lists {
        if list.name.is_empty() || list.name.contains('\0') {
            return Err("invalid geosite list name".into());
        }
        for rule in list.rules {
            let (kind, value) = rule.split_once(':').ok_or("geosite rule has no type")?;
            let (value, attributes) = value.split_once(":@").unwrap_or((value, ""));
            if value.is_empty() || value.contains('\0') {
                return Err("empty or invalid geosite rule".into());
            }
            let value = match kind {
                "regexp" => {
                    regex::Regex::new(value)?;
                    value.to_owned()
                }
                "full" | "domain" | "keyword" => value.to_ascii_lowercase(),
                _ => return Err(format!("unsupported geosite rule type: {kind}").into()),
            };
            table.insert(key(&list.name, kind, &value).as_str(), ())?;
            for attribute in attributes.split(",@").filter(|s| !s.is_empty()) {
                table.insert(
                    key(&format!("{}@{attribute}", list.name), kind, &value).as_str(),
                    (),
                )?;
            }
        }
    }
    Ok(())
}
fn import_country(source: &Path, table: &mut redb::Table<&str, ()>) -> Result<()> {
    let reader = maxminddb::Reader::open_readfile(source)?;
    let mut count = 0;
    for entry in reader.networks(Default::default())? {
        let entry = entry?;
        let Some(record) = entry.decode::<maxminddb::geoip2::Country>()? else {
            continue;
        };
        let network = entry.network()?;
        for name in [record.country.iso_code, record.country.names.english]
            .into_iter()
            .flatten()
        {
            table.insert(ip_key(name, network.ip(), network.prefix()).as_str(), ())?;
            count += 1;
        }
    }
    if count == 0 {
        return Err("MMDB contains no country networks".into());
    }
    Ok(())
}

struct Slot {
    kind: RouterDBKind,
    value: RwLock<std::result::Result<Arc<RedbDatabase>, String>>,
}
#[derive(Default)]
pub(crate) struct Databases {
    slots: HashMap<String, Arc<Slot>>,
}
impl Databases {
    pub(crate) fn build(
        configs: &[RouterDatabaseCfg],
        inbounds: &mut HashMap<String, Box<dyn Inbound>>,
    ) -> std::result::Result<Arc<Self>, SError> {
        let mut manager = Self::default();
        for cfg in configs {
            let existing = cfg.path().try_exists()?;
            let value = if existing {
                Ok(Arc::new(RedbDatabase::open(cfg).map_err(config_error)?))
            } else {
                Err("download pending".into())
            };
            let slot = Arc::new(Slot {
                kind: cfg.kind(),
                value: RwLock::new(value),
            });
            if !existing {
                inbounds.insert(
                    cfg.tag().to_owned(),
                    Box::new(download::DownloadInbound::new(cfg.clone(), slot.clone())),
                );
            }
            manager.slots.insert(cfg.tag().to_owned(), slot);
        }
        Ok(Arc::new(manager))
    }
    pub(crate) fn install(self: &Arc<Self>, lua: &mlua::Lua) -> mlua::Result<()> {
        for name in ["find_ip_v4", "find_ip_v6", "find_domain"] {
            let manager = self.clone();
            lua.globals().set(
                name,
                lua.create_function(move |_, (tag, list, value): (String, String, String)| {
                    let slot = manager.slots.get(&tag).ok_or_else(|| {
                        mlua::Error::runtime(format!("unknown router database: {tag}"))
                    })?;
                    let expected = if name == "find_domain" {
                        RouterDBKind::Geosite
                    } else {
                        RouterDBKind::Country
                    };
                    if slot.kind != expected {
                        return Err(mlua::Error::runtime(format!(
                            "database {tag} does not support {name}"
                        )));
                    }
                    let db = slot
                        .value
                        .read()
                        .map_err(|_| mlua::Error::runtime("database lock poisoned"))?
                        .clone()
                        .map_err(|error| {
                            mlua::Error::runtime(format!("database {tag} unavailable: {error}"))
                        })?;
                    if list.contains('\0') || value.contains('\0') {
                        return Err(mlua::Error::runtime(
                            "NUL is not allowed in database lookups",
                        ));
                    }
                    let result = match name {
                        "find_ip_v4" => db.find_ip(
                            &list,
                            IpAddr::V4(value.parse().map_err(mlua::Error::external)?),
                        ),
                        "find_ip_v6" => db.find_ip(
                            &list,
                            IpAddr::V6(value.parse().map_err(mlua::Error::external)?),
                        ),
                        _ => db.find_domain(&list, &value),
                    };
                    result.map_err(|e| mlua::Error::runtime(e.to_string()))
                })?,
            )?;
        }
        Ok(())
    }
}
fn config_error(error: impl std::fmt::Display) -> SError {
    SError::InvalidConfig(format!("router database: {error}"))
}
pub(crate) fn validate_url(url: &str) -> std::result::Result<(), SError> {
    download::parse_url(url).map(|_| ()).map_err(config_error)
}
