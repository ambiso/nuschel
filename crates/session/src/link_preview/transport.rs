// SPDX-License-Identifier: EUPL-1.2
//! Numeric-only sockets to validated page addresses.
use super::{Budget, Cancel, io_stage};
use nuschl_link_preview::{
    metadata::Unavailable,
    policy::{self, Target},
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};

pub(super) trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}
pub(super) type Stream = Box<dyn Transport>;
pub(super) struct Endpoint {
    address: SocketAddr,
    hostname: String,
    tls: bool,
}
impl Endpoint {
    pub(super) fn target(target: &Target, ip: IpAddr) -> Result<Self, Unavailable> {
        if !policy::public_ip(ip) {
            return Err(Unavailable::Address);
        }
        Ok(Self {
            address: SocketAddr::new(ip, target.port()),
            hostname: hostname(target.host()),
            tls: target.request_url().scheme() == "https",
        })
    }
}
fn hostname(host: &str) -> String {
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
        .to_owned()
}

pub(super) async fn open(
    endpoint: Endpoint,
    cancel: &Cancel,
    budget: &mut Budget,
) -> Result<Stream, Unavailable> {
    budget.connection(cancel)?;
    let peer = endpoint.address;
    // SocketAddr overload: never give the socket layer a name to resolve again.
    #[cfg(test)]
    super::tests::admit(super::tests::Admission::Socket);
    let socket = io_stage(cancel, budget, 5, TcpStream::connect(peer)).await?;
    if socket.peer_addr().ok() != Some(peer) {
        return Err(Unavailable::Address);
    }
    socket.set_nodelay(true).map_err(|_| Unavailable::Network)?;
    if !endpoint.tls {
        return Ok(Box::new(socket));
    }
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    #[cfg(test)]
    let roots = super::tests::with_fixture_root(roots);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| Unavailable::Network)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config.resumption = rustls::client::Resumption::disabled();
    let name = rustls::pki_types::ServerName::try_from(endpoint.hostname)
        .map_err(|_| Unavailable::Address)?;
    let mut tls = io_stage(
        cancel,
        budget,
        5,
        tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, socket),
    )
    .await?;
    if tls
        .get_ref()
        .1
        .alpn_protocol()
        .is_some_and(|p| p != b"http/1.1")
    {
        return Err(Unavailable::Network);
    }
    tls.get_mut().1.set_buffer_limit(Some(64 * 1024));
    Ok(Box::new(tls))
}

/// Fixed GET, no cookies, authorization, referrer, decompression or ambient
/// proxy configuration. Arguments come only from validated Target/config URLs.
pub(super) fn get_request(
    url: &HttpUrl,
    purpose: nuschl_ipc::preview_fetch::Purpose,
) -> Result<Vec<u8>, Unavailable> {
    let path = &url.path_query;
    let host = &url.authority;
    if path.len() > 4096
        || host.len() > 512
        || path.chars().any(char::is_control)
        || host.chars().any(char::is_control)
    {
        return Err(Unavailable::Address);
    }
    let accept = match purpose {
        nuschl_ipc::preview_fetch::Purpose::Document => "text/html",
        nuschl_ipc::preview_fetch::Purpose::Image => "image/png, image/jpeg, image/webp, image/gif",
    };
    Ok(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: Nuschl-Link-Preview/1\r\nAccept: {accept}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n").into_bytes())
}
/// Small owned request projection: constructing it cannot resolve or connect.
pub(super) struct HttpUrl {
    pub(super) authority: String,
    pub(super) path_query: String,
}
impl HttpUrl {
    pub(super) fn target(target: &Target) -> Self {
        let url = target.request_url();
        let authority = format!(
            "{}{}",
            url.host_str().expect("validated host"),
            url.port().map(|p| format!(":{p}")).unwrap_or_default()
        );
        let path_query = format!(
            "{}{}",
            url.path(),
            url.query().map(|q| format!("?{q}")).unwrap_or_default()
        );
        Self {
            authority,
            path_query,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_has_original_authority_no_fragment_and_only_fixed_headers() {
        let target = Target::parse("https://Example.ORG/a%20b?q=two#private-fragment").unwrap();
        let request = String::from_utf8(
            get_request(
                &HttpUrl::target(&target),
                nuschl_ipc::preview_fetch::Purpose::Document,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(request.starts_with("GET /a%20b?q=two HTTP/1.1\r\nHost: example.org\r\n"));
        assert!(!request.contains("private-fragment"));
        assert!(
            !request.contains("Cookie:")
                && !request.contains("Authorization:")
                && !request.contains("Referer:")
                && !request.contains("Proxy-Authorization:")
        );
        assert!(request.contains("Accept-Encoding: identity\r\nConnection: close\r\n"));
        let invalid = HttpUrl {
            authority: "example.org\r\nCookie: secret".into(),
            path_query: "/".into(),
        };
        assert!(get_request(&invalid, nuschl_ipc::preview_fetch::Purpose::Document).is_err());
        assert!(Endpoint::target(&target, "127.0.0.1".parse().unwrap()).is_err());
        assert!(Endpoint::target(&target, "93.184.215.14".parse().unwrap()).is_ok());
    }
}
