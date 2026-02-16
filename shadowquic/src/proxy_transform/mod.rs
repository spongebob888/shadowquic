use crate::{ProxyRequest, error::SResult};

pub mod sq_shim;
pub mod tls;

#[async_trait::async_trait]
pub trait ProxyTransform {
    async fn transform(&self, proxy: ProxyRequest) -> SResult<ProxyRequest>;
}
