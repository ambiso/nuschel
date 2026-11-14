// SPDX-License-Identifier: EUPL-1.2
//! Target addresses: an IP literal as written, or the system resolver.
use super::{Budget, Cancel};
use nuschl_link_preview::{
    metadata::Unavailable,
    policy::{self, Target},
};
use std::{
    net::{IpAddr, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};

pub(super) async fn addresses(
    target: &Target,
    cancel: &Arc<Cancel>,
    budget: &mut Budget,
) -> Result<Vec<IpAddr>, Unavailable> {
    budget.check(cancel)?;
    let host = target.host().trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return policy::validate_addresses(&[ip]).map_err(|_| Unavailable::Address);
    }
    let addresses = system(host, target.port(), cancel, budget).await?;
    budget.check(cancel)?;
    // Validate the complete bounded set, including a private answer mixed into
    // otherwise public results. Never silently choose only the public subset.
    policy::validate_addresses(&addresses).map_err(|_| Unavailable::Address)
}

async fn system(
    host: &str,
    port: u16,
    cancel: &Arc<Cancel>,
    budget: &Budget,
) -> Result<Vec<IpAddr>, Unavailable> {
    budget.check(cancel)?;
    let host = host.to_owned();
    let before = cancel.clone();
    #[cfg(test)]
    super::tests::admit(super::tests::Admission::Dns);
    let mut resolver = tokio::task::spawn_blocking(move || {
        if before.is_cancelled() {
            return Err(Unavailable::Network);
        }
        (host.as_str(), port)
            .to_socket_addrs()
            .map(|values| values.take(17).map(|a| a.ip()).collect::<Vec<_>>())
            .map_err(|_| Unavailable::Network)
    });
    let deadline = budget
        .deadline
        .min(tokio::time::Instant::now() + Duration::from_secs(5));
    tokio::select! {
        result=&mut resolver=>result.map_err(|_|Unavailable::Network)?,
        _=cancel.cancelled()=>{
            // libc may finish/retransmit this already-started OS lookup. It
            // cannot be forcibly cancelled through std. Keep the actual owned
            // handle AND sole pipeline credit until it really returns; no later
            // lookup/socket/worker/image/redirect can start from this result.
            let _=resolver.await;Err(Unavailable::Network)
        },
        _=tokio::time::sleep_until(deadline)=>{
            cancel.revoke();let _=resolver.await;Err(Unavailable::Timeout)
        }
    }
}
