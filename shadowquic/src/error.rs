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
    #[error("QUIC Connect Error:{0}")]
    QuicConnect(#[from] quinn::ConnectError),
    #[error("QUIC Connection Error:{0}")]
    QuicConnection(#[from] quinn::ConnectionError),
    #[error("QUIC Write Error:{0}")]
    QuicWrite(#[from] quinn::WriteError),
    #[error("QUIC ReadExact Error:{0}")]
    QuicReadExactError(#[from] quinn::ReadExactError),
    #[error("QUIC SendDatagramError:{0}")]
    QuicSendDatagramError(#[from] quinn::SendDatagramError),
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
    ChannelError(String),
    #[error("UDP session closed closed due to: {0}")]
    UDPSessionClosed(String),
    #[error("socks error: {0}")]
    SocksError(String),
}
