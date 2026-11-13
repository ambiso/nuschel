// SPDX-License-Identifier: EUPL-1.2
//! Resolution is explicit policy authority. ProxyRequired never invokes the
//! system resolver, including for its configured DoH/proxy bootstrap path.
use super::{
    Budget, Cancel, supervise,
    transport::{Endpoint, HttpUrl},
};
use nuschl_ipc::preview_fetch::Purpose;
use nuschl_link_preview::{
    metadata::Unavailable,
    policy::{self, Route, Target},
};
use std::{
    net::{IpAddr, ToSocketAddrs},
    path::Path,
    sync::Arc,
    time::Duration,
};

pub(super) async fn addresses(
    executable: &Path,
    route: &Route,
    target: &Target,
    cancel: &Arc<Cancel>,
    budget: &mut Budget,
) -> Result<Vec<IpAddr>, Unavailable> {
    budget.check(cancel)?;
    let host = target.host().trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return policy::validate_addresses(&[ip]).map_err(|_| Unavailable::Address);
    }
    let addresses = match route {
        Route::Direct => system(host, target.port(), cancel, budget).await?,
        Route::Proxy { dns, .. } => {
            let mut answers = Vec::new();
            for kind in [1u16, 28] {
                budget.check(cancel)?;
                let request = HttpUrl::dns(route, host, kind)?;
                let mut response = None;
                for ip in dns.bootstrap() {
                    budget.check(cancel)?;
                    let endpoint = Endpoint::resolver(route, *ip)?;
                    match supervise::http(
                        executable,
                        route,
                        endpoint,
                        &request,
                        Purpose::Dns,
                        cancel,
                        budget,
                    )
                    .await
                    {
                        Ok(value) => {
                            response = Some(value);
                            break;
                        }
                        Err(error) => {
                            budget.check(cancel)?;
                            if matches!(error, Unavailable::TooLarge) {
                                return Err(error);
                            }
                        }
                    }
                }
                let response = response.ok_or(Unavailable::Proxy)?;
                if response.status != 200
                    || !matches!(
                        response.media_type.as_deref(),
                        Some("application/dns-json" | "application/json")
                    )
                {
                    return Err(Unavailable::Proxy);
                }
                let bytes = response
                    .decode_body(Purpose::Dns)
                    .map_err(|_| Unavailable::Malformed)?;
                answers.extend(json_answers(&bytes, host, kind)?);
                if answers.len() > 16 {
                    return Err(Unavailable::Address);
                }
            }
            answers
        }
    };
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

fn json_answers(bytes: &[u8], name: &str, kind: u16) -> Result<Vec<IpAddr>, Unavailable> {
    if bytes.len() > Purpose::Dns.body_limit() {
        return Err(Unavailable::TooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| Unavailable::Malformed)?;
    if value["Status"].as_u64() != Some(0)
        || value.get("TC").is_some_and(|v| v.as_bool() != Some(false))
    {
        return Err(Unavailable::Proxy);
    }
    let questions = value["Question"]
        .as_array()
        .filter(|q| q.len() == 1)
        .ok_or(Unavailable::Malformed)?;
    if questions[0]["type"].as_u64() != Some(u64::from(kind))
        || !questions[0]["name"]
            .as_str()
            .is_some_and(|q| q.len() <= 254 && q.trim_end_matches('.').eq_ignore_ascii_case(name))
    {
        return Err(Unavailable::Malformed);
    }
    let Some(answers) = value.get("Answer") else {
        return Ok(Vec::new());
    };
    let answers = answers
        .as_array()
        .filter(|a| a.len() <= 16)
        .ok_or(Unavailable::TooLarge)?;
    let mut out = Vec::new();
    for answer in answers {
        let record = answer["type"].as_u64().ok_or(Unavailable::Malformed)?;
        let data = answer["data"]
            .as_str()
            .filter(|s| s.len() <= 1024 && !s.chars().any(char::is_control))
            .ok_or(Unavailable::Malformed)?;
        if matches!(record, 1 | 28) {
            let ip = data.parse::<IpAddr>().map_err(|_| Unavailable::Malformed)?;
            if !policy::public_ip(ip) {
                return Err(Unavailable::Address);
            }
            if record == 1 && !ip.is_ipv4() || record == 28 && !ip.is_ipv6() {
                return Err(Unavailable::Malformed);
            }
            if record == u64::from(kind) {
                out.push(ip)
            }
        } else if record == 5 {
            // Bounded CNAME text is not a connection request or a new lookup.
            if data.len() > 254
                || !data.trim_end_matches('.').contains('.')
                || data
                    .chars()
                    .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-')))
            {
                return Err(Unavailable::Malformed);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn dns_question_type_and_all_addresses_are_bound_and_private_answers_never_filtered() {
        let reply = |answers| {
            serde_json::to_vec(&json!({"Status":0,"TC":false,"Question":[{"name":"example.org.","type":1}],"Answer":answers})).unwrap()
        };
        let public = json!({"type":1,"data":"93.184.215.14"});
        assert_eq!(
            json_answers(&reply(json!([public.clone()])), "example.org", 1).unwrap(),
            vec!["93.184.215.14".parse::<IpAddr>().unwrap()]
        );
        assert!(
            json_answers(
                &reply(json!([public.clone(),{"type":1,"data":"127.0.0.1"}])),
                "example.org",
                1
            )
            .is_err()
        );
        assert!(
            json_answers(
                &reply(json!([{"type":28,"data":"::ffff:127.0.0.1"},public.clone()])),
                "example.org",
                1
            )
            .is_err()
        );
        assert!(json_answers(&reply(json!([public.clone()])), "different.org", 1).is_err());
        assert!(json_answers(&reply(json!([public.clone()])), "example.org", 28).is_err());
        assert!(
            json_answers(
                &reply(serde_json::Value::Array(vec![public; 17])),
                "example.org",
                1
            )
            .is_err()
        );
    }
}
