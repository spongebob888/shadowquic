use super::Result;
use redb::TableDefinition;
use regex_automata::{
    Input,
    dfa::{Automaton, StartKind, dense, sparse},
};
use serde::Deserialize;
use std::path::Path;

// Schema 2 uses byte-slice keys and values; compiled regexes are DFA keys.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum SiteMatchType {
    Full = 0,
    Domain = 1,
    Regex = 2,
    Keyword = 3,
    CompiledRegex = 4,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum AttrType {
    Nil = 0,
    Ads = 1,
    NotCn = 2,
    Cn = 3,
}

impl AttrType {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "ads" => Some(Self::Ads),
            "!cn" => Some(Self::NotCn),
            "cn" => Some(Self::Cn),
            _ => None,
        }
    }
}

pub(super) type GeositeTable<'a> = TableDefinition<'a, (u8, &'static [u8]), u8>;

pub(super) fn geosite_table_name(list: &str) -> String {
    format!("geosite_{}", list.to_ascii_lowercase())
}

pub(super) fn deserialize_dfa(bytes: &[u8]) -> Result<sparse::DFA<&[u8]>> {
    let (dfa, _) = sparse::DFA::from_bytes(bytes)?;
    Ok(dfa)
}

pub(super) fn find_domain(read: &redb::ReadTransaction, list: &str, domain: &str) -> Result<bool> {
    let list = list.to_ascii_lowercase();
    let (list, attribute) = match list.split_once('@') {
        Some((list, attribute)) => match AttrType::parse(attribute) {
            Some(attribute) => (list, attribute),
            None => return Ok(false),
        },
        None => (list.as_str(), AttrType::Nil),
    };
    let name = geosite_table_name(list);
    let table = match read.open_table(GeositeTable::new(&name)) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let indexed = |kind: SiteMatchType, value: &str| -> Result<bool> {
        Ok(table
            .get((kind as u8, value.as_bytes()))?
            .is_some_and(|stored| attribute == AttrType::Nil || stored.value() == attribute as u8))
    };
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    if indexed(SiteMatchType::Full, &domain)? {
        return Ok(true);
    }
    let mut suffix = domain.as_str();
    loop {
        if indexed(SiteMatchType::Domain, suffix)? {
            return Ok(true);
        }
        match suffix.split_once('.') {
            Some((_, rest)) => suffix = rest,
            None => break,
        }
    }
    for entry in table.range(
        (SiteMatchType::Keyword as u8, &b""[..])..((SiteMatchType::Keyword as u8 + 1), &b""[..]),
    )? {
        let (key, stored_attribute) = entry?;
        if attribute != AttrType::Nil && stored_attribute.value() != attribute as u8 {
            continue;
        }
        let (_, pattern) = key.value();
        if domain
            .as_bytes()
            .windows(pattern.len())
            .any(|window| window == pattern)
        {
            return Ok(true);
        }
    }
    for entry in table.range(
        (SiteMatchType::CompiledRegex as u8, &b""[..])
            ..((SiteMatchType::CompiledRegex as u8 + 1), &b""[..]),
    )? {
        let (key, stored_attribute) = entry?;
        if attribute != AttrType::Nil && stored_attribute.value() != attribute as u8 {
            continue;
        }
        let (_, dfa_bytes) = key.value();
        let dfa = deserialize_dfa(dfa_bytes)?;
        if dfa.try_search_fwd(&Input::new(&domain))?.is_some() {
            return Ok(true);
        }
    }
    for entry in table.range(
        (SiteMatchType::Regex as u8, &b""[..])..((SiteMatchType::Regex as u8 + 1), &b""[..]),
    )? {
        let (key, stored_attribute) = entry?;
        if attribute != AttrType::Nil && stored_attribute.value() != attribute as u8 {
            continue;
        }
        let (_, pattern) = key.value();
        let pattern = std::str::from_utf8(pattern)?;
        if regex::Regex::new(pattern)?.is_match(&domain) {
            return Ok(true);
        }
    }
    Ok(false)
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

pub(super) fn import_geosite(source: &Path, write: &redb::WriteTransaction) -> Result<()> {
    let source = std::fs::read_to_string(source)?;
    let data: Geosite = serde_saphyr::from_str(&source)?;
    if data.lists.is_empty() {
        return Err("geosite contains no lists".into());
    }
    for list in data.lists {
        if list.name.is_empty() || list.name.contains(['\0', '@']) {
            return Err("invalid geosite list name".into());
        }
        let name = geosite_table_name(&list.name);
        // Preserve empty lists too, and merge repeated names case insensitively.
        let mut table = write.open_table(GeositeTable::new(&name))?;
        for rule in list.rules {
            let (kind, value) = rule.split_once(':').ok_or("geosite rule has no type")?;
            let (value, attribute) = value.split_once(":@").unwrap_or((value, ""));
            if value.is_empty() || value.contains('\0') {
                return Err("empty or invalid geosite rule".into());
            }
            let kind = match kind {
                "full" => SiteMatchType::Full,
                "domain" => SiteMatchType::Domain,
                "regexp" => SiteMatchType::Regex,
                "keyword" => SiteMatchType::Keyword,
                _ => return Err(format!("unsupported geosite rule type: {kind}").into()),
            };
            assert!(!attribute.contains("@"));
            let attribute =
                AttrType::parse(&attribute.to_ascii_lowercase()).unwrap_or(AttrType::Nil);
            let (kind, key_value) = if kind == SiteMatchType::Regex {
                let compiled = dense::Builder::new()
                    .configure(dense::Config::new().start_kind(StartKind::Unanchored))
                    .build(value)
                    .ok()
                    .and_then(|dfa| dfa.to_sparse().ok());
                match compiled {
                    Some(dfa) => (SiteMatchType::CompiledRegex, dfa.to_bytes_little_endian()),
                    None => {
                        regex::Regex::new(value)?;
                        (SiteMatchType::Regex, value.as_bytes().to_vec())
                    }
                }
            } else {
                (kind, value.to_ascii_lowercase().into_bytes())
            };
            table.insert((kind as u8, key_value.as_slice()), attribute as u8)?;
        }
    }
    Ok(())
}
