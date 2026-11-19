use super::Result;
use redb::{Key, ReadableTable, TableDefinition, TypeName, Value};
use regex_automata::{
    Input,
    dfa::{Automaton, StartKind, dense, sparse},
};
use serde::Deserialize;
use std::{borrow::Cow, cmp::Ordering, collections::HashSet, path::Path};

// Schema 4 uses tagged rule keys and retains the source of compiled regexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum SiteMatchType<'a> {
    Full(&'a str) = 0,
    Domain(&'a str) = 1,
    Regex(&'a str) = 2,
    Keyword(&'a str) = 3,
    CompiledRegex(&'a str, &'a [u8]) = 4,
}

// Text keys are tag + UTF-8. Compiled keys are tag + redb's (&str, &[u8]) tuple
// encoding, borrowing the original pattern and DFA bytes when reading.
impl Value for SiteMatchType<'_> {
    type SelfType<'a>
        = SiteMatchType<'a>
    where
        Self: 'a;
    type AsBytes<'a>
        = Vec<u8>
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let (&kind, value) = data.split_first().expect("geosite key has no rule type");
        match kind {
            0 => SiteMatchType::Full(std::str::from_utf8(value).unwrap()),
            1 => SiteMatchType::Domain(std::str::from_utf8(value).unwrap()),
            2 => SiteMatchType::Regex(std::str::from_utf8(value).unwrap()),
            3 => SiteMatchType::Keyword(std::str::from_utf8(value).unwrap()),
            4 => {
                let (source, dfa) = <(&str, &[u8])>::from_bytes(value);
                SiteMatchType::CompiledRegex(source, dfa)
            }
            _ => panic!("invalid geosite rule type: {kind}"),
        }
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        let (kind, text) = match value {
            SiteMatchType::Full(text) => (0, text),
            SiteMatchType::Domain(text) => (1, text),
            SiteMatchType::Regex(text) => (2, text),
            SiteMatchType::Keyword(text) => (3, text),
            SiteMatchType::CompiledRegex(source, dfa) => {
                let mut encoded = <(&str, &[u8])>::as_bytes(&(*source, *dfa));
                encoded.insert(0, 4);
                return encoded;
            }
        };
        let mut encoded = Vec::with_capacity(1 + text.len());
        encoded.push(kind);
        encoded.extend_from_slice(text.as_bytes());
        encoded
    }

    fn type_name() -> TypeName {
        TypeName::new("shadowquic::geosite::SiteMatchType")
    }
}

impl Key for SiteMatchType<'_> {
    fn compare(data1: &[u8], data2: &[u8]) -> Ordering {
        data1.cmp(data2)
    }

    fn separator<'a>(left: &'a [u8], right: &'a [u8]) -> Cow<'a, [u8]> {
        // Internal B-tree separators are compared as bytes, never decoded as
        // rules. Shorten them to avoid repeating large DFA keys in branch pages.
        <&[u8]>::separator(left, right)
    }

    fn min_encoded_key() -> Option<Cow<'static, [u8]>> {
        Some(Cow::Borrowed(&[0]))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum AttrType {
    Nil = 0,
    Ads = 1 << 0,
    NotCn = 1 << 1,
    Cn = 1 << 2,
    Telemetry = 1 << 3,
}

impl AttrType {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "ads" => Some(Self::Ads),
            "!cn" => Some(Self::NotCn),
            "cn" => Some(Self::Cn),
            "telemetry" => Some(Self::Telemetry),
            _ => None,
        }
    }

    fn matches(self, attributes: u32) -> bool {
        self == Self::Nil || attributes & self as u32 != 0
    }
}

pub(super) type GeositeTable<'a> = TableDefinition<'a, SiteMatchType<'static>, u32>;

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
    let indexed = |key: SiteMatchType| -> Result<bool> {
        Ok(table
            .get(key)?
            .is_some_and(|stored| attribute.matches(stored.value())))
    };
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    if indexed(SiteMatchType::Full(&domain))? {
        return Ok(true);
    }
    let mut suffix = domain.as_str();
    loop {
        if indexed(SiteMatchType::Domain(suffix))? {
            return Ok(true);
        }
        match suffix.split_once('.') {
            Some((_, rest)) => suffix = rest,
            None => break,
        }
    }
    for entry in table.range(SiteMatchType::Keyword("")..SiteMatchType::CompiledRegex("", &[]))? {
        let (key, stored_attribute) = entry?;
        if !attribute.matches(stored_attribute.value()) {
            continue;
        }
        let SiteMatchType::Keyword(pattern) = key.value() else {
            unreachable!("keyword range contains a different rule type");
        };
        if domain.contains(pattern) {
            return Ok(true);
        }
    }
    for entry in table.range(SiteMatchType::CompiledRegex("", &[])..)? {
        let (key, stored_attribute) = entry?;
        if !attribute.matches(stored_attribute.value()) {
            continue;
        }
        let SiteMatchType::CompiledRegex(_, dfa_bytes) = key.value() else {
            unreachable!("compiled regex range contains a different rule type");
        };
        let dfa = deserialize_dfa(dfa_bytes)?;
        if dfa.try_search_fwd(&Input::new(&domain))?.is_some() {
            return Ok(true);
        }
    }
    for entry in table.range(SiteMatchType::Regex("")..SiteMatchType::Keyword(""))? {
        let (key, stored_attribute) = entry?;
        if !attribute.matches(stored_attribute.value()) {
            continue;
        }
        let SiteMatchType::Regex(pattern) = key.value() else {
            unreachable!("regex range contains a different rule type");
        };
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

pub(super) fn selection_key(selected: &[String]) -> String {
    let mut names: Vec<_> = selected
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    names.sort_unstable();
    names.dedup();
    names.join("\0")
}

pub(super) fn import_geosite(
    source: &Path,
    write: &redb::WriteTransaction,
    selected: &[String],
) -> Result<()> {
    let source = std::fs::read_to_string(source)?;
    let data: Geosite = serde_saphyr::from_str(&source)?;
    if data.lists.is_empty() {
        return Err("geosite contains no lists".into());
    }
    for name in selected {
        if name.is_empty() || name.contains(['\0', '@']) {
            return Err(format!("invalid geosite list name: {name:?}").into());
        }
        if !data
            .lists
            .iter()
            .any(|list| list.name.eq_ignore_ascii_case(name))
        {
            return Err(format!("geosite list not found: {name}").into());
        }
    }
    let mut warned_attributes = HashSet::new();
    for list in data.lists {
        if !selected.is_empty()
            && !selected
                .iter()
                .any(|name| list.name.eq_ignore_ascii_case(name))
        {
            continue;
        }
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
            let mut attributes = 0u32;
            for attribute in attribute.split(',') {
                let attribute = attribute
                    .trim()
                    .trim_start_matches('@')
                    .to_ascii_lowercase();
                if attribute.is_empty() {
                    continue;
                }
                match AttrType::parse(&attribute) {
                    Some(attribute) => attributes |= attribute as u32,
                    None if warned_attributes.insert(attribute.clone()) => {
                        tracing::warn!(event = "database:unknown_attribute", %attribute, list = %list.name, "ignoring unknown geosite attribute");
                    }
                    None => {}
                }
            }
            let normalized;
            let dfa_bytes;
            let key = if kind == "regexp" {
                let compiled = dense::Builder::new()
                    .configure(dense::Config::new().start_kind(StartKind::Unanchored))
                    .build(value)
                    .ok()
                    .and_then(|dfa| dfa.to_sparse().ok());
                match compiled {
                    Some(dfa) => {
                        dfa_bytes = dfa.to_bytes_little_endian();
                        SiteMatchType::CompiledRegex(value, &dfa_bytes)
                    }
                    None => {
                        regex::Regex::new(value)?;
                        SiteMatchType::Regex(value)
                    }
                }
            } else {
                normalized = value.to_ascii_lowercase();
                match kind {
                    "full" => SiteMatchType::Full(&normalized),
                    "domain" => SiteMatchType::Domain(&normalized),
                    "keyword" => SiteMatchType::Keyword(&normalized),
                    _ => return Err(format!("unsupported geosite rule type: {kind}").into()),
                }
            };
            // Repeated rules and case-insensitive list aliases retain every attribute.
            let attributes = table
                .get(key)?
                .map_or(attributes, |old| old.value() | attributes);
            table.insert(key, attributes)?;
        }
    }
    Ok(())
}
