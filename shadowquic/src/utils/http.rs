use bytes::Bytes;
use http_body_util::Empty;
use hyper::{Request, Response, body::Incoming, client::conn::http1, http::request::Builder};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use url::{Host, Position, Url};

use crate::msgs::socks5::{AddrOrDomain, SocksAddr};

/// The URL must have been validated by the caller for its supported schemes.
pub(crate) fn destination(url: &Url) -> Result<SocksAddr, &'static str> {
    let port = url.port_or_known_default().ok_or("missing HTTP port")?;
    let addr = match url.host().ok_or("missing HTTP host")? {
        Host::Ipv4(ip) => AddrOrDomain::V4(ip.octets()),
        Host::Ipv6(ip) => AddrOrDomain::V6(ip.octets()),
        Host::Domain(domain) => AddrOrDomain::Domain(domain.as_bytes().to_vec().into()),
    };
    Ok(SocksAddr { addr, port })
}

pub(crate) fn get_request(url: &Url) -> Builder {
    Request::builder()
        .method(hyper::Method::GET)
        .uri(&url[Position::BeforePath..Position::AfterQuery])
        .header(
            hyper::header::HOST,
            &url[Position::BeforeHost..Position::AfterPort],
        )
}

/// Owns the HTTP driver until the caller finishes reading the response body.
pub(crate) struct HttpConnection {
    sender: http1::SendRequest<Empty<Bytes>>,
    driver: tokio::task::JoinHandle<()>,
}

impl HttpConnection {
    pub(crate) async fn new(
        stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
        max_buf_size: Option<usize>,
    ) -> Result<Self, hyper::Error> {
        let mut builder = http1::Builder::new();
        if let Some(max_buf_size) = max_buf_size {
            builder.max_buf_size(max_buf_size);
        }
        let (sender, connection) = builder.handshake(TokioIo::new(stream)).await?;
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Self { sender, driver })
    }

    pub(crate) async fn send_request(
        &mut self,
        request: Request<Empty<Bytes>>,
    ) -> Result<Response<Incoming>, hyper::Error> {
        self.sender.send_request(request).await
    }
}

impl Drop for HttpConnection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_urls_preserve_destination_authority_and_request_target() {
        for (url, dst, authority, target) in [
            (
                "http://example.test",
                "example.test:80",
                "example.test",
                "/",
            ),
            (
                "https://example.test/a%20b?q=x%2Fy#fragment",
                "example.test:443",
                "example.test",
                "/a%20b?q=x%2Fy",
            ),
            (
                "http://127.0.0.1:8080/?",
                "127.0.0.1:8080",
                "127.0.0.1:8080",
                "/?",
            ),
            (
                "https://[::1]:8443/path?q=value",
                "[::1]:8443",
                "[::1]:8443",
                "/path?q=value",
            ),
        ] {
            let url = Url::parse(url).unwrap();
            assert_eq!(destination(&url).unwrap().to_string(), dst);
            let request = get_request(&url).body(Empty::<Bytes>::new()).unwrap();
            assert_eq!(request.method(), hyper::Method::GET);
            assert_eq!(request.headers()[hyper::header::HOST], authority);
            assert_eq!(request.uri().to_string(), target);
        }
    }
}
