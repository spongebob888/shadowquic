use std::{net::IpAddr, sync::Arc};

use async_trait::async_trait;
use tokio::net::lookup_host;

use crate::error::SError;

type Result<T> = std::result::Result<T, SError>;

/// Owns the resolver tag map and the shared DNS cache used by every resolver.
#[derive(Clone)]
pub struct ResolverManager {}
pub struct DnsCache {}
pub struct Resolver {}

impl Default for ResolverManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolverManager {
    pub fn new() -> Self {
        Self {}
    }

    pub(crate) fn resolver(&self, _tag: &str) -> Option<Arc<Resolver>> {
        Some(Arc::new(Resolver {}))
    }
}

#[async_trait]
pub trait DnsService: Send + Sync {
    async fn resolve(&self, domain: &str) -> Result<Vec<IpAddr>> {
        let addrs = lookup_host((domain, 0)).await?;
        Ok(addrs.map(|addr| addr.ip()).collect())
    }
}

#[async_trait]
impl DnsService for Resolver {}
