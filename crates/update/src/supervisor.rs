// SPDX-License-Identifier: EUPL-1.2
//! The parent has no asymmetric verifier: acceptance is the AND of two fixed,
//! independently confined processes. A compromised worker can forge its own
//! success reply, so the other algorithm must still hold. Shared OS/compiler/
//! hashing/parent defects are outside that independence claim.
use crate::{
    Error, Manifest,
    auth_format::{PublicKey, Signature, signed_bytes},
    manifest,
    verifier_wire::{Algorithm, REPLY_BYTES, Request},
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Debug)]
pub struct VerifierPaths {
    pub ed25519: PathBuf,
    pub mldsa44: PathBuf,
}
impl VerifierPaths {
    pub fn discover() -> Result<Self, Error> {
        Ok(Self {
            ed25519: nuschl_sandbox::sibling(Algorithm::Ed25519.executable())?,
            mldsa44: nuschl_sandbox::sibling(Algorithm::MlDsa44.executable())?,
        })
    }
}

/// Explicit keys and paths support private fixture callers. Production `check`
/// supplies only embedded trust and fixed adjacent worker names; no downloaded
/// manifest or writable trust document can select either.
pub fn verify_hybrid(
    raw: &[u8],
    signature: &[u8],
    keys: &[PublicKey],
    workers: &VerifierPaths,
    now: u64,
) -> Result<Manifest, Error> {
    if keys.is_empty() {
        return Err(Error::Unenrolled);
    }
    let signature = Signature::from_bytes(signature).map_err(|_| Error::Signature)?;
    let key = keys
        .iter()
        .find(|key| signature.check_key(key).is_ok())
        .ok_or(Error::Signature)?;
    let message = signed_bytes(key, raw).map_err(|_| Error::Signature)?;
    let ed = Request::new(
        Algorithm::Ed25519,
        key.ed25519(),
        signature.ed25519(),
        &message,
    )?;
    let ml = Request::new(
        Algorithm::MlDsa44,
        key.mldsa44(),
        signature.mldsa44(),
        &message,
    )?;
    let run = move || -> Result<(), Error> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            // Independently reap both children, including when one rejects.
            let ed_result = one(&workers.ed25519, false, ed).await;
            let ml_result = one(&workers.mldsa44, true, ml).await;
            ed_result?;
            ml_result
        })
    };
    // Desktop invokes this blocking API from Tokio spawn_blocking, which still
    // carries a current Handle. A scoped dedicated thread avoids nesting a
    // Runtime::block_on (or dropping its runtime) in any entered async context.
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("nuschl-update-verify".into())
                .spawn_scoped(scope, run)?
                .join()
                .map_err(|_| Error::Verifier)?
        })?;
    } else {
        run()?;
    }
    manifest::parse(raw, now)
}

async fn one(executable: &Path, post_quantum: bool, request: Request) -> Result<(), Error> {
    let role = nuschl_sandbox::Role::Update { post_quantum };
    let command = nuschl_sandbox::command(executable, role).map_err(|_| Error::Verifier)?;
    let nuschl_sandbox::Worker {
        mut child,
        mut input,
        output,
        containment: _containment,
    } = nuschl_sandbox::spawn(command)
        .await
        .map_err(|_| Error::Verifier)?;
    let expected = request.expected_reply();
    let bytes = request.encode();
    let result = tokio::time::timeout(Duration::from_secs(6), async {
        input.write_all(&bytes).await.map_err(|_| Error::Verifier)?;
        input.shutdown().await.map_err(|_| Error::Verifier)?;
        drop(input);
        let mut reply = Vec::new();
        output
            .take((REPLY_BYTES + 1) as u64)
            .read_to_end(&mut reply)
            .await
            .map_err(|_| Error::Verifier)?;
        if reply != expected {
            return Err(Error::Verifier);
        }
        let status = child.wait().await.map_err(|_| Error::Verifier)?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Verifier)
        }
    })
    .await;
    let result = match result {
        Ok(result) => result,
        Err(_) => Err(Error::Verifier),
    };
    if result.is_err() {
        nuschl_sandbox::reap(&mut child).await;
    }
    result
}
