//! Live structured logging for the REST API.
//!
//! Embedding applications must attach [`LogLayer`] to their tracing subscriber.
//! The CLI installs it automatically when `restful-api` is enabled.

use std::{
    fmt,
    sync::{Arc, OnceLock},
};

use futures_util::{SinkExt, StreamExt};
use hyper::{Request, body::Incoming};
use hyper_util::rt::TokioIo;
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, oneshot};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::server::create_response,
        protocol::{Role, WebSocketConfig},
    },
};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

use super::{ApiError, HttpResponse};

const CAPACITY: usize = 1024;
const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(test)]
pub(super) static TEST_LOG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn sender() -> &'static broadcast::Sender<Arc<str>> {
    static SENDER: OnceLock<broadcast::Sender<Arc<str>>> = OnceLock::new();
    SENDER.get_or_init(|| broadcast::channel(CAPACITY).0)
}

#[derive(Default)]
struct Fields(Map<String, Value>);

impl Visit for Fields {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().into(), format!("{value:?}").into());
    }
}

/// Broadcasts tracing events and their span fields to live API log subscribers.
/// Attach a filter to this layer to control the records exposed by the API.
#[derive(Clone, Copy, Debug, Default)]
pub struct LogLayer;

/// Capture application TRACE and Quinn WARN independently of console filtering.
pub fn trace_filter() -> tracing_subscriber::filter::Targets {
    tracing_subscriber::filter::Targets::new()
        .with_target("shadowquic", tracing::Level::TRACE)
        .with_target("quinn", tracing::Level::WARN)
}

impl<S> Layer<S> for LogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut fields = Fields::default();
            attrs.record(&mut fields);
            span.extensions_mut().insert(fields);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id)
            && let Some(fields) = span.extensions_mut().get_mut::<Fields>()
        {
            values.record(fields);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let sender = sender();
        if sender.receiver_count() == 0 {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let name = fields.0.remove("event");
        let spans: Vec<_> = ctx
            .event_scope(event)
            .into_iter()
            .flat_map(|scope| scope.from_root())
            .map(|span| {
                let fields = span
                    .extensions()
                    .get::<Fields>()
                    .map(|fields| fields.0.clone())
                    .unwrap_or_default();
                json!({"name": span.name(), "fields": fields})
            })
            .collect();
        let record = json!({
            "timestamp": super::timestamp(std::time::SystemTime::now()),
            "level": event.metadata().level().as_str(),
            "target": event.metadata().target(),
            "event": name,
            "fields": fields.0,
            "spans": spans,
        });
        // Sending to a bounded broadcast channel never waits for readers.
        let _ = sender.send(Arc::from(record.to_string()));
    }
}

pub(super) struct Upgrade {
    upgraded: hyper::upgrade::OnUpgrade,
    receiver: broadcast::Receiver<Arc<str>>,
}

pub(super) type UpgradeSender = Arc<std::sync::Mutex<Option<oneshot::Sender<Upgrade>>>>;

pub(super) fn upgrade(
    request: Request<Incoming>,
    sender: &UpgradeSender,
) -> Result<HttpResponse, ApiError> {
    let (parts, _) = request.into_parts();
    let mut request = Request::from_parts(parts, ());
    let response = create_response(&request).map_err(ApiError::invalid)?;
    let upgrade = Upgrade {
        upgraded: hyper::upgrade::on(&mut request),
        receiver: self::sender().subscribe(),
    };
    sender
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| ApiError::invalid("connection already upgraded"))?
        .send(upgrade)
        .map_err(|_| ApiError::invalid("connection closed"))?;
    Ok(response.map(|_| http_body_util::Full::new(bytes::Bytes::new())))
}

impl Upgrade {
    // Kept inside the HTTP connection task so API shutdown cancels all streams.
    pub(super) async fn serve(self) {
        let Ok(Ok(upgraded)) = tokio::time::timeout(IO_TIMEOUT, self.upgraded).await else {
            return;
        };
        let config = WebSocketConfig::default()
            .max_message_size(Some(4096))
            .max_frame_size(Some(4096));
        let mut socket =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(config))
                .await;
        let mut receiver = self.receiver;
        loop {
            tokio::select! {
                record = receiver.recv() => {
                    let text = match record {
                        Ok(record) => record.to_string(),
                        Err(broadcast::error::RecvError::Lagged(dropped)) => json!({
                            "event": "api:logs_lagged", "dropped": dropped,
                        }).to_string(),
                        Err(broadcast::error::RecvError::Closed) => return,
                    };
                    if !matches!(tokio::time::timeout(IO_TIMEOUT, socket.send(Message::Text(text.into()))).await, Ok(Ok(()))) {
                        return;
                    }
                }
                message = socket.next() => {
                    match message {
                        Some(Ok(Message::Ping(_))) => {
                            // Tungstenite queued the pong while reading the ping.
                            if !matches!(tokio::time::timeout(IO_TIMEOUT, socket.flush()).await, Ok(Ok(()))) { return; }
                        }
                        Some(Ok(Message::Close(_))) => {
                            let _ = tokio::time::timeout(IO_TIMEOUT, socket.flush()).await;
                            return;
                        }
                        None | Some(Err(_)) => return,
                        _ => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    #[test]
    fn websocket_trace_filter_is_independent_of_console_level() {
        let _guard = TEST_LOG_LOCK.blocking_lock();
        let mut receiver = sender().subscribe();
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::sink)
                    .with_filter(tracing_subscriber::filter::LevelFilter::ERROR),
            )
            .with(LogLayer.with_filter(trace_filter()));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::trace_span!(target: "shadowquic::test", "request", req_id = 17_u64);
            let _entered = span.enter();
            tracing::trace!(target: "shadowquic::test", event = "test:trace", "trace record");
            tracing::debug!(target: "shadowquic::test", event = "test:debug", "debug record");
            tracing::info!(target: "shadowquic::test", event = "test:info", "info record");
            tracing::warn!(target: "shadowquic::test", event = "test:warn", "warn record");
            tracing::error!(target: "shadowquic::test", event = "test:error", "error record");
            tracing::trace!(target: "quinn::test", event = "test:quinn", "QUIC trace");
            tracing::debug!(target: "quinn::test", event = "test:quinn_debug", "QUIC debug");
            tracing::info!(target: "quinn::test", event = "test:quinn_info", "QUIC info");
            tracing::warn!(target: "quinn::test", event = "test:quinn_warn", "QUIC warning");
            tracing::error!(target: "quinn::test", event = "test:quinn_error", "QUIC error");
        });
        for (level, target) in [
            ("TRACE", "shadowquic::test"),
            ("DEBUG", "shadowquic::test"),
            ("INFO", "shadowquic::test"),
            ("WARN", "shadowquic::test"),
            ("ERROR", "shadowquic::test"),
            ("WARN", "quinn::test"),
            ("ERROR", "quinn::test"),
        ] {
            let record: Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
            assert_eq!(record["level"], level);
            assert_eq!(record["target"], target);
            assert_eq!(record["spans"][0]["fields"]["req_id"], 17);
        }
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn slow_log_receivers_do_not_block_and_missing_event_is_null() {
        let _guard = TEST_LOG_LOCK.blocking_lock();
        let mut receiver = sender().subscribe();
        let subscriber = tracing_subscriber::registry().with(LogLayer);
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..CAPACITY + 1 {
                tracing::info!("record without event");
            }
        });
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        let record: Value = serde_json::from_str(&receiver.try_recv().unwrap()).unwrap();
        assert!(record["event"].is_null());
        assert_eq!(record["fields"]["message"], "record without event");
    }
}
