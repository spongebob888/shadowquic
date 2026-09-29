use std::fmt;

use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor},
};

pub(super) fn deserialize_inbounds<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<super::InboundCfg>, D::Error> {
    deserialize_endpoints(deserializer, "inbound")
}

pub(super) fn deserialize_outbounds<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<super::OutboundCfg>, D::Error> {
    deserialize_endpoints(deserializer, "outbound")
}

// Keep endpoint deserialization strict, adding a default tag only for a legacy
// single endpoint object. Lists continue to require explicit tags.
fn deserialize_endpoints<'de, D, T>(deserializer: D, tag: &'static str) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Endpoints<T>(&'static str, std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for Endpoints<T> {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an endpoint object or a list of tagged endpoints")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut endpoints = Vec::new();
            while let Some(endpoint) = seq.next_element()? {
                endpoints.push(endpoint);
            }
            Ok(endpoints)
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            let endpoint = T::deserialize(de::value::MapAccessDeserializer::new(DefaultTag {
                map,
                tag: self.0,
                seen_tag: false,
                injected: false,
            }))?;
            Ok(vec![endpoint])
        }
    }

    deserializer.deserialize_any(Endpoints(tag, std::marker::PhantomData))
}

struct DefaultTag<A> {
    map: A,
    tag: &'static str,
    seen_tag: bool,
    injected: bool,
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for DefaultTag<A> {
    type Error = A::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        if let Some(key) = self.map.next_key::<String>()? {
            self.seen_tag |= key == "tag";
            seed.deserialize(key.into_deserializer()).map(Some)
        } else if !self.seen_tag {
            self.seen_tag = true;
            self.injected = true;
            seed.deserialize("tag".into_deserializer()).map(Some)
        } else {
            Ok(None)
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        if self.injected {
            self.injected = false;
            seed.deserialize(self.tag.into_deserializer())
        } else {
            self.map.next_value_seed(seed)
        }
    }
}

pub fn parse_bps(input: &str) -> Result<u64, String> {
    let s = input.trim();

    if s.is_empty() {
        return Err("empty bandwidth string".to_string());
    }

    let (num_str, multiplier) = match s.as_bytes().last().copied() {
        Some(b'K') | Some(b'k') => (&s[..s.len() - 1], 1024f64),
        Some(b'M') | Some(b'm') => (&s[..s.len() - 1], 1024f64 * 1024f64),
        Some(b'G') | Some(b'g') => (&s[..s.len() - 1], 1024f64 * 1024f64 * 1024f64),
        Some(b'0'..=b'9') => (s, 1f64),
        _ => return Err(format!("invalid bandwidth suffix: {input}")),
    };

    let value: f64 = num_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid bandwidth number: {input}"))?;

    if !value.is_finite() {
        return Err(format!("invalid bandwidth number: {input}"));
    }

    if value < 0.0 {
        return Err(format!("bandwidth must be non-negative: {input}"));
    }

    let result = value * multiplier;

    if result > u64::MAX as f64 {
        return Err(format!("bandwidth value overflow: {input}"));
    }

    Ok(result.round() as u64)
}

pub fn deserialize_bps<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct BpsVisitor;

    impl<'de> Visitor<'de> for BpsVisitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an integer bps value or a string like \"30M\" or \"1.5G\"")
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value)
        }

        fn visit_u32<E>(self, value: u32) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value as u64)
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if value < 0 {
                return Err(E::custom("bandwidth must be non-negative"));
            }
            Ok(value as u64)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_bps(value).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_bps(&value).map_err(E::custom)
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if !value.is_finite() {
                return Err(E::custom("bandwidth must be finite"));
            }
            if value < 0.0 {
                return Err(E::custom("bandwidth must be non-negative"));
            }
            Ok(value.round() as u64)
        }
    }

    deserializer.deserialize_any(BpsVisitor)
}
