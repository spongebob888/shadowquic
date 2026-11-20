use super::{
    COUNTRY_SCHEMA, CountryV4Table, CountryV6Table, GEOSITE_SCHEMA, GeositeTable, META, Result,
    country_table_name, geosite, insert_country_range,
};
use crate::config::{CustomDbCfg, CustomRuleList};
use ipnet::IpNet;
use redb::{
    Database, Key, ReadableTable, Table, TableDefinition, TableHandle, Value, WriteTransaction,
};
use std::{collections::HashSet, net::IpAddr, path::Path};

pub(super) fn open(cfg: &CustomDbCfg, cache_size: usize) -> Result<Database> {
    let mut builder = Database::builder();
    builder.set_cache_size(cache_size);
    let (db, temporary) = if cfg.path.try_exists()? {
        (builder.open(&cfg.path)?, None)
    } else {
        let parent = cfg
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let file = tempfile::NamedTempFile::new_in(parent)?;
        let db = builder.create(file.path())?;
        (db, Some(file))
    };

    let identity = [
        ("type", "custom"),
        ("country_schema", COUNTRY_SCHEMA),
        ("geosite_schema", GEOSITE_SCHEMA),
    ];
    let write = db.begin_write()?;
    {
        let mut meta = write.open_table(META)?;
        if temporary.is_some() {
            for (key, value) in identity {
                meta.insert(key, value)?;
            }
            meta.insert("version", env!("CARGO_PKG_VERSION"))?;
        } else {
            for (key, expected) in identity {
                if meta.get(key)?.as_ref().map(|value| value.value()) != Some(expected) {
                    return Err(format!(
                        "incompatible custom database {key}, expected {expected:?}"
                    )
                    .into());
                }
            }
            if meta.get("version")?.is_none() {
                return Err("missing custom database creator version".into());
            }
        }
    }
    let existing: HashSet<_> = write.list_tables()?.map(|t| t.name().to_owned()).collect();
    let mut names = HashSet::new();
    let mut warned_attributes = HashSet::new();
    for list in &cfg.list {
        if list.name.trim().is_empty() || list.name.contains(['\0', '@']) {
            return Err(format!("invalid custom list name {:?}", list.name).into());
        }
        if !names.insert(list.name.to_ascii_lowercase()) {
            return Err(format!("duplicate custom list {:?}", list.name).into());
        }
        write_list(&write, &existing, list, &mut warned_attributes)?;
    }
    write.commit()?;
    if let Some(file) = temporary {
        file.persist_noclobber(&cfg.path)?;
    }
    Ok(db)
}

fn prepare_table<'txn, K: Key + 'static, V: Value + 'static>(
    write: &'txn WriteTransaction,
    existing: &HashSet<String>,
    definition: TableDefinition<K, V>,
    force: bool,
) -> Result<Option<Table<'txn, K, V>>> {
    if existing.contains(definition.name()) {
        if !force {
            // Check the persisted table's key and value types before serving lookups.
            write.open_table(definition)?;
            return Ok(None);
        }
        write.delete_table(definition)?;
    }
    Ok(Some(write.open_table(definition)?))
}

fn write_list(
    write: &WriteTransaction,
    existing: &HashSet<String>,
    list: &CustomRuleList,
    warned_attributes: &mut HashSet<String>,
) -> Result<()> {
    let mut domains = prepare_table(
        write,
        existing,
        GeositeTable::new(&geosite::geosite_table_name(&list.name)),
        list.force,
    )?;
    let mut v4 = prepare_table(
        write,
        existing,
        CountryV4Table::new(&country_table_name(&list.name, 4)),
        list.force,
    )?;
    let mut v6 = prepare_table(
        write,
        existing,
        CountryV6Table::new(&country_table_name(&list.name, 6)),
        list.force,
    )?;
    if domains.is_none() && v4.is_none() && v6.is_none() {
        return Ok(());
    }
    for rule in &list.rules {
        let (kind, value) = rule
            .split_once(':')
            .ok_or_else(|| format!("missing rule type in list {:?}: {rule:?}", list.name))?;
        let value = value.trim();
        if kind.trim() == "ip" {
            if v4.is_none() && v6.is_none() {
                continue;
            }
            let network: IpNet = if value.contains('/') {
                value.parse::<IpNet>().ok()
            } else {
                value.parse::<IpAddr>().ok().map(IpNet::from)
            }
            .ok_or_else(|| {
                format!(
                    "invalid IP address or CIDR in list {:?}: {value:?}",
                    list.name
                )
            })?;
            match network {
                IpNet::V4(net) => {
                    if let Some(table) = &mut v4 {
                        insert_country_range(
                            table,
                            u32::from(net.network()),
                            u32::from(net.broadcast()),
                        )?;
                    }
                }
                IpNet::V6(net) => {
                    if let Some(table) = &mut v6 {
                        insert_country_range(
                            table,
                            u128::from(net.network()),
                            u128::from(net.broadcast()),
                        )?;
                    }
                }
            }
        } else if let Some(table) = &mut domains {
            geosite::insert_rule(table, kind.trim(), value, &list.name, warned_attributes)
                .map_err(|e| format!("invalid rule {rule:?} in list {:?}: {e}", list.name))?;
        }
    }
    Ok(())
}
