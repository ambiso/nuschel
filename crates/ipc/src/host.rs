// SPDX-License-Identifier: EUPL-1.2
//! Native supervisor adapter. The portable client model does not depend on this module.

use std::{io, path::Path, time::Duration};

use nuschl_core::MessageDocument;
use nuschl_sandbox::{Role, Worker};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Child,
    time::timeout,
};

use crate::{
    ContentRequest, ContentResponse, EncodedImage, Envelope, MAX_FRAME_BYTES, PROTOCOL_VERSION,
    PreparedContent, write_frame,
};

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] crate::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("content worker exceeded its deadline")]
    Timeout,
    #[error("content worker returned an invalid response or exited unsuccessfully")]
    InvalidResponse,
    #[error("content processing rejected: {0}")]
    Rejected(String),
    #[error(transparent)]
    Sandbox(#[from] nuschl_sandbox::Error),
}

/// Seconds of wall-clock time, and on Linux of CPU time, a request may take.
fn budget(request: &ContentRequest) -> u64 {
    match request {
        // Several encodes of a large picture; the user waits for it.
        ContentRequest::EncodeImage { .. } => 15,
        _ => 5,
    }
}

/// Render literal message text.
pub async fn render_content(executable: &Path, text: &str) -> Result<MessageDocument, Error> {
    rendered(
        one(
            executable,
            ContentRequest::Render {
                text: text.to_owned(),
            },
        )
        .await?,
    )
}

/// Render many messages (a history page) in one worker; `None` where one failed.
pub async fn render_many(executable: &Path, texts: &[String]) -> Vec<Option<MessageDocument>> {
    let requests: Vec<_> = texts
        .iter()
        .map(|text| ContentRequest::Render { text: text.clone() })
        .collect();
    match exchange(executable, &requests).await {
        Ok(results) => results
            .into_iter()
            .map(|result| result.and_then(rendered).ok())
            .collect(),
        Err(_) => vec![None; texts.len()],
    }
}

/// Prepare many messages (received in a burst) in one worker, each as
/// [`prepare_content`] would: `(text, legacy_html)` in, `None` where one failed.
pub async fn prepare_contents(
    executable: &Path,
    messages: &[(String, bool)],
) -> Vec<Option<PreparedContent>> {
    let requests: Vec<_> = messages
        .iter()
        .map(|(text, legacy_html)| match legacy_html {
            true => ContentRequest::LegacyHtml { html: text.clone() },
            false => ContentRequest::Render { text: text.clone() },
        })
        .collect();
    match exchange(executable, &requests).await {
        Ok(results) => prepared_each(results, messages),
        Err(_) => vec![None; messages.len()],
    }
}

/// The replies to [`prepare_contents`], each as [`prepare_content`] takes
/// it: `None` for one that failed, is of the wrong kind or holds a document
/// over its budgets (`within_limits`), each on its own, so one message's
/// reply never decides another's.
fn prepared_each(
    results: Vec<Result<ContentResponse, Error>>,
    messages: &[(String, bool)],
) -> Vec<Option<PreparedContent>> {
    results
        .into_iter()
        .zip(messages)
        .map(
            |(result, (text, legacy_html))| match (result, legacy_html) {
                (Ok(response @ ContentResponse::Prepared(_)), true) => prepared(response).ok(),
                (Ok(response @ ContentResponse::Rendered(_)), false) => {
                    let document = rendered(response).ok()?;
                    Some(PreparedContent {
                        source: text.clone(),
                        document,
                    })
                }
                _ => None,
            },
        )
        .collect()
}

/// Decode an avatar in the sandboxed content worker; returns an Image document.
pub async fn decode_avatar(executable: &Path, encoded: &[u8]) -> Result<MessageDocument, Error> {
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(encoded);
    rendered(one(executable, ContentRequest::Avatar { image }).await?)
}

/// Decode artwork through the existing content sandbox. This future must run
/// to completion in an owned supervisor task: caller cancellation is not exit.
/// On any error, process capacity remains occupied until actual kill/reaping.
/// The only returned authority is validated RGBA, never worker source/URLs.
pub async fn thumbnail(
    executable: &Path,
    encoded: &[u8],
    max_width: u32,
    max_height: u32,
) -> Result<nuschl_core::ContentBlock, Error> {
    use base64::Engine as _;
    if encoded.len() > crate::MAX_THUMBNAIL_INPUT_BYTES
        || max_width == 0
        || max_height == 0
        || max_width > crate::MAX_THUMBNAIL_EDGE
        || max_height > crate::MAX_THUMBNAIL_EDGE
    {
        return Err(Error::InvalidResponse);
    }
    let request = ContentRequest::Thumbnail {
        image: base64::engine::general_purpose::STANDARD.encode(encoded),
        max_width,
        max_height,
    };
    let mut frame = Vec::new();
    write_frame(&mut frame, &Envelope::new(1, request))?;
    let Worker {
        mut child,
        mut input,
        mut output,
        containment: _containment,
    } = spawn(executable, &[5]).await?;
    let exchange = async {
        let write = async move {
            input.write_all(&frame).await?;
            input.shutdown().await?;
            io::Result::Ok(())
        };
        let read = async {
            let maximum = (max_width as usize * max_height as usize * 4).div_ceil(3) * 4 + 4096;
            let reply = read_thumbnail_reply(&mut output, maximum).await?;
            if reply.request_id != 1 {
                return Err(Error::InvalidResponse);
            }
            Ok(reply.payload)
        };
        let (_, response) = tokio::try_join!(async { write.await.map_err(Error::from) }, read)?;
        Ok::<_, Error>(response)
    };
    let mut reply = timeout(Duration::from_secs(5), exchange)
        .await
        .map_err(|_| Error::Timeout)
        .and_then(|result| result);
    if reply.is_ok() {
        let finish = async {
            let mut extra = [0];
            if output.read(&mut extra).await? != 0 {
                return Err(Error::InvalidResponse);
            }
            if !child.wait().await?.success() {
                return Err(Error::InvalidResponse);
            }
            Ok(())
        };
        reply = match timeout(Duration::from_secs(2), finish).await {
            Ok(Ok(())) => reply,
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Timeout),
        };
    }
    // A failed wait/kill must not release a caller's process credit. Never
    // replace this owner; a supervising HTTP task can time out independently.
    reap(&mut child).await;
    validated_thumbnail(reply?, max_width, max_height)
}

async fn read_thumbnail_reply(
    output: &mut tokio::process::ChildStdout,
    maximum: usize,
) -> Result<Envelope<ContentResponse>, Error> {
    let size = output.read_u32().await? as usize;
    if size == 0 || size > maximum {
        return Err(Error::Frame(crate::Error::TooLarge));
    }
    let mut bytes = vec![0; size];
    output.read_exact(&mut bytes).await?;
    let response: Envelope<ContentResponse> = serde_json::from_slice(&bytes)?;
    if response.version != PROTOCOL_VERSION {
        return Err(Error::InvalidResponse);
    }
    Ok(response)
}

async fn reap(child: &mut Child) {
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        // Unlike start_kill, kill().await waits for the actual process exit.
        if child.kill().await.is_ok() {
            return;
        }
        if child.wait().await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn validated_thumbnail(
    response: ContentResponse,
    max_width: u32,
    max_height: u32,
) -> Result<nuschl_core::ContentBlock, Error> {
    use base64::Engine as _;
    let ContentResponse::Rendered(mut document) = response else {
        return Err(Error::InvalidResponse);
    };
    if document.blocks.len() != 1 {
        return Err(Error::InvalidResponse);
    }
    let Some(nuschl_core::ContentBlock::Image {
        width,
        height,
        rgba,
        source: _,
    }) = document.blocks.pop()
    else {
        return Err(Error::InvalidResponse);
    };
    if width == 0
        || height == 0
        || width > max_width
        || height > max_height
        || width > crate::MAX_THUMBNAIL_EDGE
        || height > crate::MAX_THUMBNAIL_EDGE
    {
        return Err(Error::InvalidResponse);
    }
    let count = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .filter(|n| *n <= crate::MAX_THUMBNAIL_RGBA_BYTES)
        .ok_or(Error::InvalidResponse)?;
    if rgba.len() != count.div_ceil(3) * 4 {
        return Err(Error::InvalidResponse);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&rgba)
        .map_err(|_| Error::InvalidResponse)?;
    if bytes.len() != count {
        return Err(Error::InvalidResponse);
    }
    Ok(nuschl_core::ContentBlock::Image {
        width,
        height,
        rgba,
        source: None,
    })
}

/// Decode a server's website icon in the sandboxed content worker.
pub async fn decode_icon(executable: &Path, encoded: &[u8]) -> Result<MessageDocument, Error> {
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(encoded);
    rendered(one(executable, ContentRequest::Icon { image }).await?)
}

/// Decode an inline image at the viewer's size in the sandboxed content worker.
pub async fn decode_view(executable: &Path, uri: &str) -> Result<MessageDocument, Error> {
    let uri = uri.to_owned();
    rendered(one(executable, ContentRequest::View { uri }).await?)
}

/// Re-encode a pasted image as a Mumble `<img>` element of at most `limit`
/// bytes, with a thumbnail for the composer.
pub async fn encode_image(
    executable: &Path,
    image: &[u8],
    limit: u32,
) -> Result<EncodedImage, Error> {
    use base64::Engine as _;
    let image = base64::engine::general_purpose::STANDARD.encode(image);
    match one(executable, ContentRequest::EncodeImage { image, limit }).await? {
        ContentResponse::Encoded(encoded) => Ok(encoded),
        _ => Err(Error::InvalidResponse),
    }
}

/// Literal text, or (`legacy_html`) Mumble rich text made inert, with its plain source.
pub async fn prepare_content(
    executable: &Path,
    text: &str,
    legacy_html: bool,
) -> Result<PreparedContent, Error> {
    if !legacy_html {
        let document = render_content(executable, text).await?;
        return Ok(PreparedContent {
            source: text.to_owned(),
            document,
        });
    }
    let html = text.to_owned();
    prepared(one(executable, ContentRequest::LegacyHtml { html }).await?)
}

/// Prepared legacy text from the worker, refused unless its document is
/// within its budgets (`within_limits`), as [`rendered`] refuses a rendered
/// one, before anything converts, copies or lays it out.
fn prepared(response: ContentResponse) -> Result<PreparedContent, Error> {
    match response {
        ContentResponse::Prepared(prepared) if prepared.document.within_limits() => Ok(prepared),
        _ => Err(Error::InvalidResponse),
    }
}

/// A worker's document, refused unless within its budgets (line breaks
/// between blocks among them) before anything converts, copies or lays it out.
fn rendered(response: ContentResponse) -> Result<MessageDocument, Error> {
    match response {
        ContentResponse::Rendered(document) if document.within_limits() => Ok(document),
        _ => Err(Error::InvalidResponse),
    }
}

/// A worker's document over its budgets is refused here, before anything
/// converts, copies or lays it out.
#[cfg(test)]
mod rendered_tests {
    use super::{Error, prepared, prepared_each, rendered};
    use crate::{ContentResponse, PreparedContent};
    use nuschl_core::{ContentBlock, MAX_BLOCKS, MAX_CONTENT_BYTES, MessageDocument, TextRole};

    fn reply(blocks: Vec<ContentBlock>) -> Result<MessageDocument, Error> {
        rendered(ContentResponse::Rendered(MessageDocument { blocks }))
    }

    /// Over budget: breaks counting more line breaks than a message holds
    /// together, and 513 blocks of every kind (the one block budget).
    fn malformed() -> [Vec<ContentBlock>; 2] {
        let text = || ContentBlock::text("x");
        let lines = |count| ContentBlock::Break { count };
        let breaks = vec![
            text(),
            lines(MAX_CONTENT_BYTES as u32 / 2),
            text(),
            lines(MAX_CONTENT_BYTES as u32 / 2),
            text(),
            lines(1),
        ];
        let kinds = [
            ContentBlock::Text {
                text: "h".into(),
                spans: Vec::new(),
                role: TextRole::Heading(1),
            },
            ContentBlock::Code {
                language: None,
                runs: Vec::new(),
            },
            ContentBlock::Math {
                source: "$$x$$".into(),
                tex: "x".into(),
            },
            text(),
        ];
        let blocks = (0..MAX_BLOCKS + 1)
            .flat_map(|index| [kinds[index % kinds.len()].clone(), lines(2)])
            .collect();
        [breaks, blocks]
    }

    fn honest() -> Vec<ContentBlock> {
        vec![
            ContentBlock::Break { count: 2 },
            ContentBlock::text("a"),
            ContentBlock::Break { count: 0 },
            ContentBlock::Math {
                source: "$$x$$".into(),
                tex: "x".into(),
            },
        ]
    }

    fn prepared_reply(blocks: Vec<ContentBlock>) -> ContentResponse {
        ContentResponse::Prepared(PreparedContent {
            source: "stored".into(),
            document: MessageDocument { blocks },
        })
    }

    /// One prepared reply (stock HTML) over its budgets is a typed failure.
    #[test]
    fn a_malformed_prepared_reply_is_refused() {
        for blocks in malformed() {
            assert!(matches!(
                prepared(prepared_reply(blocks)),
                Err(Error::InvalidResponse)
            ));
        }
        let honest_reply = prepared(prepared_reply(honest())).unwrap();
        assert_eq!(honest_reply.source, "stored");
        assert_eq!(honest_reply.document.blocks, honest());
    }

    /// In a batch, each malformed reply is refused on its own; the others
    /// pass unchanged.
    #[test]
    fn a_batch_refuses_each_malformed_reply_on_its_own() {
        let [breaks, blocks] = malformed();
        let messages: Vec<(String, bool)> = [
            ("a", true),
            ("b", false),
            ("c", true),
            ("d", false),
            ("e", true),
            ("f", false),
        ]
        .map(|(text, html)| (text.to_owned(), html))
        .into();
        let results = vec![
            Ok(prepared_reply(honest())),
            Ok(ContentResponse::Rendered(MessageDocument {
                blocks: breaks,
            })),
            Ok(prepared_reply(blocks.clone())),
            Ok(ContentResponse::Rendered(MessageDocument {
                blocks: honest(),
            })),
            Err(Error::InvalidResponse),
            Ok(ContentResponse::Rendered(MessageDocument { blocks })),
        ];
        let shown: Vec<Option<(String, Vec<ContentBlock>)>> = prepared_each(results, &messages)
            .into_iter()
            .map(|prepared| prepared.map(|prepared| (prepared.source, prepared.document.blocks)))
            .collect();
        assert_eq!(
            shown,
            [
                Some(("stored".to_owned(), honest())),
                None,
                None,
                Some(("d".to_owned(), honest())),
                None,
                None,
            ]
        );
    }

    #[test]
    fn malformed_documents_from_the_worker_are_refused() {
        let text = || ContentBlock::text("x");
        let lines = |count| ContentBlock::Break { count };
        // One huge gap, u32::MAX, and one just over a whole message.
        for count in [u32::MAX, MAX_CONTENT_BYTES as u32 + 1] {
            assert!(matches!(
                reply(vec![text(), lines(count), text()]),
                Err(Error::InvalidResponse)
            ));
        }
        // Each gap a whole message, 511 of them.
        let mut blocks = vec![text()];
        for _ in 0..511 {
            blocks.extend([lines(MAX_CONTENT_BYTES as u32), text()]);
        }
        assert!(matches!(reply(blocks), Err(Error::InvalidResponse)));
        // Two breaks in a row.
        assert!(matches!(
            reply(vec![text(), lines(2), lines(2), text()]),
            Err(Error::InvalidResponse)
        ));
        // An honest document passes unchanged.
        let honest = vec![lines(2), text(), lines(0), text(), lines(1)];
        assert_eq!(reply(honest.clone()).unwrap().blocks, honest);
    }
}

async fn one(executable: &Path, request: ContentRequest) -> Result<ContentResponse, Error> {
    exchange(executable, std::slice::from_ref(&request))
        .await?
        .pop()
        .unwrap_or(Err(Error::InvalidResponse))
}

/// Answer `requests` in order, all in one sandboxed worker unless one fails.
/// Every request has its own budget: its reply must arrive within that many
/// seconds of the previous one, and on Linux the worker's CPU limit is the sum.
/// A worker that misses a deadline, exits early or answers out of turn is
/// killed and reaped; the request it was working on fails and the rest go to
/// a fresh worker. `Err` means no worker could be started at all.
pub async fn exchange(
    executable: &Path,
    requests: &[ContentRequest],
) -> Result<Vec<Result<ContentResponse, Error>>, Error> {
    let mut frames = Vec::with_capacity(requests.len());
    for (index, request) in requests.iter().enumerate() {
        let mut frame = Vec::new();
        write_frame(&mut frame, &Envelope::new(index as u64 + 1, request))?;
        frames.push(frame);
    }
    let budgets: Vec<u64> = requests.iter().map(budget).collect();
    let mut results = Vec::with_capacity(requests.len());
    while results.len() < requests.len() {
        let first = results.len();
        let outcome = match spawn(executable, &budgets[first..]).await {
            Ok(worker) => converse(worker, &frames[first..], &budgets[first..], &mut results).await,
            Err(error) if first == 0 => return Err(error),
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            results.push(Err(error));
        }
    }
    Ok(results)
}

/// A content worker for requests with these budgets; one that cannot be
/// contained is reaped before the error returns.
async fn spawn(executable: &Path, budgets: &[u64]) -> Result<Worker, Error> {
    let role = Role::Content {
        cpu_seconds: budgets.iter().sum(),
    };
    match nuschl_sandbox::spawn(nuschl_sandbox::command(executable, role, &[])?) {
        Err(nuschl_sandbox::Error::Uncontained { error, mut child }) => {
            reap(&mut child).await;
            Err(error.into())
        }
        result => Ok(result?),
    }
}

/// Send `frames` to one worker and collect its replies into `results`, whose
/// length is the request id of the first frame minus one.
async fn converse(
    worker: Worker,
    frames: &[Vec<u8>],
    budgets: &[u64],
    results: &mut Vec<Result<ContentResponse, Error>>,
) -> Result<(), Error> {
    let Worker {
        mut child,
        mut input,
        mut output,
        containment: _containment,
    } = worker;
    let outcome = {
        // Written alongside reading, so neither pipe can fill up and stall
        // the worker. Closing stdin afterwards tells it to exit.
        let write = async move {
            for frame in frames {
                input.write_all(frame).await?;
            }
            io::Result::Ok(())
        };
        let read = async {
            for budget in budgets {
                let reply = timeout(Duration::from_secs(*budget), read_reply(&mut output))
                    .await
                    .map_err(|_| Error::Timeout)??;
                if reply.request_id != results.len() as u64 + 1 {
                    return Err(Error::InvalidResponse);
                }
                results.push(match reply.payload {
                    ContentResponse::Rejected(reason) => Err(Error::Rejected(reason)),
                    response => Ok(response),
                });
            }
            // A worker that does not then exit cleanly: its last reply is not trusted.
            match timeout(Duration::from_secs(2), child.wait()).await {
                Ok(Ok(status)) if status.success() => Ok(()),
                exited => {
                    results.pop();
                    Err(match exited {
                        Err(_) => Error::Timeout,
                        Ok(Err(error)) => error.into(),
                        Ok(Ok(_)) => Error::InvalidResponse,
                    })
                }
            }
        };
        tokio::pin!(read);
        tokio::select! {
            outcome = &mut read => outcome,
            // A worker that died early fails the read at the request it was on.
            _ = write => read.await,
        }
    };
    if outcome.is_err() {
        // kill_on_drop is a fallback; explicitly reap while the runtime still exists.
        child.kill().await.ok();
    }
    outcome
}

async fn read_reply(
    output: &mut tokio::process::ChildStdout,
) -> Result<Envelope<ContentResponse>, Error> {
    let size = output.read_u32().await? as usize;
    if size > MAX_FRAME_BYTES {
        return Err(Error::Frame(crate::Error::TooLarge));
    }
    let mut bytes = vec![0; size];
    output.read_exact(&mut bytes).await?;
    let reply: Envelope<ContentResponse> = serde_json::from_slice(&bytes)?;
    if reply.version != PROTOCOL_VERSION {
        return Err(Error::InvalidResponse);
    }
    Ok(reply)
}
#[cfg(test)]
mod thumbnail_validation_tests {
    use super::*;
    use nuschl_core::ContentBlock;
    fn image(width: u32, height: u32, rgba: &str) -> ContentBlock {
        ContentBlock::Image {
            width,
            height,
            rgba: rgba.to_owned(),
            source: Some("https://never-fetch.invalid/private".into()),
        }
    }
    fn reply(blocks: Vec<ContentBlock>) -> ContentResponse {
        ContentResponse::Rendered(MessageDocument { blocks })
    }
    #[test]
    fn thumbnail_reply_has_one_bounded_image_and_no_source_authority() {
        let output = validated_thumbnail(reply(vec![image(1, 1, "AQIDBA==")]), 300, 300).unwrap();
        assert!(matches!(
            output,
            ContentBlock::Image {
                source: None,
                width: 1,
                height: 1,
                ..
            }
        ));
        for blocks in [
            vec![],
            vec![image(1, 1, "AQIDBA=="), image(1, 1, "AQIDBA==")],
            vec![image(0, 1, "")],
            vec![image(401, 1, "AQIDBA==")],
            vec![image(1, 1, "AQID")],
            vec![image(1, 1, "AQIDBB==")],
            vec![image(1, 1, "https://not-pixels.invalid")],
        ] {
            assert!(validated_thumbnail(reply(blocks), 400, 400).is_err());
        }
        assert!(validated_thumbnail(reply(vec![image(2, 1, "AQIDBAUGBwg=")]), 1, 1).is_err());
        assert!(validated_thumbnail(ContentResponse::Rejected("no".into()), 1, 1).is_err());
    }
}
