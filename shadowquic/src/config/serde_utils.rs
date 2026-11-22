use std::{fmt, ops::Deref};

use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeSeed, Error as _, IntoDeserializer, MapAccess, SeqAccess, Visitor},
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

/// A whole-number duration. Bare values and `ms` use milliseconds, and `s` uses seconds.
/// `LB` is the inclusive minimum in milliseconds and the default value.
#[derive(Clone, Debug)]
pub struct DurationMs<const LB: u64 = 0>(u64);

impl<const LB: u64> From<u64> for DurationMs<{ LB }> {
    fn from(value: u64) -> Self {
        DurationMs(value)
    }
}
impl<const LB: u64> From<DurationMs<{ LB }>> for u64 {
    fn from(val: DurationMs<{ LB }>) -> Self {
        val.0
    }
}
impl<const LB: u64> Deref for DurationMs<LB> {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<const LB: u64> Default for DurationMs<LB> {
    fn default() -> Self {
        Self(LB)
    }
}

impl<'de, const LB: u64> Deserialize<'de> for DurationMs<LB> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(
            untagged,
            expecting = "a whole-number duration such as 12345, 50ms, or 50s"
        )]
        enum Input {
            Milliseconds(u64),
            Text(String),
        }

        let ms = match Input::deserialize(deserializer)? {
            Input::Milliseconds(ms) => ms,
            Input::Text(text) => {
                let (number, scale) = if let Some(ms) = text.strip_suffix("ms") {
                    (ms, 1)
                } else if let Some(seconds) = text.strip_suffix('s') {
                    (seconds, 1000)
                } else if let Some(min) = text.strip_suffix("min") {
                    (min, 60_1000)
                } else {
                    (text.as_str(), 1)
                };
                let value = number.parse::<u64>().map_err(|_| {
                    D::Error::custom(format!(
                        "invalid duration {text:?}, expected 12345, 50ms, or 50s"
                    ))
                })?;
                value.checked_mul(scale).ok_or_else(|| {
                    D::Error::custom(format!("duration {text:?} exceeds u64 milliseconds"))
                })?
            }
        };
        if ms < LB {
            return Err(D::Error::custom(format!(
                "duration {ms}ms is below the minimum of {LB}ms"
            )));
        }
        Ok(Self(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::DurationMs;

    #[test]
    fn duration_ms_enforces_inclusive_lower_bound_in_milliseconds() {
        type Bounded = DurationMs<2000>;
        assert_eq!(*Bounded::default(), 2000);
        for value in ["1999", "1999ms", "1s"] {
            let error = serde_saphyr::from_str::<Bounded>(value).unwrap_err();
            assert!(error.to_string().contains("minimum of 2000ms"), "{error}");
        }
        for (value, expected) in [("2000", 2000), ("2000ms", 2000), ("2s", 2000), ("3s", 3000)] {
            assert_eq!(*serde_saphyr::from_str::<Bounded>(value).unwrap(), expected);
        }
    }

    #[test]
    fn duration_ms_accepts_bare_values_and_unit_suffixes() {
        for (value, expected) in [
            ("12345", 12345),
            ("50ms", 50),
            ("50s", 50000),
            ("'50s'", 50000),
        ] {
            assert_eq!(
                *serde_saphyr::from_str::<DurationMs>(value).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn duration_ms_rejects_invalid_values() {
        for value in [
            "-1",
            "1.5",
            "18446744073709551616",
            "18446744073709552s",
            "-1ms",
            "1.5s",
            "50m",
            "invalid",
        ] {
            assert!(
                serde_saphyr::from_str::<DurationMs>(value).is_err(),
                "{value}"
            );
        }
    }
}
