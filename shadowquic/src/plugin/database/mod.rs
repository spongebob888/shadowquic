//! Routing membership databases: memory-mapped MMDB country lookups or
//! indexed redb lookups with a bounded page cache.
mod country;
mod custom;
#[cfg(feature = "restful-api")]
mod edit;
#[cfg(feature = "restful-api")]
pub(crate) use edit::ListEdit;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RuleError {
    #[error("{0}")]
    Invalid(String),
    #[cfg(feature = "restful-api")]
    #[error("unknown rule list")]
    MissingList,
}
mod download;
mod geosite;
#[cfg(test)]
mod tests;
mod v2ray_dat;

use crate::{
    Inbound,
    config::{RouterDBKind, RouterDatabaseCfg},
    error::SError,
};
pub use country::MmdbCountryRecord;
use geosite::{GeositeTable, import_geosite};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableHandle};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    net::IpAddr,
    path::Path,
    sync::{Arc, RwLock},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const META: TableDefinition<&str, &str> = TableDefinition::new("metadata");
const COUNTRY_SCHEMA: &str = "2";
const GEOSITE_SCHEMA: &str = "4";

#[derive(Debug, thiserror::Error)]
#[error("geosite schema {0} requires rebuilding to schema {GEOSITE_SCHEMA}")]
struct GeositeRebuildRequired(u32);

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
            .decode::<MmdbCountryRecord>()?
            .is_some_and(|record| record.contains(list)))
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
    let db = match unsafe { MmdbDatabase::open(temporary.path()) } {
        Ok(db) => db,
        Err(error) => {
            let bytes = std::fs::read(source)?;
            if v2ray_dat::decode_geoip(&bytes).is_ok_and(|entries| !entries.is_empty()) {
                return Err(format!(
                    "{error}; the source is a geoip.dat, which is converted to redb: use a path without the .mmdb suffix"
                )
                .into());
            }
            return Err(error);
        }
    };
    temporary.persist_noclobber(cfg.path())?;
    Ok(Arc::new(db))
}

pub struct RedbDatabase {
    db: Database,
    kind: RouterDBKind,
}

fn source_metadata(cfg: &RouterDatabaseCfg) -> Result<[(&'static str, &str); 3]> {
    let (kind, schema) = match cfg {
        RouterDatabaseCfg::Country(_) => ("country", COUNTRY_SCHEMA),
        RouterDatabaseCfg::Geosite(_) => ("geosite", GEOSITE_SCHEMA),
        RouterDatabaseCfg::Custom(_) => return Err("custom databases use inline rules".into()),
    };
    Ok([("schema", schema), ("type", kind), ("url", cfg.url())])
}

impl RedbDatabase {
    fn legacy_geosite_schema(cfg: &RouterDatabaseCfg, db: &Database) -> Result<Option<u32>> {
        if cfg.kind() != RouterDBKind::Geosite {
            return Ok(None);
        }
        let read = db.begin_read()?;
        let meta = read.open_table(META)?;
        if meta.get("type")?.as_ref().map(|entry| entry.value()) != Some("geosite") {
            return Ok(None);
        }
        let current_schema = GEOSITE_SCHEMA.parse::<u32>()?;
        Ok(meta
            .get("schema")?
            .and_then(|entry| entry.value().parse::<u32>().ok())
            .filter(|version| *version < current_schema))
    }

    /// Open a router database, applying configured custom lists on startup.
    pub fn open(cfg: &RouterDatabaseCfg) -> Result<Self> {
        if let RouterDatabaseCfg::Custom(custom) = cfg {
            let db = custom::open(custom, cfg.cache_size()).map_err(|e| {
                format!("custom database {:?} at {:?}: {e}", custom.tag, custom.path)
            })?;
            return Ok(Self {
                db,
                kind: RouterDBKind::Custom,
            });
        }
        let db = Database::builder()
            .set_cache_size(cfg.cache_size())
            .open(cfg.path())?;
        if let Some(version) = Self::legacy_geosite_schema(cfg, &db)? {
            return Err(GeositeRebuildRequired(version).into());
        }
        {
            let read = db.begin_read()?;
            let meta = read.open_table(META)?;
            for (name, expected) in source_metadata(cfg)? {
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
            if let RouterDatabaseCfg::Geosite(site) = cfg {
                let expected = geosite::selection_key(&site.list_include);
                let stored = meta.get("list")?;
                let stored = stored.as_ref().map(|value| value.value()).unwrap_or("");
                if stored != expected {
                    return Err(format!(
                        "database {:?} at {:?} has incompatible list selection. Remove the database file {:?} and restart Shadowquic to download and rebuild it",
                        cfg.tag(), cfg.path(), cfg.path()
                    ).into());
                }
            }
            if meta.get("sha256")?.is_none() {
                return Err(format!(
                    "database {:?} at {:?} has incomplete metadata (missing sha256). Remove the database file {:?} and restart Shadowquic to download and rebuild it from {:?}",
                    cfg.tag(), cfg.path(), cfg.path(), cfg.url()
                ).into());
            }
            match cfg {
                RouterDatabaseCfg::Geosite(_) => {
                    let mut count = 0;
                    for table in read.list_tables()? {
                        if table.name().starts_with("geosite_") {
                            read.open_table(GeositeTable::new(table.name()))?;
                            count += 1;
                        }
                    }
                    if count == 0 {
                        return Err("geosite database contains no list tables".into());
                    }
                }
                RouterDatabaseCfg::Country(_) => {
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
                RouterDatabaseCfg::Custom(_) => unreachable!(),
            }
        }
        Ok(Self {
            db,
            kind: cfg.kind(),
        })
    }

    /// Import a source atomically, rebuilding tables in a legacy geosite database.
    pub fn import(cfg: &RouterDatabaseCfg, source: &Path) -> Result<Self> {
        let identity = source_metadata(cfg)?;
        let parent = cfg
            .path()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let legacy = if cfg.kind() == RouterDBKind::Geosite && cfg.path().try_exists()? {
            let db = Database::builder()
                .set_cache_size(cfg.cache_size())
                .open(cfg.path())?;
            if Self::legacy_geosite_schema(cfg, &db)?.is_none() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "refusing to replace an existing non-legacy database",
                )
                .into());
            }
            Some(db)
        } else {
            None
        };
        let temporary = if legacy.is_none() {
            Some(tempfile::NamedTempFile::new_in(parent)?)
        } else {
            None
        };
        let rebuilding = legacy.is_some();
        let mut db = match legacy {
            Some(db) => db,
            None => Database::builder()
                .set_cache_size(cfg.cache_size())
                .create(temporary.as_ref().unwrap().path())?,
        };
        let write = db.begin_write()?;
        if rebuilding {
            for table in write.list_tables()?.collect::<Vec<_>>() {
                write.delete_table(table)?;
            }
            for table in write.list_multimap_tables()?.collect::<Vec<_>>() {
                write.delete_multimap_table(table)?;
            }
        }
        {
            match cfg {
                RouterDatabaseCfg::Geosite(site) => {
                    import_geosite(source, &write, &site.list_include)?
                }
                RouterDatabaseCfg::Country(_) => import_country(source, &write)?,
                RouterDatabaseCfg::Custom(_) => unreachable!(),
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
            if let RouterDatabaseCfg::Geosite(site) = cfg {
                meta.insert("list", geosite::selection_key(&site.list_include).as_str())?;
            }
            for (name, value) in identity.into_iter().chain([
                ("version", env!("CARGO_PKG_VERSION")),
                ("sha256", digest.as_str()),
            ]) {
                meta.insert(name, value)?;
            }
        }
        write.commit()?;
        db.compact(redb::CompactionMode::Maximum)?;
        drop(db);
        if let Some(temporary) = temporary {
            temporary.persist_noclobber(cfg.path())?;
        }
        Self::open(cfg)
    }
}

impl RouterDB for RedbDatabase {
    fn find_ip(&self, list: &str, ip: IpAddr) -> Result<bool> {
        assert_ne!(
            self.kind,
            RouterDBKind::Geosite,
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
        assert_ne!(
            self.kind,
            RouterDBKind::Country,
            "country does not support domain lookup"
        );
        geosite::find_domain(&self.db.begin_read()?, list, domain)
    }
}

/// Insert an inclusive range, merging adjacent and overlapping ranges.
fn insert_country_range<T>(table: &mut redb::Table<T, T>, mut start: T, mut end: T) -> Result<()>
where
    T: for<'a> redb::Key<SelfType<'a> = T> + Copy + Ord + Into<u128> + 'static,
{
    let previous = table
        .range(..=start)?
        .next_back()
        .transpose()?
        .map(|(start, end)| (start.value(), end.value()));
    if let Some((previous_start, previous_end)) = previous
        && previous_end.into().saturating_add(1) >= start.into()
    {
        start = previous_start;
        end = end.max(previous_end);
    }
    loop {
        let next = table
            .range((std::ops::Bound::Excluded(start), std::ops::Bound::Unbounded))?
            .next()
            .transpose()?
            .map(|(start, end)| (start.value(), end.value()));
        let Some((next_start, next_end)) = next else {
            break;
        };
        if next_start.into() > end.into().saturating_add(1) {
            break;
        }
        table.remove(next_start)?;
        end = end.max(next_end);
    }
    table.insert(start, end)?;
    Ok(())
}

/// Import a country source: an MMDB, or a v2ray/xray `geoip.dat`.
fn import_country(source: &Path, write: &redb::WriteTransaction) -> Result<()> {
    let bytes = std::fs::read(source)?;
    let mmdb = match maxminddb::Reader::from_source(bytes.as_slice()) {
        Ok(reader) => return import_country_mmdb(&reader, write),
        Err(error) => error,
    };
    let entries = v2ray_dat::decode_geoip(&bytes).map_err(|dat| {
        format!("country source is neither an MMDB ({mmdb}) nor a geoip.dat ({dat})")
    })?;
    import_country_dat(entries, write)
}

fn import_country_dat(
    entries: Vec<v2ray_dat::GeoIp>,
    write: &redb::WriteTransaction,
) -> Result<()> {
    let mut count = 0;
    for mut entry in entries {
        if entry.code.is_empty() {
            continue;
        }
        // Sorted inserts merge into the previous range instead of scanning.
        entry.ranges.sort_unstable();
        let mut v4 = write.open_table(CountryV4Table::new(&country_table_name(&entry.code, 4)))?;
        let mut v6 = write.open_table(CountryV6Table::new(&country_table_name(&entry.code, 6)))?;
        for (start, end) in entry.ranges {
            match (start, end) {
                (IpAddr::V4(start), IpAddr::V4(end)) => insert_country_range(
                    &mut v4,
                    u32::from_be_bytes(start.octets()),
                    u32::from_be_bytes(end.octets()),
                )?,
                (IpAddr::V6(start), IpAddr::V6(end)) => insert_country_range(
                    &mut v6,
                    u128::from_be_bytes(start.octets()),
                    u128::from_be_bytes(end.octets()),
                )?,
                _ => unreachable!("CIDR endpoints have the same address family"),
            }
            count += 1;
        }
    }
    if count == 0 {
        return Err("geoip.dat contains no networks".into());
    }
    Ok(())
}

fn import_country_mmdb(
    reader: &maxminddb::Reader<&[u8]>,
    write: &redb::WriteTransaction,
) -> Result<()> {
    let mut count = 0;
    for entry in reader.networks(Default::default())? {
        let entry = entry?;
        let Some(record) = entry.decode::<MmdbCountryRecord>()? else {
            continue;
        };
        let network = entry.network()?;
        let mut seen = HashSet::new();
        for code in record.codes() {
            // Case variants share a table. Insert each range only once per list.
            if !seen.insert(code.to_ascii_lowercase()) {
                continue;
            }
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
    #[cfg(feature = "restful-api")]
    pub(crate) custom: HashMap<String, Arc<RedbDatabase>>,
    slots: HashMap<String, Arc<Slot>>,
}
impl Databases {
    pub(crate) fn build(
        configs: &[RouterDatabaseCfg],
        inbounds: &mut HashMap<String, Box<dyn Inbound>>,
    ) -> std::result::Result<Arc<Self>, SError> {
        let mut manager = Self::default();
        for cfg in configs {
            let value = if cfg.kind() == RouterDBKind::Custom {
                let db = Arc::new(RedbDatabase::open(cfg).map_err(config_error)?);
                #[cfg(feature = "restful-api")]
                manager.custom.insert(cfg.tag().to_owned(), db.clone());
                Ok(db as Arc<dyn RouterDB>)
            } else if cfg.path().try_exists()? {
                match open_database(cfg) {
                    Ok(db) => Ok(db),
                    Err(error) if error.is::<GeositeRebuildRequired>() => {
                        tracing::warn!(event = "database:rebuild_started", tag = %cfg.tag(), path = ?cfg.path(), %error, "rebuilding legacy geosite database");
                        Err("schema upgrade download pending".into())
                    }
                    Err(error) => return Err(config_error(error)),
                }
            } else {
                Err("download pending".into())
            };
            let download = value.is_err();
            let slot = Arc::new(Slot {
                kind: cfg.kind(),
                value: RwLock::new(value),
            });
            if download {
                inbounds.insert(
                    cfg.tag().to_owned(),
                    Box::new(download::inbound(cfg.clone(), slot.clone())),
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
                    if slot.kind != RouterDBKind::Custom && slot.kind != expected {
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
    crate::utils::download::parse_url(url)
        .map(|_| ())
        .map_err(config_error)
}
