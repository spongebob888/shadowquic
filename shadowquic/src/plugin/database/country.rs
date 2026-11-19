use maxminddb::geoip2::Country;
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, SeqAccess, Visitor, value::MapAccessDeserializer},
};
use std::fmt;

/// Country or category labels decoded from MaxMind and Mihomo MMDB records.
///
/// Accepts `country.iso_code`, a root string, or a root array of strings.
/// Labels borrow the database bytes; only arrays allocate a label vector.
#[derive(Debug)]
pub enum MmdbCountryRecord<'a> {
    Single(Option<&'a str>),
    Multiple(Vec<&'a str>),
}

impl MmdbCountryRecord<'_> {
    /// Iterate over every nonempty label, preserving its original case.
    pub fn codes(&self) -> impl Iterator<Item = &str> {
        let codes = match self {
            Self::Single(code) => code.as_slice(),
            Self::Multiple(codes) => codes.as_slice(),
        };
        codes.iter().copied().filter(|code| !code.is_empty())
    }

    /// Test membership in any of the record's labels, ignoring ASCII case.
    pub fn contains(&self, list: &str) -> bool {
        self.codes().any(|code| code.eq_ignore_ascii_case(list))
    }
}

impl<'de> Deserialize<'de> for MmdbCountryRecord<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RecordVisitor;

        impl<'de> Visitor<'de> for RecordVisitor {
            type Value = MmdbCountryRecord<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a country map, a country code string, or an array of strings")
            }

            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                code: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(MmdbCountryRecord::Single(Some(code)))
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut codes = Vec::new();
                while let Some(code) = sequence.next_element::<&'de str>()? {
                    codes.push(code);
                }
                Ok(MmdbCountryRecord::Multiple(codes))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let record = Country::deserialize(MapAccessDeserializer::new(map))?;
                Ok(MmdbCountryRecord::Single(record.country.iso_code))
            }
        }

        deserializer.deserialize_any(RecordVisitor)
    }
}
