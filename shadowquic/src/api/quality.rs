use serde::Serialize;
use tokio::sync::Mutex;

use crate::{
    msgs::squic::ConnStats,
    quic::QuicConnection,
    squic::{SQConn, SQConnStats},
};

use super::{ErrorBody, millis};

#[async_trait::async_trait]
pub trait SQStatsTrait: Send + Sync {
    async fn get_sq_stats(&self) -> Option<SQConnStats>;
}

#[async_trait::async_trait]
impl SQStatsTrait for crate::shadowquic::outbound::ShadowQuicClient {
    async fn get_sq_stats(&self) -> Option<SQConnStats> {
        cached_stats(&self.quic_conn).await
    }
}

#[async_trait::async_trait]
impl SQStatsTrait for crate::sunnyquic::outbound::SunnyQuicClient {
    async fn get_sq_stats(&self) -> Option<SQConnStats> {
        cached_stats(&self.quic_conn).await
    }
}

/// A busy connection cache may be reconnecting; never wait for network setup here.
pub(crate) async fn cached_stats<C: QuicConnection>(
    cache: &Mutex<Option<SQConn<C>>>,
) -> Option<SQConnStats> {
    let conn = cache.try_lock().ok()?.as_ref()?.clone();
    if conn.close_reason().is_some() {
        return None;
    }
    conn.stats.lock().await.clone()
}

#[derive(Serialize)]
pub(super) struct ConnQuality {
    pub sq_conn_stats: Option<ConnSample>,
    pub probe: Option<ProbeResult>,
}

#[derive(Serialize)]
pub(super) struct ConnSample {
    uplink: ConnStats,
    downlink: ConnStats,
    sample_age_ms: u64,
}

impl From<SQConnStats> for ConnSample {
    fn from(stats: SQConnStats) -> Self {
        Self {
            uplink: stats.uplink,
            downlink: stats.downlink,
            sample_age_ms: millis(stats.time.elapsed()),
        }
    }
}

#[derive(Clone, Serialize)]
pub(super) struct ProbeResult {
    pub url: String,
    pub status: &'static str,
    pub latency_ms: Option<u64>,
    pub measured_at: String,
    pub error: Option<ErrorBody>,
}
