// SPDX-License-Identifier: EUPL-1.2
use crate::Error;
use ed25519_dalek::Signer as _;
use ml_dsa::Keypair as _;
use nuschl_update::auth_format::{ML_CONTEXT, PublicKey, Signature, signed_bytes};
use zeroize::Zeroizing;

fn keys(
    ed: &[u8; 32],
    ml: &[u8; 32],
) -> (
    ed25519_dalek::SigningKey,
    ml_dsa::SigningKey<ml_dsa::MlDsa44>,
) {
    let seed = Zeroizing::new(ml_dsa::Seed::from(*ml));
    (
        ed25519_dalek::SigningKey::from_bytes(ed),
        ml_dsa::SigningKey::from_seed(&seed),
    )
}
pub fn public_key(ed: &[u8; 32], ml: &[u8; 32]) -> Result<PublicKey, Error> {
    let (ed, ml) = keys(ed, ml);
    let mut encoded = Vec::with_capacity(1344);
    encoded.extend_from_slice(ed.verifying_key().as_bytes());
    encoded.extend_from_slice(&ml.verifying_key().encode());
    Ok(PublicKey::from_bytes(&encoded)?)
}

#[cfg(feature = "signer")]
pub fn sign(
    ed: &[u8; 32],
    ml: &[u8; 32],
    manifest: &[u8],
) -> Result<(PublicKey, Signature), Error> {
    let public = public_key(ed, ml)?;
    let message = signed_bytes(&public, manifest)?;
    let (ed, ml) = keys(ed, ml);
    let ed_signature = ed.sign(&message);
    let ml_signature = ml
        .expanded_key()
        .sign_randomized(&message, ML_CONTEXT, &mut getrandom::SysRng)
        .map_err(|_| Error::Randomness)?;
    let signature =
        Signature::from_components(&public, &ed_signature.to_bytes(), &ml_signature.encode())?;
    Ok((public, signature))
}

#[cfg(test)]
pub(crate) fn deterministic_fixture(
    ed: &[u8; 32],
    ml: &[u8; 32],
    manifest: &[u8],
) -> Result<(PublicKey, Signature), Error> {
    let public = public_key(ed, ml)?;
    let message = signed_bytes(&public, manifest)?;
    let (ed, ml) = keys(ed, ml);
    let ed_signature = ed.sign(&message);
    let ml_signature = ml
        .expanded_key()
        .sign_deterministic(&message, ML_CONTEXT)
        .map_err(|_| Error::Signature)?;
    Ok((
        public.clone(),
        Signature::from_components(&public, &ed_signature.to_bytes(), &ml_signature.encode())?,
    ))
}
