// SPDX-License-Identifier: EUPL-1.2
//! The owned preview pipeline. Cancellation closes transport first; every
//! started parser/content child is then awaited through actual retirement.
use super::{
    Budget, Cancel, io_stage, resolve,
    transport::{self, Endpoint, HttpUrl},
};
use nuschl_ipc::{
    host, preview,
    preview_fetch::{self as wire, Purpose, Reply, Request, Response},
};
use nuschl_link_preview::{
    card::Card,
    metadata::{ExtractedMetadata, Unavailable},
    policy::Target,
};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) async fn pipeline(
    executable: &Path,
    original: &Target,
    cancel: &Arc<Cancel>,
    budget: &mut Budget,
) -> Result<Card, Unavailable> {
    budget.check(cancel)?;
    let (document, document_url) = target_http(
        executable,
        original.clone(),
        Purpose::Document,
        cancel,
        budget,
    )
    .await?;
    if document.media_type.as_deref() != Some("text/html") {
        return Err(Unavailable::MediaType);
    }
    let extracted = offline(
        executable,
        preview::Request::Document {
            schema: preview::SCHEMA,
            body: document.body,
            charset: document.charset,
            truncated: false,
        },
        cancel,
        budget,
    )
    .await?;
    let metadata = match extracted {
        preview::Response::Metadata { metadata, .. } => ExtractedMetadata {
            title: metadata.title,
            description: metadata.description,
            image: metadata.image,
            image_alt: metadata.image_alt,
        },
        preview::Response::Unavailable {
            reason: preview::Unavailable::Encoding,
            ..
        } => return Err(Unavailable::Encoding),
        preview::Response::Unavailable {
            reason: preview::Unavailable::TooLarge,
            ..
        } => return Err(Unavailable::TooLarge),
        _ => return Err(Unavailable::NoMetadata),
    };
    if !metadata.is_bounded() {
        return Err(Unavailable::Malformed);
    }
    let image = if let Some(reference) = metadata.image.as_ref() {
        match document_url.join(reference) {
            Ok(target) => prepare_image(executable, target, &metadata.image_alt, cancel, budget)
                .await
                .ok(),
            Err(_) => None,
        }
    } else {
        None
    };
    budget.check(cancel)?;
    Card::new(original, &metadata, image)
        .or_else(|_| Card::new(original, &metadata, None))
        .map_err(|_| Unavailable::NoMetadata)
}

async fn prepare_image(
    executable: &Path,
    target: Target,
    alt: &str,
    cancel: &Arc<Cancel>,
    budget: &mut Budget,
) -> Result<nuschl_core::preview::EmbeddedImage, Unavailable> {
    budget.check(cancel)?;
    let (image, _) = target_http(executable, target, Purpose::Image, cancel, budget).await?;
    if !matches!(
        image.media_type.as_deref(),
        Some("image/png" | "image/jpeg" | "image/webp" | "image/gif")
    ) {
        return Err(Unavailable::MediaType);
    }
    match offline(
        executable,
        preview::Request::PrepareImage {
            schema: preview::SCHEMA,
            image: image.body,
            alt: alt.to_owned(),
        },
        cancel,
        budget,
    )
    .await?
    {
        preview::Response::Image { image, .. } => {
            nuschl_link_preview::card::check_image(&image).map_err(|_| Unavailable::Malformed)?;
            Ok(image)
        }
        _ => Err(Unavailable::Malformed),
    }
}

async fn offline(
    executable: &Path,
    request: preview::Request,
    cancel: &Arc<Cancel>,
    budget: &Budget,
) -> Result<preview::Response, Unavailable> {
    budget.check(cancel)?;
    // Reserve the existing helper's 5s operation + 2s clean-exit window.
    if budget
        .deadline
        .saturating_duration_since(tokio::time::Instant::now())
        < Duration::from_secs(7)
    {
        return Err(Unavailable::Timeout);
    }
    #[cfg(test)]
    super::tests::admit(super::tests::Admission::Offline);
    let mut job = Box::pin(host::preview(executable, request));
    let response = tokio::select! {
        result=&mut job=>result.map_err(|_|Unavailable::Malformed)?,
        _=cancel.cancelled()=>{let _=job.await;return Err(Unavailable::Network)}
    };
    // host::preview returns only after its owned task has reaped its child.
    budget.check(cancel)?;
    if !response.is_bounded() {
        return Err(Unavailable::Malformed);
    }
    Ok(response)
}

async fn target_http(
    executable: &Path,
    mut target: Target,
    purpose: Purpose,
    cancel: &Arc<Cancel>,
    budget: &mut Budget,
) -> Result<(Response, Target), Unavailable> {
    loop {
        budget.check(cancel)?;
        let addresses = resolve::addresses(&target, cancel, budget).await?;
        let ip = *addresses.first().ok_or(Unavailable::Address)?;
        let endpoint = Endpoint::target(&target, ip)?;
        let request = HttpUrl::target(&target);
        let response = http(executable, endpoint, &request, purpose, cancel, budget).await?;
        if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            budget.check(cancel)?;
            budget.redirects += 1;
            if budget.redirects > 3 {
                return Err(Unavailable::TooLarge);
            }
            let next = target
                .join(response.location.as_deref().ok_or(Unavailable::Malformed)?)
                .map_err(|_| Unavailable::Address)?;
            if target.request_url().scheme() == "https" && next.request_url().scheme() != "https" {
                return Err(Unavailable::Address);
            }
            target = next; // Fresh complete policy/DNS/pinning on every hop.
        } else if response.status == 200 {
            return Ok((response, target));
        } else {
            return Err(Unavailable::Network);
        }
    }
}

async fn http(
    executable: &Path,
    endpoint: Endpoint,
    url: &HttpUrl,
    purpose: Purpose,
    cancel: &Arc<Cancel>,
    budget: &mut Budget,
) -> Result<Response, Unavailable> {
    budget.check(cancel)?;
    let request = transport::get_request(url, purpose)?;
    #[cfg(test)]
    super::tests::admit(super::tests::Admission::HttpChild);
    let nuschl_sandbox::Worker {
        mut child,
        input: child_input,
        output: child_output,
        containment: _containment,
    } = nuschl_sandbox::spawn_preview(executable, nuschl_sandbox::PreviewOperation::Fetch)
        .await
        .map_err(|_| Unavailable::Network)?;
    let mut retirement = host::preview_lifecycle::Retirement::new(&mut child);
    let result = async {
        // Trusted bwrap status opens/retains the original namespace-init task
        // handle BEFORE a socket or any untrusted input is admitted.
        // Startup observation is already bounded by ready(). Ordinary cancel
        // does not discard a partial trusted receipt: no TCP exists yet, no
        // request bytes are sent, and the next check forbids all new work.
        retirement.ready().await.map_err(|_| Unavailable::Network)?;
        budget.check(cancel)?;
        let mut socket = transport::open(endpoint, cancel, budget).await?;
        io_stage(cancel, budget, 5, socket.write_all(&request)).await?;
        budget.check(cancel)?;
        let mut input = child_input;
        let mut output = child_output;
        let mut replies = 0usize;
        let mut id = 1u64;
        let frame = wire::encode_request(
            id,
            &Request::Start {
                schema: wire::SCHEMA,
                purpose,
            },
        )
        .map_err(|_| Unavailable::Malformed)?;
        io_stage(cancel, budget, 5, input.write_all(&frame)).await?;
        if !matches!(
            reply(&mut output, id, &mut replies, cancel, budget).await?,
            Reply::More
        ) {
            return Err(Unavailable::Malformed);
        }
        let mut raw = 0usize;
        let response = loop {
            budget.check(cancel)?;
            if id > wire::MAX_CHUNKS as u64 {
                return Err(Unavailable::TooLarge);
            }
            let mut bytes = [0; wire::MAX_CHUNK_BYTES];
            let count = io_stage(cancel, budget, 5, socket.read(&mut bytes)).await?;
            budget.raw(count)?;
            raw = raw.checked_add(count).ok_or(Unavailable::TooLarge)?;
            if raw > purpose.raw_limit() {
                return Err(Unavailable::TooLarge);
            }
            id = id.checked_add(1).ok_or(Unavailable::TooLarge)?;
            let frame = wire::encode_request(
                id,
                &Request::Chunk {
                    data: wire::encode_data(&bytes[..count]),
                    eof: count == 0,
                },
            )
            .map_err(|_| Unavailable::Malformed)?;
            io_stage(cancel, budget, 5, input.write_all(&frame)).await?;
            match reply(&mut output, id, &mut replies, cancel, budget).await? {
                Reply::More if count != 0 => {}
                Reply::Complete(response) => {
                    response
                        .decode_body(purpose)
                        .map_err(|_| Unavailable::Malformed)?;
                    break response;
                }
                Reply::Rejected(wire::Failure::TooLarge) => return Err(Unavailable::TooLarge),
                _ => return Err(Unavailable::Malformed),
            }
        };
        drop(input);
        // No more outbound bytes after completion/cancellation. Waiting for the
        // child's clean exit never keeps this approved connection open.
        drop(socket);
        let mut extra = [0];
        if io_stage(cancel, budget, 2, output.read(&mut extra)).await? != 0 {
            return Err(Unavailable::Malformed);
        }
        if !io_stage(cancel, budget, 2, child.wait()).await?.success() {
            return Err(Unavailable::Malformed);
        }
        Ok(response)
    }
    .await;
    // On every error the socket was dropped with the exchange future above,
    // before child retirement. kill_on_drop alone would not prove actual exit.
    // Wrapper exit alone is not namespace teardown. The tracker retains the
    // original task handle and holds this actor/credit on unknown retirement.
    retirement.retire(&mut child).await;
    result
}

async fn reply(
    output: &mut tokio::process::ChildStdout,
    id: u64,
    total: &mut usize,
    cancel: &Cancel,
    budget: &Budget,
) -> Result<Reply, Unavailable> {
    let size = io_stage(cancel, budget, 5, output.read_u32()).await? as usize;
    if size == 0 || size > wire::MAX_RESPONSE_BYTES {
        return Err(Unavailable::TooLarge);
    }
    *total = total.checked_add(size + 4).ok_or(Unavailable::TooLarge)?;
    if *total > wire::MAX_RESPONSE_BYTES + 64 * 1024 {
        return Err(Unavailable::TooLarge);
    }
    let mut bytes = vec![0; size];
    io_stage(cancel, budget, 5, output.read_exact(&mut bytes)).await?;
    wire::decode_reply(id, &bytes).map_err(|_| Unavailable::Malformed)
}
