use crate::{ProxyRequest, error::SResult};

#[async_trait::async_trait]
pub trait ProxyTransform {
    async fn transform(&self, proxy: ProxyRequest) -> SResult<ProxyRequest>;
}