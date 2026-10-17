use std::time::Duration;

use crate::{shadowquic::outbound::ShadowQuicClient, squic::SQConnStats, sunnyquic::outbound::SunnyQuicClient};

#[async_trait::async_trait]
pub(super) trait SQStatsTrait {
    async fn get_sq_stats(&self) -> Option<SQConnStats>;
}

#[async_trait::async_trait]
impl SQStatsTrait for ShadowQuicClient {
    async fn get_sq_stats(&self) -> Option<SQConnStats> {
         let conn = self.quic_conn.lock().await;
         let Some(conn) = conn.as_ref() else {
            return None;
         };

         let stats = conn.stats.lock().await;
         stats.to_owned()
    }
}
#[async_trait::async_trait]
impl SQStatsTrait for SunnyQuicClient {
    async fn get_sq_stats(&self) -> Option<SQConnStats> {
         let conn = self.quic_conn.lock().await;
         let Some(conn) = conn.as_ref() else {
            return None;
         };

         let stats = conn.stats.lock().await;
         stats.to_owned()
    }

}

pub(super) struct ConnQuality {
    pub sq_conn_stats: Option<SQConnStats>,
    pub latency: Option<Duration>,
}