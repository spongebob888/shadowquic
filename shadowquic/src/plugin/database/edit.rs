use super::{
    CountryV4Table, CountryV6Table, GeositeTable, RedbDatabase, Result, RuleError,
    country_table_name, custom, geosite,
};
use ipnet::{IpNet, Ipv4Subnets, Ipv6Subnets};
use redb::{Key, ReadableDatabase, ReadableTable, Table, TableHandle};
use std::{
    collections::{BTreeSet, HashSet},
    ops::{Add, Sub},
};

pub(crate) enum ListEdit {
    Replace(Vec<String>),
    Add(Vec<String>),
    Remove(Vec<String>),
    Delete,
}

impl RedbDatabase {
    pub(crate) fn list_names(&self) -> Result<BTreeSet<String>> {
        let read = self.db.begin_read()?;
        let mut names = BTreeSet::new();
        for table in read.list_tables()? {
            for prefix in ["geosite_", "country_v4_", "country_v6_"] {
                if let Some(name) = table.name().strip_prefix(prefix) {
                    names.insert(name.to_string());
                }
            }
        }
        Ok(names)
    }

    pub(crate) fn list_rules(&self, list: &str) -> Result<Vec<String>> {
        custom::validate_name(list)?;
        let read = self.db.begin_read()?;
        let name = geosite::geosite_table_name(list);
        let sites = match read.open_table(GeositeTable::new(&name)) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(RuleError::MissingList.into());
            }
            Err(e) => return Err(e.into()),
        };
        let mut rules = Vec::new();
        for entry in sites.iter()? {
            let (key, mask) = entry?;
            rules.push(geosite::format_rule(key.value(), mask.value()));
        }
        for entry in read
            .open_table(CountryV4Table::new(&country_table_name(list, 4)))?
            .iter()?
        {
            let (start, end) = entry?;
            rules.extend(
                Ipv4Subnets::new(start.value().into(), end.value().into(), 0)
                    .map(|net| ip_rule(net.into())),
            );
        }
        for entry in read
            .open_table(CountryV6Table::new(&country_table_name(list, 6)))?
            .iter()?
        {
            let (start, end) = entry?;
            rules.extend(
                Ipv6Subnets::new(start.value().into(), end.value().into(), 0)
                    .map(|net| ip_rule(net.into())),
            );
        }
        Ok(rules)
    }

    pub(crate) fn edit_list(&self, list: &str, edit: ListEdit) -> Result<()> {
        custom::validate_name(list)?;
        let site_name = geosite::geosite_table_name(list);
        let v4_name = country_table_name(list, 4);
        let v6_name = country_table_name(list, 6);
        let sites = GeositeTable::new(&site_name);
        let v4 = CountryV4Table::new(&v4_name);
        let v6 = CountryV6Table::new(&v6_name);
        let write = self.db.begin_write()?;
        if !matches!(edit, ListEdit::Replace(_))
            && !write.list_tables()?.any(|t| t.name() == site_name)
        {
            return Err(RuleError::MissingList.into());
        }
        if matches!(edit, ListEdit::Replace(_) | ListEdit::Delete) {
            write.delete_table(sites)?;
            write.delete_table(v4)?;
            write.delete_table(v6)?;
        }
        let mut warned = HashSet::new();
        match edit {
            ListEdit::Delete => {}
            ListEdit::Replace(rules) | ListEdit::Add(rules) => {
                custom::insert_rules(
                    list,
                    &rules,
                    Some(write.open_table(sites)?),
                    Some(write.open_table(v4)?),
                    Some(write.open_table(v6)?),
                    &mut warned,
                )?;
            }
            ListEdit::Remove(rules) => {
                let mut sites = write.open_table(sites)?;
                let mut v4 = write.open_table(v4)?;
                let mut v6 = write.open_table(v6)?;
                for rule in rules {
                    let (kind, value) = custom::split_rule(&rule)?;
                    if kind == "ip" {
                        match custom::network(value)? {
                            IpNet::V4(net) => subtract_range(
                                &mut v4,
                                net.network().into(),
                                net.broadcast().into(),
                            )?,
                            IpNet::V6(net) => subtract_range(
                                &mut v6,
                                net.network().into(),
                                net.broadcast().into(),
                            )?,
                        }
                    } else {
                        geosite::remove_rule(&mut sites, kind, value, list, &mut warned)?;
                    }
                }
            }
        }
        write.commit()?;
        Ok(())
    }
}

fn ip_rule(net: IpNet) -> String {
    if net.prefix_len() == net.max_prefix_len() {
        format!("ip:{}", net.addr())
    } else {
        format!("ip:{net}")
    }
}

/// Stored ranges are inclusive and disjoint. Preserve fragments on either side.
fn subtract_range<T>(table: &mut Table<T, T>, start: T, end: T) -> Result<()>
where
    T: for<'a> Key<SelfType<'a> = T>
        + Copy
        + Ord
        + From<u8>
        + Add<Output = T>
        + Sub<Output = T>
        + 'static,
{
    loop {
        let entry = table
            .range(..=end)?
            .next_back()
            .transpose()?
            .map(|(k, v)| (k.value(), v.value()));
        let Some((left, right)) = entry else { break };
        if right < start {
            break;
        }
        table.remove(left)?;
        if right > end {
            table.insert(end + T::from(1), right)?;
        }
        if left < start {
            table.insert(left, start - T::from(1))?;
            break;
        }
    }
    Ok(())
}
