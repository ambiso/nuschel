// SPDX-License-Identifier: EUPL-1.2
//! Server icons from the server's website, where `nuschl_client::favicon`
//! says to look: only HTTPS, with short timeouts and a size cap. The bytes
//! are decoded by the sandboxed content worker; here they are only fetched
//! and checked for an image signature.
use nuschl_client::favicon::{icon_link, is_image, resolve, sites};
use std::time::Duration;

const LIMIT: u64 = 512 * 1024;

/// The icon's bytes, if the server's website has one.
pub fn fetch(host: &str) -> Option<Vec<u8>> {
    if host.parse::<std::net::IpAddr>().is_ok() || !host.contains('.') {
        return None;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(10)))
        .max_redirects(3)
        .user_agent("Nuschl")
        .build()
        .into();
    let get = |url: &str| {
        let mut response = agent.get(url).call().ok()?;
        response
            .body_mut()
            .with_config()
            .limit(LIMIT)
            .read_to_vec()
            .ok()
    };
    for site in sites(host) {
        if let Some(icon) = get(&format!("https://{site}/favicon.ico")).filter(|b| is_image(b)) {
            return Some(icon);
        }
        let linked = get(&format!("https://{site}/"))
            .and_then(|page| icon_link(&String::from_utf8_lossy(&page)))
            .and_then(|href| resolve(site, &href));
        if let Some(icon) = linked.and_then(|url| get(&url)).filter(|b| is_image(b)) {
            return Some(icon);
        }
    }
    None
}
