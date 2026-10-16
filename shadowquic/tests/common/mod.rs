use shadowquic::{Outbound, ProxyRequest, error::SError};
use tokio::sync::mpsc::Sender;

/// Deliver dispatched requests to the test so it controls their lifetime.
pub struct CaptureOutbound(pub Sender<ProxyRequest>);

#[async_trait::async_trait]
impl Outbound for CaptureOutbound {
    async fn handle(&self, req: ProxyRequest) -> Result<(), SError> {
        self.0
            .send(req)
            .await
            .map_err(|_| SError::OutboundUnavailable)
    }
}
