//! Routing membership databases: memory-mapped MMDB country lookups or
//! indexed redb lookups with a bounded page cache.
mod download;
#[cfg(test)]
mod tests;

use crate::{
    Inbound,
    config::{RouterDBKind, RouterDatabaseCfg},
    error::SError,
};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableHandle};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Read,
    net::IpAddr,
    path::Path,
    sync::{Arc, RwLock},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const META: TableDefinition<&str, &str> = TableDefinition::new("metadata");
const RULES: TableDefinition<&str, ()> = TableDefinition::new("rules");
const COUNTRY_SCHEMA: &str = "2";
const GEOSITE_SCHEMA: &str = "1";

fn schema(kind: RouterDBKind) -> &'static str {
    match kind {
        RouterDBKind::Country => COUNTRY_SCHEMA,
        RouterDBKind::Geosite => GEOSITE_SCHEMA,
    }
}

type CountryV4Table<'a> = TableDefinition<'a, u32, u32>;
type CountryV6Table<'a> = TableDefinition<'a, u128, u128>;

fn country_table_name(list: &str, family: u8) -> String {
    format!("country_v{family}_{}", list.to_ascii_lowercase())
}

fn find_range<T>(read: &redb::ReadTransaction, name: &str, query: T) -> Result<bool>
where
    T: for<'a> redb::Key<SelfType<'a> = T> + Copy + Ord + 'static,
{
    let table = match read.open_table(TableDefinition::<T, T>::new(name)) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let Some(entry) = table.range(..=query)?.next_back() else {
        return Ok(false);
    };
    let (_, end) = entry?;
    Ok(query <= end.value())
}

/// Unsupported lookup families panic. Lua checks the family before calling.
pub trait RouterDB: Send + Sync {
    fn find_ip(&self, list: &str, ip: IpAddr) -> Result<bool>;
    fn find_domain(&self, list: &str, domain: &str) -> Result<bool>;
}

/// Country membership using a read-only mapping of the original MMDB file.
pub struct MmdbDatabase {
    reader: maxminddb::Reader<maxminddb::Mmap>,
}

impl MmdbDatabase {
    /// Open a country MMDB without copying its contents into a heap buffer.
    ///
    /// # Safety
    /// The mapped file must not be modified or truncated until this database
    /// is dropped. Stop Shadowquic before updating a configured MMDB file.
    pub unsafe fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            // SAFETY: the caller guarantees the mapped file remains unchanged.
            reader: unsafe { maxminddb::Reader::open_mmap(path)? },
        })
    }
}

impl RouterDB for MmdbDatabase {
    fn find_ip(&self, list: &str, ip: IpAddr) -> Result<bool> {
        // An IPv4-only database has no IPv6 members, like an empty redb v6 table.
        if ip.is_ipv6() && self.reader.metadata().ip_version == 4 {
            return Ok(false);
        }
        Ok(self
            .reader
            .lookup(ip)?
            .decode::<maxminddb::geoip2::Country>()?
            .and_then(|record| record.country.iso_code)
            .is_some_and(|code| code.eq_ignore_ascii_case(list)))
    }

    fn find_domain(&self, _list: &str, _domain: &str) -> Result<bool> {
        panic!("country does not support domain lookup")
    }
}

fn open_database(cfg: &RouterDatabaseCfg) -> Result<Arc<dyn RouterDB>> {
    if cfg.uses_mmdb() {
        // SAFETY: configured MMDBs are read-only for the router's lifetime.
        // Downloads never overwrite existing files; external updates require
        // stopping Shadowquic, as documented for memory-mapped databases.
        Ok(Arc::new(unsafe { MmdbDatabase::open(cfg.path())? }))
    } else {
        Ok(Arc::new(RedbDatabase::open(cfg)?))
    }
}

fn import_database(cfg: &RouterDatabaseCfg, source: &Path) -> Result<Arc<dyn RouterDB>> {
    if !cfg.uses_mmdb() {
        return Ok(Arc::new(RedbDatabase::import(cfg, source)?));
    }
    let parent = cfg
        .path()
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut std::fs::File::open(source)?, &mut temporary)?;
    // SAFETY: writing is complete. Publishing renames/links this same file;
    // it does not change its bytes. The router never modifies published MMDBs.
    let db = unsafe { MmdbDatabase::open(temporary.path())? };
    temporary.persist_noclobber(cfg.path())?;
    Ok(Arc::new(db))
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
                ("schema", schema(cfg.kind())),
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
            match cfg.kind() {
                RouterDBKind::Geosite => {
                    read.open_table(RULES)?;
                }
                RouterDBKind::Country => {
                    let mut count = 0;
                    for table in read.list_tables()? {
                        if table.name().starts_with("country_v4_") {
                            read.open_table(CountryV4Table::new(table.name()))?;
                            count += 1;
                        } else if table.name().starts_with("country_v6_") {
                            read.open_table(CountryV6Table::new(table.name()))?;
                            count += 1;
                        }
                    }
                    if count == 0 {
                        return Err("country database contains no country tables".into());
                    }
                }
            }
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
            match cfg.kind() {
                RouterDBKind::Geosite => import_geosite(source, &mut write.open_table(RULES)?)?,
                RouterDBKind::Country => import_country(source, &write)?,
            }
            let mut hash = Sha256::new();
            let mut source = std::fs::File::open(source)?;
            let mut buffer = [0; 8192];
            loop {
                match source.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(len) => hash.update(&buffer[..len]),
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => return Err(err.into()),
                }
            }
            let digest: String = hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let mut meta = write.open_table(META)?;
            for (name, value) in [
                ("schema", schema(cfg.kind())),
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
        match ip {
            // Interpret network-order octets independently of host endianness.
            IpAddr::V4(ip) => find_range(
                &read,
                &country_table_name(list, 4),
                u32::from_be_bytes(ip.octets()),
            ),
            IpAddr::V6(ip) => find_range(
                &read,
                &country_table_name(list, 6),
                u128::from_be_bytes(ip.octets()),
            ),
        }
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
/// Insert a disjoint MMDB range, joining adjacent ranges in either direction.
fn insert_country_range<T>(table: &mut redb::Table<T, T>, mut start: T, mut end: T) -> Result<()>
where
    T: for<'a> redb::Key<SelfType<'a> = T> + Copy + Ord + Into<u128> + 'static,
{
    let previous = table
        .range(..start)?
        .next_back()
        .transpose()?
        .map(|(start, end)| (start.value(), end.value()));
    if let Some((previous_start, previous_end)) = previous
        && previous_end.into().checked_add(1) == Some(start.into())
    {
        start = previous_start;
    }
    let next = table
        .range(end..)?
        .next()
        .transpose()?
        .map(|(start, end)| (start.value(), end.value()));
    if let Some((next_start, next_end)) = next
        && end.into().checked_add(1) == Some(next_start.into())
    {
        table.remove(next_start)?;
        end = next_end;
    }
    table.insert(start, end)?;
    Ok(())
}

fn import_country(source: &Path, write: &redb::WriteTransaction) -> Result<()> {
    let reader = maxminddb::Reader::open_readfile(source)?;
    let mut count = 0;
    for entry in reader.networks(Default::default())? {
        let entry = entry?;
        let Some(record) = entry.decode::<maxminddb::geoip2::Country>()? else {
            continue;
        };
        let Some(code) = record.country.iso_code else {
            continue;
        };
        let network = entry.network()?;
        let v4_name = country_table_name(code, 4);
        let v6_name = country_table_name(code, 6);
        let mut v4 = write.open_table(CountryV4Table::new(&v4_name))?;
        let mut v6 = write.open_table(CountryV6Table::new(&v6_name))?;
        match (network.network(), network.broadcast()) {
            (IpAddr::V4(start), IpAddr::V4(end)) => {
                insert_country_range(
                    &mut v4,
                    u32::from_be_bytes(start.octets()),
                    u32::from_be_bytes(end.octets()),
                )?;
            }
            (IpAddr::V6(start), IpAddr::V6(end)) => {
                insert_country_range(
                    &mut v6,
                    u128::from_be_bytes(start.octets()),
                    u128::from_be_bytes(end.octets()),
                )?;
            }
            _ => unreachable!("network endpoints have the same address family"),
        }
        count += 1;
    }
    if count == 0 {
        return Err("MMDB contains no country networks".into());
    }
    Ok(())
}

struct Slot {
    kind: RouterDBKind,
    value: RwLock<std::result::Result<Arc<dyn RouterDB>, String>>,
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
                Ok(open_database(cfg).map_err(config_error)?)
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
