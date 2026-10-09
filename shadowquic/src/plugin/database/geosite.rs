use super::Result;
use redb::TableDefinition;
use regex_automata::{
    Input,
    dfa::{Automaton, StartKind, dense},
};
use serde::Deserialize;
use std::{collections::HashMap, path::Path};

// Schema 2 uses byte-slice keys/values and stores serialized regex DFAs.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum SiteMatchType {
    Full = 0,
    Domain = 1,
    Regex = 2,
    Keyword = 3,
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

pub(super) type GeositeTable<'a> = TableDefinition<'a, (u8, &'static [u8]), &'static [u8]>;

const REGEX_SOURCE: u8 = 0;
const REGEX_DFA: u8 = 1;

pub(super) struct RegexEntry {
    attribute: u8,
    regex: CachedRegex,
}

enum CachedRegex {
    Dfa(Box<dense::DFA<Vec<u32>>>),
    Source(Box<regex::Regex>),
}

impl CachedRegex {
    fn is_match(&self, domain: &str) -> Result<bool> {
        match self {
            Self::Dfa(dfa) => Ok(dfa.try_search_fwd(&Input::new(domain))?.is_some()),
            Self::Source(regex) => Ok(regex.is_match(domain)),
        }
    }
}

pub(super) type RegexCache = HashMap<String, Vec<RegexEntry>>;

pub(super) fn geosite_table_name(list: &str) -> String {
    format!("geosite_{}", list.to_ascii_lowercase())
}

pub(super) fn compile_regexes(
    read: &redb::ReadTransaction,
    table_name: &str,
) -> Result<Vec<RegexEntry>> {
    let table = read.open_table(GeositeTable::new(table_name))?;
    table
        .range(
            (SiteMatchType::Regex as u8, &b""[..])..((SiteMatchType::Regex as u8 + 1), &b""[..]),
        )?
        .map(|entry| {
            let (key, attribute) = entry?;
            let (_, pattern) = key.value();
            let pattern = std::str::from_utf8(pattern)?;
            let stored = attribute.value();
            let (&stored_attribute, regex_value) =
                stored.split_first().ok_or("empty stored geosite value")?;
            let (kind, payload) = regex_value
                .split_first()
                .ok_or("empty stored geosite regex value")?;
            let regex = match *kind {
                REGEX_DFA => {
                    match dense::DFA::from_bytes(payload) {
                        Ok((dfa, _)) => CachedRegex::Dfa(Box::new(dfa.to_owned())),
                        // The key retains the source so persisted DFAs remain
                        // recoverable after a format/compiler incompatibility.
                        Err(_) => CachedRegex::Source(Box::new(regex::Regex::new(pattern)?)),
                    }
                }
                REGEX_SOURCE => {
                    // Validate the persisted fallback source before storing it in
                    // the runtime cache.
                    CachedRegex::Source(Box::new(regex::Regex::new(pattern)?))
                }
                _ => return Err("unknown stored geosite regex encoding".into()),
            };
            Ok(RegexEntry {
                attribute: stored_attribute,
                regex,
            })
        })
        .collect()
}

pub(super) fn find_domain(
    read: &redb::ReadTransaction,
    regex_cache: &RegexCache,
    list: &str,
    domain: &str,
) -> Result<bool> {
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
            .is_some_and(|stored| {
                attribute == AttrType::Nil || stored.value().first() == Some(&(attribute as u8))
            }))
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
        if attribute != AttrType::Nil
            && stored_attribute.value().first() != Some(&(attribute as u8))
        {
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
    if let Some(regexes) = regex_cache.get(&name) {
        for entry in regexes {
            if (attribute == AttrType::Nil || entry.attribute == attribute as u8)
                && entry.regex.is_match(&domain)?
            {
                return Ok(true);
            }
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
            let value = if kind == SiteMatchType::Regex {
                value.to_owned()
            } else {
                value.to_ascii_lowercase()
            };
            assert!(!attribute.contains("@"));
            let attribute =
                AttrType::parse(&attribute.to_ascii_lowercase()).unwrap_or(AttrType::Nil);
            let mut stored_value = vec![attribute as u8];
            if kind == SiteMatchType::Regex {
                regex::Regex::new(&value)?;
                match dense::Builder::new()
                    .configure(dense::Config::new().start_kind(StartKind::Unanchored))
                    .build(&value)
                {
                    Ok(dfa) => {
                        stored_value.push(REGEX_DFA);
                        stored_value.extend(dfa.to_bytes_little_endian().0);
                    }
                    // Some regex syntax supported by `regex` has no DFA
                    // implementation. Preserve its source for a compiled
                    // runtime fallback while storing DFAs for supported rules.
                    Err(_) => {
                        stored_value.push(REGEX_SOURCE);
                    }
                }
            }
            table.insert((kind as u8, value.as_bytes()), stored_value.as_slice())?;
        }
    }
    Ok(())
}
