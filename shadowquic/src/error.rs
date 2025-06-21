use std::io;
use std::result;
use thiserror::Error;
use crate::quic::QuicErrorRepr;

#[derive(Error, Debug)]
pub enum SError {
    #[error("Protocol Violated")]
    ProtocolViolation,
    #[error("Protocol Unimplemented")]
    ProtocolUnimpl,
    #[error("IO Error:{0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    QuicError(#[from] QuicErrorRepr),
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

pub type SResult<T> = result::Result<T, SError>;

// #[derive(Error, Debug)]
// #[error(transparent)]
// pub struct QuicError(#[from] QuicErrorRepr);

