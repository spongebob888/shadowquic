use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::SError;

pub mod socks5;
pub mod squic;

pub(crate) trait SEncode {
    async fn encode<T: AsyncWrite + Unpin>(&self, s: &mut T) -> Result<(), SError>;
}

pub(crate) trait SDecode
where
    Self: Sized,
{
    async fn decode<T: AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError>;
}
