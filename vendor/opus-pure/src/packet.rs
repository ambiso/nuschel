//! Reading an Opus packet's shape without decoding it.
//!
//! Every Opus packet starts with a TOC byte describing its mode, bandwidth,
//! frame duration and channel count, followed by a frame-packing code
//! (RFC 6716 §3.1). That is enough to answer the questions a muxer or a
//! remuxer needs to ask before handing the packet to a decoder.
//!
//! [`samples_48k`] is the unit Ogg counts in — granule positions are at 48 kHz
//! whatever rate the encoder ran at — and is what
//! [`OggOpusWriter::write_packet`] reads out of a packet for you.
//!
//! ```
//! use opus_pure::packet;
//!
//! # fn f(pkt: &[u8]) -> opus_pure::Result<()> {
//! // How long is this packet, without decoding it?
//! let samples = packet::samples_48k(pkt)?;
//! let millis = samples as f32 / 48.0;
//! # let _ = millis;
//! # Ok(())
//! # }
//! ```
//!
//! [`OggOpusWriter::write_packet`]: crate::OggOpusWriter::write_packet

use crate::repacketizer::samples_per_frame as toc_samples_per_frame;
use crate::toc::{bandwidth_from_toc, channels_from_toc, mode_from_toc};
use crate::{Bandwidth, Error, OpusMode, Result};

/// The most samples per channel any Opus packet can decode to, at any rate.
///
/// A packet carries at most 120 ms (RFC 6716 §3.2), which is 5760 samples at
/// 48 kHz and fewer at every lower rate — so a buffer of this many samples per
/// channel holds the output of any packet a decoder is handed, whatever rate it
/// was created with and whatever frame size the stream turns out to use.
///
/// This is the decode-side companion to
/// [`MAX_PACKET_BYTES`](crate::MAX_PACKET_BYTES), which sizes the buffer on the
/// way in. For the exact length of a packet you already hold — rather than the
/// bound over all of them — use [`samples`].
///
/// ```
/// use opus_pure::{MAX_PACKET_SAMPLES, OpusDecoder};
///
/// let channels = 2;
/// let mut decoder = OpusDecoder::new(48_000, channels)?;
/// // Sized once, correct for every packet in any stream.
/// let mut pcm = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
/// # let _ = (&mut decoder, &mut pcm);
/// # Ok::<(), opus_pure::Error>(())
/// ```
pub const MAX_PACKET_SAMPLES: usize = 5760;

/// [`MAX_PACKET_SAMPLES`] at a rate below 48 kHz, for the bound in [`samples`].
/// Every Opus rate divides 48 000, so this is exact. The caller has already
/// checked the rate.
fn max_samples(sample_rate: i32) -> usize {
    MAX_PACKET_SAMPLES * sample_rate as usize / 48_000
}

/// Validate the complete framing of one ordinary Opus packet.
///
/// Checks frame counts, lengths, padding and the format's 120 ms limit using
/// the same parser as the decoder. This accepts valid zero/one-byte DTX frames
/// and all Opus durations, including 2.5 and 5 ms. It does not decode or validate
/// the compressed audio's entropy content, allocate, or change decoder state.
///
/// Call this before admitting untrusted packets to a jitter buffer or advancing
/// its clock. The TOC-only helpers such as [`samples`] and [`frame_count`] do
/// not validate the complete framing.
pub fn validate(packet: &[u8]) -> Result<()> {
    crate::repacketizer::parse_packet(packet, false).map(|_| ())
}

/// Reject a rate Opus cannot decode at, so a buffer is never sized from one.
///
/// The set lives here rather than in each caller so that "which rates does Opus
/// support" is answered in one place and cannot drift.
pub(crate) fn check_rate(sample_rate: i32) -> Result<()> {
    if ![8_000, 12_000, 16_000, 24_000, 48_000].contains(&sample_rate) {
        return Err(Error::InvalidArgument(
            "sample rate must be 8, 12, 16, 24 or 48 kHz",
        ));
    }
    Ok(())
}

/// How many frames the packet holds (RFC 6716 §3.2, frame-packing codes 0-3).
pub fn frame_count(packet: &[u8]) -> Result<usize> {
    let toc = *packet
        .first()
        .ok_or(Error::InvalidPacket("packet is empty"))?;
    match toc & 0x03 {
        0 => Ok(1),
        1 | 2 => Ok(2),
        _ => {
            let n = *packet
                .get(1)
                .ok_or(Error::InvalidPacket("code 3 packet has no frame count"))?
                & 0x3F;
            if n == 0 {
                return Err(Error::InvalidPacket("code 3 packet declares zero frames"));
            }
            Ok(n as usize)
        }
    }
}

/// Samples per channel this packet decodes to at `sample_rate`.
///
/// Mirrors `opus_packet_get_nb_samples`. `sample_rate` must be one Opus
/// supports (8/12/16/24/48 kHz); the answer is per channel, so a stereo packet
/// still reports the number of sample *frames*.
pub fn samples(packet: &[u8], sample_rate: i32) -> Result<usize> {
    check_rate(sample_rate)?;
    let toc = *packet
        .first()
        .ok_or(Error::InvalidPacket("packet is empty"))?;
    let n = frame_count(packet)? * toc_samples_per_frame(toc, sample_rate) as usize;
    // A packet cannot describe more than 120 ms, so a frame count that implies
    // more is a malformed packet rather than a very long one.
    if n > max_samples(sample_rate) {
        return Err(Error::InvalidPacket("packet claims more than 120 ms"));
    }
    Ok(n)
}

/// Samples per channel in one *frame* of this packet at `sample_rate`.
///
/// A packet holds one to forty-eight frames, all of the same duration, so this
/// times [`frame_count`] is [`samples`]. Mirrors
/// `opus_packet_get_samples_per_frame`; `sample_rate` must be one Opus supports.
pub fn samples_per_frame(packet: &[u8], sample_rate: i32) -> Result<usize> {
    check_rate(sample_rate)?;
    let toc = *packet
        .first()
        .ok_or(Error::InvalidPacket("packet is empty"))?;
    Ok(toc_samples_per_frame(toc, sample_rate) as usize)
}

/// Samples per channel at 48 kHz — the unit Ogg granule positions use, and so
/// the value [`crate::OggOpusWriter::write_packet`] expects.
pub fn samples_48k(packet: &[u8]) -> Result<usize> {
    samples(packet, 48_000)
}

/// Which coding layers the packet used (RFC 6716 §3.1).
///
/// The reason to ask is usually in-band FEC: a redundant copy of the previous
/// frame only exists in SILK and hybrid packets, so
/// [`OpusDecoder::decode_fec`](crate::OpusDecoder::decode_fec) on an
/// [`OpusMode::CeltOnly`] packet can only conceal. A caller recovering from
/// loss can check first and know which of the two it is about to get.
///
/// ```
/// # use opus_pure::{packet, OpusMode};
/// # fn f(pkt: &[u8]) -> opus_pure::Result<()> {
/// if packet::mode(pkt)? != OpusMode::CeltOnly {
///     // worth calling decode_fec: this packet can carry the previous frame
/// }
/// # Ok(())
/// # }
/// ```
pub fn mode(packet: &[u8]) -> Result<OpusMode> {
    let toc = *packet
        .first()
        .ok_or(Error::InvalidPacket("packet is empty"))?;
    Ok(mode_from_toc(toc))
}

/// The audio bandwidth the packet carries (`opus_packet_get_bandwidth`).
///
/// This is what the encoder chose to spend its bits on, which is not the same
/// question as what rate a decoder will produce: a narrowband packet decoded at
/// 48 kHz still comes back at 48 kHz, with nothing above 4 kHz in it.
pub fn bandwidth(packet: &[u8]) -> Result<Bandwidth> {
    let toc = *packet
        .first()
        .ok_or(Error::InvalidPacket("packet is empty"))?;
    Ok(bandwidth_from_toc(toc))
}

/// Channels the packet carries: 1 or 2. A stream may alternate between them.
pub fn channels(packet: &[u8]) -> Result<usize> {
    let toc = *packet
        .first()
        .ok_or(Error::InvalidPacket("packet is empty"))?;
    Ok(channels_from_toc(toc))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a TOC for a CELT config with the given frame-packing code.
    fn toc(config: u8, stereo: bool, code: u8) -> u8 {
        (config << 3) | ((stereo as u8) << 2) | code
    }

    #[test]
    fn validate_accepts_dtx_padding_and_every_frame_packing_code() {
        let packets: &[&[u8]] = &[
            &[0x48],                      // code 0, zero-byte DTX
            &[0x48, 0],                   // code 0, one-byte DTX
            &[0x49],                      // code 1, two empty frames
            &[0x49, 0, 0],                // code 1, two one-byte frames
            &[0x4a, 0, 0, 0],             // code 2, empty then two-byte frame
            &[0x4a, 2, 0, 0],             // code 2, two-byte then empty frame
            &[0x4b, 0x41, 2, 0, 0, 0],    // code 3, one byte + two padding
            &[0x4b, 0x83, 0, 1, 0, 0, 0], // code 3 VBR, lengths 0/1/2
            &[0x4b, 6],                   // six empty 20 ms frames = 120 ms
            &[0x83, 48],                  // 48 empty 2.5 ms frames = 120 ms
            &[0x88],                      // legal 5 ms frame
        ];
        for packet in packets {
            assert!(validate(packet).is_ok(), "{packet:02x?}");
        }
        // The original ordered-stream failure: one coded byte, 56 padding.
        let mut padded_dtx = vec![0x4b, 0x41, 56];
        padded_dtx.resize(60, 0);
        assert!(validate(&padded_dtx).is_ok());
    }

    #[test]
    fn validate_rejects_incomplete_or_inconsistent_framing() {
        let packets: &[&[u8]] = &[
            &[],
            &[0x09, 0],          // odd-sized two-frame CBR payload
            &[0x99, 0],          // same invalid framing in CELT
            &[0x4a],             // missing VBR length
            &[0x4a, 252],        // truncated two-byte length field
            &[0x4a, 2, 0],       // first VBR frame exceeds payload
            &[0x4b],             // missing frame count
            &[0x4b, 0],          // zero frames
            &[0x4b, 7],          // more than 120 ms
            &[0x4b, 0x41],       // missing padding length
            &[0x4b, 0x41, 255],  // truncated continued padding
            &[0x4b, 0x41, 2, 0], // padding exceeds packet
            &[0x4b, 2, 0],       // code 3 CBR length not divisible by count
            &[0x4b, 0x82, 2, 0], // code 3 VBR frame exceeds payload
        ];
        for packet in packets {
            assert!(
                matches!(validate(packet), Err(Error::InvalidPacket(_))),
                "{packet:02x?}"
            );
        }
        let mut too_large = vec![0x48];
        too_large.resize(1277, 0); // a coded frame may not exceed 1275 bytes
        assert!(validate(&too_large).is_err());
    }

    #[test]
    fn duration_helpers_do_not_replace_complete_validation() {
        let malformed = [0x09, 0];
        assert_eq!(samples_48k(&malformed).unwrap(), 1920);
        assert_eq!(frame_count(&malformed).unwrap(), 2);
        assert!(validate(&malformed).is_err());
        assert_eq!(malformed, [0x09, 0]);
    }

    #[test]
    fn durations_match_the_toc() {
        // CELT configs 16..=19 are 2.5/5/10/20 ms.
        for (config, ms10) in [(16u8, 25i32), (17, 50), (18, 100), (19, 200)] {
            let p = [toc(config, false, 0)];
            assert_eq!(
                samples_48k(&p).unwrap(),
                (48_000 * ms10 / 10_000) as usize,
                "config {config}"
            );
        }
        // SILK configs 0..=3 are 10/20/40/60 ms.
        for (config, ms) in [(0u8, 10i32), (1, 20), (2, 40), (3, 60)] {
            let p = [toc(config, false, 0)];
            assert_eq!(
                samples_48k(&p).unwrap(),
                (48 * ms) as usize,
                "config {config}"
            );
        }
    }

    #[test]
    fn frame_packing_codes_multiply_the_duration() {
        let one = [toc(19, false, 0)]; // 20 ms, single frame
        assert_eq!(samples_48k(&one).unwrap(), 960);
        for code in [1u8, 2] {
            let two = [toc(19, false, code)];
            assert_eq!(samples_48k(&two).unwrap(), 1920, "code {code}");
        }
        let three = [toc(19, false, 3), 3];
        assert_eq!(samples_48k(&three).unwrap(), 2880);
    }

    #[test]
    fn rates_scale_the_answer() {
        let p = [toc(19, false, 0)]; // 20 ms
        for (rate, expect) in [
            (8_000, 160),
            (12_000, 240),
            (16_000, 320),
            (24_000, 480),
            (48_000, 960),
        ] {
            assert_eq!(samples(&p, rate).unwrap(), expect, "{rate} Hz");
        }
        assert!(samples(&p, 44_100).is_err(), "44.1 kHz is not an Opus rate");
    }

    #[test]
    fn channels_come_from_the_toc() {
        assert_eq!(channels(&[toc(19, false, 0)]).unwrap(), 1);
        assert_eq!(channels(&[toc(19, true, 0)]).unwrap(), 2);
    }

    /// Malformed packets must be reported, never turned into a plausible
    /// duration a muxer would then write into a granule position.
    #[test]
    fn malformed_packets_are_rejected() {
        assert!(samples_48k(&[]).is_err(), "empty packet");
        assert!(frame_count(&[]).is_err(), "empty packet");
        // Code 3 without its frame-count byte.
        assert!(samples_48k(&[toc(19, false, 3)]).is_err());
        // Code 3 declaring zero frames.
        assert!(samples_48k(&[toc(19, false, 3), 0]).is_err());
        // 48 frames of 20 ms is 960 ms, far past the 120 ms limit.
        assert!(samples_48k(&[toc(19, false, 3), 48]).is_err());
        // 6 frames of 20 ms is 120 ms exactly, which is legal.
        assert_eq!(samples_48k(&[toc(19, false, 3), 6]).unwrap(), 5760);
    }
}
