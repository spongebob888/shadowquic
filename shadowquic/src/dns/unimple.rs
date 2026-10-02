use std::sync::Arc;
/// Owns the resolver tag map and the shared DNS cache used by every resolver.
#[derive(Clone)]
pub struct ResolverManager {}
pub struct DnsCache {}
pub struct Resolver {}

impl ResolverManager {
    pub(crate) fn new() -> Self {
        Self {}
    }

    pub(crate) fn cache(&self) -> Arc<DnsCache> {
        Arc::new(DnsCache {})
    }

    pub(crate) fn resolver(&self, tag: &str) -> Option<Arc<Resolver>> {
        None
    }
}
