use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SError {
    #[error("Protocol Violated")]
    ProtocolViolation,
    #[error("Protocol Unimplemented")]
    ProtocolUnimpl,
    #[error("IO Error:{0}")]
    Io(#[from] io::Error),
    #[error("QUIC Connect Error")]
    QuicConnect(#[from] quinn::ConnectError),
    #[error("QUIC Connection Error")]
    QuicConnection(#[from] quinn::ConnectionError),
    #[error("JLS Authentication failed")]
    JlsAuthFailed,
    #[error("Rustls Error")]
    RustlsError(#[from] rustls::Error),
    #[error("Outbound unavailable")]
    OutboundUnavailable,
    #[error("Inbound unavailable")]
    InboundUnavailable,
    #[error("hostname can't be resolved")]
    DomainResolveFailed,
    #[error("mpsc channel error: {0}")]
    ChannelError(String)
}
