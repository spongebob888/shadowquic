use std::collections::HashSet;

use serde::Deserialize;

use crate::error::SError;

use super::{Config, OutboundCfg};

/// Dispatch each new session to the selected member outbound.
///
/// ```yaml
/// tag: choice
/// type: selector
/// outbounds: [direct, proxy]
/// selected: direct
/// ```
///
/// Use this tag in routing or as an inbound's `default-outbound`. Change the
/// selection with `PUT /api/v1/outbounds/choice/selection` and the JSON body
/// `{"selected":"proxy"}`. Existing TCP sessions and UDP associations keep
/// their member. References may point forward or to other selectors; cycles
/// are rejected. Changes last until restart.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SelectorOutCfg {
    /// Required label for this endpoint.
    pub tag: String,
    /// Allowed outbound tags, in display order. Must be nonempty and unique.
    pub outbounds: Vec<String>,
    /// Initial member. Defaults to the first member; API changes are not persisted.
    pub selected: Option<String>,
}

impl SelectorOutCfg {
    pub(crate) fn validate(&self) -> Result<(), SError> {
        let mut seen = HashSet::new();
        if self.outbounds.is_empty()
            || self
                .outbounds
                .iter()
                .any(|tag| tag.trim().is_empty() || tag == &self.tag || !seen.insert(tag))
        {
            return Err(SError::InvalidConfig(format!(
                "selector `{}` requires nonempty, unique outbound tags other than itself",
                self.tag
            )));
        }
        if let Some(selected) = &self.selected
            && !self.outbounds.contains(selected)
        {
            return Err(SError::InvalidConfig(format!(
                "selector `{}` selected tag is not a member: {selected}",
                self.tag
            )));
        }
        Ok(())
    }
}

impl Config {
    pub(super) fn validate_selectors(&self) -> Result<(), SError> {
        let tags: HashSet<_> = self.outbound_tags().collect();
        let mut pending = Vec::new();
        for outbound in &self.outbounds {
            if let OutboundCfg::Selector(cfg) = outbound {
                cfg.validate()?;
                for tag in &cfg.outbounds {
                    if !tags.contains(tag.as_str()) {
                        return Err(SError::InvalidConfig(format!(
                            "selector `{}` references unknown outbound: {tag}",
                            cfg.tag
                        )));
                    }
                }
                pending.push(cfg);
            }
        }
        // Resolve the graph without recursion. Check every possible member,
        // since the API can select members that are initially inactive.
        let mut resolved = tags;
        for cfg in &pending {
            resolved.remove(cfg.tag.as_str());
        }
        while !pending.is_empty() {
            let before = pending.len();
            pending.retain(|cfg| {
                if cfg
                    .outbounds
                    .iter()
                    .all(|tag| resolved.contains(tag.as_str()))
                {
                    resolved.insert(cfg.tag.as_str());
                    false
                } else {
                    true
                }
            });
            if pending.len() == before {
                return Err(SError::InvalidConfig(
                    "selector outbounds contain a cycle".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(outbounds: &str) -> Config {
        serde_saphyr::from_str(&format!(
            "inbounds: [{{tag: inbound, type: socks, bind-addr: '127.0.0.1:0'}}]\noutbounds:\n{outbounds}"
        )).unwrap()
    }

    #[test]
    fn selectors_reject_invalid_members_and_cycles() {
        for (selector, message) in [
            ("{outbounds: []}", "requires nonempty"),
            ("{outbounds: ['']}", "requires nonempty"),
            ("{outbounds: [direct, direct]}", "unique outbound"),
            ("{outbounds: [choice]}", "other than itself"),
            ("{outbounds: [missing]}", "unknown outbound"),
            ("{outbounds: [direct], selected: missing}", "not a member"),
        ] {
            let selector = selector.trim_start_matches('{');
            let cfg = config(&format!(
                "  - {{tag: choice, type: selector, {selector}\n  - {{tag: direct, type: direct}}"
            ));
            let error = cfg.validate().unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
        }
        let cfg = config(
            "- {tag: a, type: selector, outbounds: [direct, b]}\n\
               - {tag: b, type: selector, outbounds: [c]}\n\
               - {tag: c, type: selector, outbounds: [a]}\n\
               - {tag: direct, type: direct}",
        );
        assert!(cfg.validate().unwrap_err().to_string().contains("cycle"));
    }

    #[tokio::test]
    async fn selectors_build_forward_references_and_preserve_initial_selection() {
        let cfg = config(
            "- {tag: outer, type: selector, outbounds: [inner, direct], selected: direct}\n\
               - {tag: inner, type: selector, outbounds: [direct, blocked]}\n\
               - {tag: direct, type: direct}\n\
               - {tag: blocked, type: drop}",
        );
        let manager = cfg.build_manager().await.unwrap();
        assert_eq!(manager.default_outbound, "outer");
        assert_eq!(
            manager.selectable_outbounds["outer"].state().selected,
            "direct"
        );
        assert_eq!(
            manager.selectable_outbounds["inner"].state().selected,
            "direct"
        );
        manager.selectable_outbounds["inner"]
            .select("blocked")
            .unwrap();
        assert_eq!(
            manager.selectable_outbounds["outer"].state().selected,
            "direct"
        );
    }
}
