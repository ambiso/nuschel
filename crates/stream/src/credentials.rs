// SPDX-License-Identifier: EUPL-1.2
//! Identity bindings in key requests (docs/screen-share-plan.md §3, "Message sizes and
//! credentials"). The binding is signed and verified by the identity layer; this crate
//! carries it, names the credentials it depends on by SHA-256 fingerprint, sends them
//! inline on a cache miss, and checks that inline bytes match a named fingerprint.

use std::collections::VecDeque;

use crate::wire::{Reader, put_opaque16};
use crate::{Error, sha256};

/// SHA-256 of a credential's exact bytes.
pub type Fingerprint = [u8; 32];

pub fn fingerprint(credential: &[u8]) -> Fingerprint {
    sha256(&[credential])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// Opaque here; signed and verified by the identity layer.
    pub call_binding: Vec<u8>,
    /// The credentials the binding depends on, in the order it names them.
    pub fingerprints: Vec<Fingerprint>,
    /// Some of those credentials in full, for a receiver that lacks them.
    pub inline: Vec<Vec<u8>>,
}

impl Binding {
    pub const MAX_CREDENTIALS: usize = 4;

    pub(crate) fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        if self.call_binding.is_empty()
            || self.fingerprints.len() > Self::MAX_CREDENTIALS
            || self.inline.len() > self.fingerprints.len()
            || self.inline.iter().any(Vec::is_empty)
        {
            return Err(Error::Malformed("binding"));
        }
        put_opaque16(out, &self.call_binding)?;
        out.push(u8::try_from(self.fingerprints.len()).expect("at most 4"));
        out.extend(self.fingerprints.iter().flatten());
        out.push(u8::try_from(self.inline.len()).expect("at most 4"));
        for credential in &self.inline {
            put_opaque16(out, credential)?;
        }
        Ok(())
    }

    pub(crate) fn decode(reader: &mut Reader<'_>) -> Result<Self, Error> {
        let call_binding = reader.opaque16(1)?.to_vec();
        let count = usize::from(reader.u8()?);
        if count > Self::MAX_CREDENTIALS {
            return Err(Error::Malformed("binding credentials"));
        }
        let fingerprints = (0..count)
            .map(|_| reader.array())
            .collect::<Result<Vec<Fingerprint>, _>>()?;
        let inline_count = usize::from(reader.u8()?);
        if inline_count > count {
            return Err(Error::Malformed("binding credentials"));
        }
        let inline = (0..inline_count)
            .map(|_| reader.opaque16(1).map(<[u8]>::to_vec))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            call_binding,
            fingerprints,
            inline,
        })
    }
}

/// A binding with every credential present, in fingerprint order, ready for the identity
/// layer to verify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiableBinding {
    /// What `call_binding` must sign: this request's [`crate::JoinDraft::binding_digest`],
    /// computed by the sharer from the request it received.
    pub digest: [u8; 32],
    pub call_binding: Vec<u8>,
    pub credentials: Vec<Vec<u8>>,
}

/// Credentials seen before, by fingerprint; bounded by entries and bytes, oldest evicted
/// first. Holds only bytes that matched their fingerprint; revocation and key
/// transparency are still the caller's checks.
#[derive(Debug)]
pub struct CredentialCache {
    entries: VecDeque<(Fingerprint, Vec<u8>)>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl CredentialCache {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    pub fn get(&self, fingerprint: &Fingerprint) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(known, _)| known == fingerprint)
            .map(|(_, bytes)| &bytes[..])
    }

    /// Stores a credential as the newest entry. One larger than the whole cache is not
    /// kept.
    pub fn insert(&mut self, credential: Vec<u8>) -> Fingerprint {
        let fingerprint = fingerprint(&credential);
        if let Some(index) = self
            .entries
            .iter()
            .position(|(known, _)| *known == fingerprint)
        {
            let entry = self.entries.remove(index).expect("index found above");
            self.entries.push_back(entry);
            return fingerprint;
        }
        if self.max_entries == 0 || credential.len() > self.max_bytes {
            return fingerprint;
        }
        self.bytes += credential.len();
        self.entries.push_back((fingerprint, credential));
        while self.entries.len() > self.max_entries || self.bytes > self.max_bytes {
            let (_, evicted) = self
                .entries
                .pop_front()
                .expect("over a limit, so not empty");
            self.bytes -= evicted.len();
        }
        fingerprint
    }

    /// Every credential the binding names, or the fingerprints still missing. Inline
    /// credentials must match a named fingerprint, and are cached.
    pub(crate) fn resolve(
        &mut self,
        binding: &Binding,
        digest: [u8; 32],
    ) -> Result<Result<VerifiableBinding, Vec<Fingerprint>>, Error> {
        let inline: Vec<(Fingerprint, &Vec<u8>)> = binding
            .inline
            .iter()
            .map(|credential| (fingerprint(credential), credential))
            .collect();
        for (index, named) in binding.fingerprints.iter().enumerate() {
            if binding.fingerprints[..index].contains(named) {
                return Err(Error::Malformed("duplicate credential"));
            }
        }
        if inline
            .iter()
            .any(|(fingerprint, _)| !binding.fingerprints.contains(fingerprint))
        {
            return Err(Error::Malformed("credential matches no fingerprint"));
        }
        let mut credentials = Vec::with_capacity(binding.fingerprints.len());
        let mut missing = Vec::new();
        for named in &binding.fingerprints {
            match inline.iter().find(|(fingerprint, _)| fingerprint == named) {
                Some((_, credential)) => credentials.push((*credential).clone()),
                None => match self.get(named) {
                    Some(credential) => credentials.push(credential.to_vec()),
                    None => missing.push(*named),
                },
            }
        }
        for credential in &binding.inline {
            self.insert(credential.clone());
        }
        Ok(if missing.is_empty() {
            Ok(VerifiableBinding {
                digest,
                call_binding: binding.call_binding.clone(),
                credentials,
            })
        } else {
            Err(missing)
        })
    }
}
