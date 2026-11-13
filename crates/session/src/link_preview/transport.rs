// SPDX-License-Identifier: EUPL-1.2
//! Numeric-only sockets. Page URLs and explicitly configured resolver services
//! have distinct constructors; no HTTP response can grant service authority.
use super::{Budget, Cancel, io_stage};
use nuschl_link_preview::{
    metadata::Unavailable,
    policy::{self, Route, Target},
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
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
    pub(super) fn resolver(route: &Route, ip: IpAddr) -> Result<Self, Unavailable> {
        let Route::Proxy { dns, .. } = route else {
            return Err(Unavailable::Proxy);
        };
        if !dns.bootstrap().contains(&ip) {
            return Err(Unavailable::Address);
        }
        Ok(Self {
            address: SocketAddr::new(
                ip,
                dns.url()
                    .port_or_known_default()
                    .ok_or(Unavailable::Proxy)?,
            ),
            hostname: hostname(dns.url().host_str().ok_or(Unavailable::Proxy)?),
            tls: true,
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
    route: &Route,
    endpoint: Endpoint,
    cancel: &Cancel,
    budget: &mut Budget,
) -> Result<Stream, Unavailable> {
    budget.connection(cancel)?;
    let peer = match route {
        Route::Direct => endpoint.address,
        Route::Proxy { proxy, .. } => proxy.address(),
    };
    // SocketAddr overload: never give the socket layer a name to resolve again.
    let mut socket = io_stage(cancel, budget, 5, TcpStream::connect(peer)).await?;
    if socket.peer_addr().ok() != Some(peer) {
        return Err(Unavailable::Address);
    }
    socket.set_nodelay(true).map_err(|_| Unavailable::Network)?;
    if matches!(route, Route::Proxy { .. }) {
        socks(&mut socket, endpoint.address, cancel, budget).await?;
    }
    if !endpoint.tls {
        return Ok(Box::new(socket));
    }
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
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

async fn socks(
    socket: &mut TcpStream,
    target: SocketAddr,
    cancel: &Cancel,
    budget: &Budget,
) -> Result<(), Unavailable> {
    io_stage(cancel, budget, 5, socket.write_all(&[5, 1, 0]))
        .await
        .map_err(|_| Unavailable::Proxy)?;
    let mut choice = [0; 2];
    io_stage(cancel, budget, 5, socket.read_exact(&mut choice))
        .await
        .map_err(|_| Unavailable::Proxy)?;
    if choice != [5, 0] {
        return Err(Unavailable::Proxy);
    }
    let mut request = vec![5, 1, 0];
    match target.ip() {
        IpAddr::V4(ip) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
    }
    request.extend_from_slice(&target.port().to_be_bytes());
    io_stage(cancel, budget, 5, socket.write_all(&request))
        .await
        .map_err(|_| Unavailable::Proxy)?;
    let mut head = [0; 4];
    io_stage(cancel, budget, 5, socket.read_exact(&mut head))
        .await
        .map_err(|_| Unavailable::Proxy)?;
    if head[..3] != [5, 0, 0] {
        return Err(Unavailable::Proxy);
    }
    let length = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut n = [0];
            io_stage(cancel, budget, 5, socket.read_exact(&mut n))
                .await
                .map_err(|_| Unavailable::Proxy)?;
            if n[0] == 0 {
                return Err(Unavailable::Proxy);
            }
            n[0] as usize
        }
        _ => return Err(Unavailable::Proxy),
    };
    // The proxy's bind address is consumed as inert data; never resolved or dialled.
    let mut bound = [0; 257];
    io_stage(
        cancel,
        budget,
        5,
        socket.read_exact(&mut bound[..length + 2]),
    )
    .await
    .map_err(|_| Unavailable::Proxy)?;
    Ok(())
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
        nuschl_ipc::preview_fetch::Purpose::Dns => "application/dns-json, application/json",
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
    pub(super) fn dns(route: &Route, name: &str, kind: u16) -> Result<Self, Unavailable> {
        let Route::Proxy { dns, .. } = route else {
            return Err(Unavailable::Proxy);
        };
        let mut url = dns.url().clone();
        url.query_pairs_mut()
            .append_pair("name", name)
            .append_pair("type", &kind.to_string());
        Ok(Self {
            authority: format!(
                "{}{}",
                url.host_str().ok_or(Unavailable::Proxy)?,
                url.port().map(|p| format!(":{p}")).unwrap_or_default()
            ),
            path_query: format!("{}?{}", url.path(), url.query().unwrap_or_default()),
        })
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
    #[test]
    fn service_bootstrap_cannot_be_created_from_a_page_or_direct_route() {
        use nuschl_link_preview::policy::{DnsSettings, Mode, Settings};
        assert!(Endpoint::resolver(&Route::Direct, "127.0.0.1".parse().unwrap()).is_err());
        let route = Settings {
            mode: Mode::ProxyRequired,
            proxy: Some("socks5://127.0.0.1:9050".into()),
            dns: Some(DnsSettings {
                url: "https://resolver.example/dns-query".into(),
                bootstrap: vec!["192.168.1.2".parse().unwrap()],
            }),
        }
        .route()
        .unwrap();
        assert!(Endpoint::resolver(&route, "192.168.1.2".parse().unwrap()).is_ok());
        assert!(Endpoint::resolver(&route, "127.0.0.1".parse().unwrap()).is_err());
        let request = HttpUrl::dns(&route, "page.example", 28).unwrap();
        assert_eq!(request.authority, "resolver.example");
        assert_eq!(request.path_query, "/dns-query?name=page.example&type=28");
    }
}
