use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::SError;

pub mod socks5;
pub mod squic;
/// SEncode is a asyc trait for encoding. It can be automatically derived for struct by the SEncode macro
/// as long as fields are SEncode.
/// For enum, the macro will encode discriminant as u8/u16... defined by `#[repr(*)]` before encoding the content. So the enum can be decoded by first reading a u8/u16...  
/// and then decoding the content based on the value of disriminant.
/// For enum, at most one field is supported.
/// named field is not supported for enum for SDecode macro.
/// `#[repr(*)]` is required for enum to specify the type of discriminant.
pub(crate) trait SEncode {
    async fn encode<T: AsyncWrite + Unpin>(&self, s: &mut T) -> Result<(), SError>;
}

/// A async decoding trait. It can be automatically derived for struct by the SDecode macro as long as fields are SDecode.
/// For enum, the macro will first read a u8/u16... defined by `#[repr(*)]` as discriminant and then decode the content based on the value of disriminant.
/// At most one field is supported for enum. Named field is not supported for enum for SDecode macro.
/// `#[repr(*)]` is required for enum to specify the type of discriminant.
pub(crate) trait SDecode
where
    Self: Sized,
{
    async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError>;
}

impl<S: SEncode> SEncode for Arc<S> {
    async fn encode<T: AsyncWrite + Unpin>(&self, s: &mut T) -> Result<(), SError> {
        self.as_ref().encode(s).await?;
        Ok(())
    }
}
impl<S: SDecode> SDecode for Arc<S> {
    async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        let data = S::decode(s).await?;
        Ok(Arc::new(data))
    }
}
impl<const N: usize> SEncode for [u8; N] {
    async fn encode<T: AsyncWrite + Unpin>(&self, s: &mut T) -> Result<(), SError> {
        s.write_all(self).await?;
        Ok(())
    }
}
impl<const N: usize> SDecode for [u8; N] {
    async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
        let mut data = [0u8; N];
        s.read_exact(&mut data).await?;
        Ok(data)
    }
}
