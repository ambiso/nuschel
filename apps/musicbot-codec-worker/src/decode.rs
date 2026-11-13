// SPDX-License-Identifier: EUPL-1.2
//! Credential-free song decoder, called only inside the isolated codec role.
//! The parent provides opaque media bytes; no URL, path or library reaches it.
//! Symphonia demuxes and decodes (Opus goes to opus-pure), rubato resamples.
use audioadapter_buffers::direct::InterleavedSlice;
use opus_pure::OpusDecoder;
use rubato::{Fft, FixedSync, Indexing, Resampler};
#[cfg(test)]
use std::sync::Arc;
use std::{
    io::{self, Cursor, Read, Seek, SeekFrom},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use symphonia::core::{
    codecs::audio::{AudioCodecParameters, AudioDecoderOptions, well_known::*},
    common::Limit,
    errors::Error as SymphoniaError,
    formats::{FormatOptions, FormatReader, TrackType, probe::Hint},
    io::{
        BitReaderLtr, BitReaderRtl, MediaSource, MediaSourceStream, ReadBitsLtr, ReadBitsRtl,
        ReadOnlySource,
    },
    meta::MetadataOptions,
    packet::Packet,
};

pub use nuschl_ipc::musicbot::{CHANNELS, FRAME, RATE};
use nuschl_ipc::musicbot::{DecodeLimits, INPUT_PROGRESS_BYTES, needs_seeking};
/// MP4 needs a seekable source, so it is downloaded first, up to this size.
const MAX_DOWNLOAD_BYTES: u64 = DecodeLimits::DEFAULT.mp4_bytes;
/// The longest Opus packet, 120 ms, per channel.
const MAX_OPUS_FRAME: usize = 5760;
/// Resource admission, not a limit on the length of a song.
const MIN_INPUT_RATE: u32 = 8_000;
const MAX_INPUT_RATE: u32 = 384_000;
const MAX_INPUT_CHANNELS: usize = 8;
/// Actual real-FFT lengths (rubato plans twice its input/output block sizes).
const MAX_FFT_SAMPLES: usize = 16_384;
const MAX_DECODED_FRAMES: usize = 65_536;
const MAX_DECODED_SAMPLES: usize = MAX_DECODED_FRAMES * MAX_INPUT_CHANNELS;
/// Renewed for probing and EACH packet, never for individual reads in that operation.
const MAX_DEMUX_BYTES: usize = 4 * 1024 * 1024;
const RESOURCE_ERROR: &str =
    "the file's metadata or audio packet exceeds the 4 MiB decoder read budget";
const CANCELLED: &str = "decoding cancelled";

/// Decode opaque bytes, preserving the original codec/resource admission rules.
/// Cancellation closes/kills this role through the parent supervisor; this module
/// never opens a source, selects an executable or receives credentials.
/// `read` hears of every whole `INPUT_PROGRESS_BYTES` of an MP4 file read
/// before decoding; it and `emit` stop decoding by returning false.
pub fn decode_stream(
    input: impl Read + Send + 'static,
    mime: Option<&str>,
    extension: Option<&str>,
    read: &mut dyn FnMut(u64) -> bool,
    emit: &mut dyn FnMut(Vec<f32>) -> bool,
) -> Result<(), String> {
    let cancelled = AtomicBool::new(false);
    let mut hint = Hint::new();
    if let Some(extension) = extension {
        hint.with_extension(extension);
    }
    if let Some(mime) = mime {
        hint.mime_type(mime);
    }
    let source: Box<dyn MediaSource> = if needs_seeking(mime, extension) {
        Box::new(Cursor::new(download(input, &cancelled, read)?))
    } else {
        Box::new(ReadOnlySource::new(Unshared(Mutex::new(input))))
    };
    decode_controlled(source, &hint, &cancelled, emit)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

/// Read in bounded chunks so cancellation works before the first decoded frame.
fn download(
    mut reader: impl Read,
    cancelled: &AtomicBool,
    read: &mut dyn FnMut(u64) -> bool,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 32 * 1024];
    let mut reported = 0u64;
    loop {
        check_cancelled(cancelled)?;
        let count = match reader.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("the music library audio download failed".into()),
        };
        check_cancelled(cancelled)?;
        if count == 0 {
            return Ok(bytes);
        }
        let needed = bytes.len() + count;
        if needed as u64 > MAX_DOWNLOAD_BYTES {
            return Err("the file is larger than 512 MiB".into());
        }
        if needed > bytes.capacity() {
            let capacity = (bytes.capacity().max(buffer.len()) * 2)
                .max(needed)
                .min(MAX_DOWNLOAD_BYTES as usize);
            bytes
                .try_reserve_exact(capacity - bytes.len())
                .map_err(|_| "not enough memory to download the audio".to_owned())?;
        }
        bytes.extend_from_slice(&buffer[..count]);
        while bytes.len() as u64 >= reported + INPUT_PROGRESS_BYTES {
            reported += INPUT_PROGRESS_BYTES;
            if !read(reported) {
                return Err(CANCELLED.into());
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn decode_source(
    source: Box<dyn MediaSource>,
    hint: &Hint,
    emit: &mut dyn FnMut(Vec<f32>) -> bool,
) -> Result<(), String> {
    decode_controlled(source, hint, &AtomicBool::new(false), emit)
}

fn decode_controlled(
    source: Box<dyn MediaSource>,
    hint: &Hint,
    cancelled: &AtomicBool,
    emit: &mut dyn FnMut(Vec<f32>) -> bool,
) -> Result<(), String> {
    let budget = ReadBudget::new(cancelled);
    budget.begin()?;
    let source = Box::new(BudgetSource {
        inner: source,
        budget: &budget,
    });
    let stream = MediaSourceStream::new(source, Default::default());
    let result = symphonia::default::get_probe().probe(
        hint,
        stream,
        FormatOptions::default(),
        MetadataOptions::default()
            .limit_tag_bytes(Limit::Maximum(MAX_DEMUX_BYTES))
            .limit_visual_bytes(Limit::Maximum(MAX_DEMUX_BYTES)),
    );
    budget.check()?;
    let mut format = result.map_err(|_| "the audio format is invalid or unsupported".to_owned())?;
    format.metadata().skip_to_latest();
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("the file has no audio")?;
    let id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .ok_or("the file has no audio")?;
    // Admit declared requirements before cloning extra data or constructing a decoder.
    admit_params(params)?;
    match params.codec {
        CODEC_ID_VORBIS => admit_vorbis(
            params
                .extra_data
                .as_deref()
                .ok_or("the Vorbis headers are missing")?,
            cancelled,
        )?,
        CODEC_ID_ALAC => admit_alac(
            params
                .extra_data
                .as_deref()
                .ok_or("the ALAC headers are missing")?,
        )?,
        CODEC_ID_AAC => {
            if let Some(data) = params.extra_data.as_deref() {
                admit_aac(data)?;
            }
        }
        _ => (),
    }
    if params.codec == CODEC_ID_OPUS {
        let channels = params
            .channels
            .as_ref()
            .map_or(2, |channels| channels.count());
        if channels > CHANNELS {
            return Err("Opus audio with more than two channels is not supported".into());
        }
        return decode_opus(format.as_mut(), id, channels, &budget, emit);
    }
    check_cancelled(cancelled)?;
    let pcm_frame_bytes = pcm_sample_bytes(params.codec).and_then(|bytes| {
        params
            .channels
            .as_ref()
            .map(|channels| bytes * channels.count())
    });
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|_| "the audio codec is invalid or unsupported".to_owned())?;
    // ALAC/AAC can resolve rate/channel declarations from their codec headers.
    admit_params(decoder.codec_params())?;
    let mut framer: Option<Framer> = None;
    let mut samples = Vec::new();
    while let Some(packet) = next_packet(format.as_mut(), &budget)? {
        if packet.track_id != id {
            continue;
        }
        if pcm_frame_bytes.is_some_and(|bytes| packet.data.len() / bytes > MAX_DECODED_FRAMES) {
            return Err("the PCM audio packet is too large".into());
        }
        let buffer = match decoder.decode(&packet) {
            Ok(buffer) => buffer,
            // A damaged packet is skipped, as players do.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => return Err("the audio could not be decoded".into()),
        };
        check_cancelled(cancelled)?;
        let spec = buffer.spec();
        let (rate, channels) = (spec.rate(), spec.channels().count());
        admit_rate(rate)?;
        admit_channels(channels)?;
        if buffer.frames() > MAX_DECODED_FRAMES
            || buffer.samples_interleaved() > MAX_DECODED_SAMPLES
        {
            return Err("the decoded audio packet is too large".into());
        }
        buffer.copy_to_vec_interleaved(&mut samples);
        let samples = trim(&samples, channels, &packet);
        let framer = match &mut framer {
            Some(framer) if framer.input_rate == rate => framer,
            _ => framer.insert(Framer::new(rate)?),
        };
        if !framer.push(samples, channels, emit)? {
            return Ok(());
        }
    }
    check_cancelled(cancelled)?;
    if let Some(framer) = framer {
        framer.finish(emit)?;
    }
    Ok(())
}

fn next_packet(
    format: &mut dyn FormatReader,
    budget: &ReadBudget<'_>,
) -> Result<Option<Packet>, String> {
    budget.begin()?;
    let result = format.next_packet();
    // Demuxers sometimes swallow IO errors as EOF. Never swallow our resource/cancel signal.
    budget.check()?;
    format.metadata().skip_to_latest();
    match result {
        Ok(packet) => Ok(packet),
        // Some demuxers report the end of a stream this way.
        Err(SymphoniaError::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
            Ok(None)
        }
        Err(_) => Err("the audio stream could not be read".into()),
    }
}

fn admit_channels(channels: usize) -> Result<(), String> {
    if !(1..=MAX_INPUT_CHANNELS).contains(&channels) {
        Err("audio must have between one and eight channels".into())
    } else {
        Ok(())
    }
}

fn admit_rate(rate: u32) -> Result<(), String> {
    if !(MIN_INPUT_RATE..=MAX_INPUT_RATE).contains(&rate) {
        return Err(format!(
            "audio sample rate {rate} Hz is outside the supported 8000–384000 Hz range"
        ));
    }
    if rate == RATE {
        return Ok(());
    }
    // rubato 5 Fft::new: sub_chunks = FRAME / 256, FixedSync::Output.
    // The true FFT plans have length twice fft_size_{in,out}. Even an admitted
    // rate can be coprime with 48 kHz and request a disproportionately large plan.
    let (mut a, mut b) = (rate as usize, RATE as usize);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    let min_in = rate as usize / a;
    let min_out = RATE as usize / a;
    let chunks = (FRAME / (FRAME / 256).max(1)).div_ceil(min_out).max(1);
    if chunks * min_in * 2 > MAX_FFT_SAMPLES || chunks * min_out * 2 > MAX_FFT_SAMPLES {
        return Err(format!(
            "audio sample rate {rate} Hz requires an unsupported resampling FFT size"
        ));
    }
    Ok(())
}

fn admit_params(params: &AudioCodecParameters) -> Result<(), String> {
    if let Some(rate) = params.sample_rate {
        admit_rate(rate)?;
    }
    if let Some(channels) = &params.channels {
        admit_channels(channels.count())?;
    }
    if params
        .max_frames_per_packet
        .is_some_and(|frames| frames > MAX_DECODED_FRAMES as u64)
    {
        return Err("the declared audio packet is too large".into());
    }
    if params
        .extra_data
        .as_ref()
        .is_some_and(|data| data.len() > MAX_DEMUX_BYTES)
    {
        return Err("the audio codec headers are too large".into());
    }
    Ok(())
}

/// ALAC's actual dimensions come from its magic cookie, not necessarily the container.
fn admit_alac(mut data: &[u8]) -> Result<(), String> {
    let invalid = || "the ALAC codec headers are invalid".to_owned();
    if data.get(4..8) == Some(b"frma") {
        data = data.get(12..).ok_or_else(invalid)?;
    }
    if data.get(4..8) == Some(b"alac") {
        data = data.get(12..).ok_or_else(invalid)?;
    }
    if data.len() != 24 && data.len() != 48 {
        return Err(invalid());
    }
    let frames = u32::from_be_bytes(data[..4].try_into().expect("four frame-count bytes"));
    if frames as usize > MAX_DECODED_FRAMES {
        return Err("the declared ALAC audio packet is too large".into());
    }
    admit_channels(data[9] as usize)?;
    admit_rate(u32::from_be_bytes(
        data[20..24].try_into().expect("four rate bytes"),
    ))
}

/// AudioSpecificConfig's escaped rate is a 24-bit integer and can contradict MP4's rate.
fn admit_aac(data: &[u8]) -> Result<(), String> {
    const AAC_RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let mut bits = BitReaderLtr::new(data);
    let rate = (|| -> io::Result<u32> {
        if bits.read_bits_leq32(5)? == 31 {
            bits.ignore_bits(6)?;
        }
        let index = bits.read_bits_leq32(4)?;
        if index == 15 {
            bits.read_bits_leq32(24)
        } else {
            AAC_RATES
                .get(index as usize)
                .copied()
                .ok_or_else(|| io::Error::other("invalid AAC sample-rate index"))
        }
    })()
    .map_err(|_| "the AAC codec headers are invalid".to_owned())?;
    admit_rate(rate)
}

fn pcm_sample_bytes(codec: symphonia::core::codecs::audio::AudioCodecId) -> Option<usize> {
    match codec {
        CODEC_ID_PCM_S8 | CODEC_ID_PCM_U8 | CODEC_ID_PCM_ALAW | CODEC_ID_PCM_MULAW => Some(1),
        CODEC_ID_PCM_S16LE | CODEC_ID_PCM_S16BE | CODEC_ID_PCM_U16LE | CODEC_ID_PCM_U16BE => {
            Some(2)
        }
        CODEC_ID_PCM_S24LE | CODEC_ID_PCM_S24BE | CODEC_ID_PCM_U24LE | CODEC_ID_PCM_U24BE => {
            Some(3)
        }
        CODEC_ID_PCM_S32LE | CODEC_ID_PCM_S32BE | CODEC_ID_PCM_U32LE | CODEC_ID_PCM_U32BE
        | CODEC_ID_PCM_F32LE | CODEC_ID_PCM_F32BE => Some(4),
        CODEC_ID_PCM_F64LE | CODEC_ID_PCM_F64BE => Some(8),
        _ => None,
    }
}

/// Preflight only the allocation-driving Vorbis codebook prefix, without allocating
/// codebooks. Symphonia bounds each book at 16 MiB but permits 256 of them.
/// Keep their combined vectors/tables within a conservative 32 MiB estimate.
fn admit_vorbis(data: &[u8], cancelled: &AtomicBool) -> Result<(), String> {
    let invalid = || "the Vorbis codec headers are invalid".to_owned();
    let (ident, setup) = if data.first() == Some(&2) {
        let mut offset = 1;
        let mut lengths = [0usize; 2];
        for length in &mut lengths {
            loop {
                let byte = *data.get(offset).ok_or_else(invalid)?;
                offset += 1;
                *length += byte as usize;
                if byte < 255 {
                    break;
                }
            }
        }
        let ident_end = offset.checked_add(lengths[0]).ok_or_else(invalid)?;
        let setup_start = ident_end.checked_add(lengths[1]).ok_or_else(invalid)?;
        (
            data.get(offset..ident_end).ok_or_else(invalid)?,
            data.get(setup_start..).ok_or_else(invalid)?,
        )
    } else {
        (
            data.get(..30).ok_or_else(invalid)?,
            data.get(30..).ok_or_else(invalid)?,
        )
    };
    if ident.len() < 30 || !ident.starts_with(b"\x01vorbis") || !setup.starts_with(b"\x05vorbis") {
        return Err(invalid());
    }
    admit_channels(ident[11] as usize)?;
    admit_rate(u32::from_le_bytes(
        ident[12..16].try_into().expect("four rate bytes"),
    ))?;
    let mut bits = BitReaderRtl::new(&setup[7..]);
    admit_vorbis_books(&mut bits, cancelled).map_err(|error| error.to_string())
}

fn admit_vorbis_books(bits: &mut BitReaderRtl<'_>, cancelled: &AtomicBool) -> io::Result<()> {
    const MAX_BOOK_BYTES: u64 = 32 * 1024 * 1024;
    let books = bits.read_bits_leq32(8)? + 1;
    let mut allocated = 0u64;
    for _ in 0..books {
        check_cancelled(cancelled).map_err(io::Error::other)?;
        if bits.read_bits_leq32(24)? != 0x564342 {
            return Err(io::Error::other("the Vorbis codebook signature is invalid"));
        }
        let dimensions = bits.read_bits_leq32(16)?;
        let entries = bits.read_bits_leq32(24)?;
        if dimensions == 0 || dimensions > 32 || entries > 128 * 1024 {
            return Err(io::Error::other(
                "the Vorbis codebook requires unsupported resources",
            ));
        }
        // VQ floats + growing multiplicands, codeword vectors and builder nodes.
        allocated += u64::from(entries) * (u64::from(dimensions) * 8 + 256);
        if allocated > MAX_BOOK_BYTES {
            return Err(io::Error::other(
                "the Vorbis codebooks exceed the 32 MiB decoder budget",
            ));
        }
        let mut lengths = [0u64; 33];
        if bits.read_bool()? {
            let mut length = bits.read_bits_leq32(5)? + 1;
            let mut used = 0;
            while used < entries {
                if length > 32 {
                    return Err(io::Error::other("the Vorbis codeword length is invalid"));
                }
                let remaining = entries - used;
                let count = bits.read_bits_leq32(u32::BITS - remaining.leading_zeros())?;
                if count > remaining {
                    return Err(io::Error::other("the Vorbis codeword count is invalid"));
                }
                lengths[length as usize] += u64::from(count);
                used += count;
                length += 1;
            }
        } else if bits.read_bool()? {
            for entry in 0..entries {
                if entry % 1024 == 0 {
                    check_cancelled(cancelled).map_err(io::Error::other)?;
                }
                if bits.read_bool()? {
                    lengths[(bits.read_bits_leq32(5)? + 1) as usize] += 1;
                }
            }
        } else {
            for entry in 0..entries {
                if entry % 1024 == 0 {
                    check_cancelled(cancelled).map_err(io::Error::other)?;
                }
                lengths[(bits.read_bits_leq32(5)? + 1) as usize] += 1;
            }
        }
        // Symphonia duplicates a single one-bit codeword to complete its tree.
        if lengths[1] == 1 && lengths.iter().sum::<u64>() == 1 {
            lengths[1] = 2;
        }
        let max_length = lengths.iter().rposition(|&count| count != 0).unwrap_or(0);
        let mut nodes = [0u64; 33];
        nodes[0] = 1;
        for depth in 1..=max_length {
            nodes[depth] = (nodes[depth - 1] * 2)
                .checked_sub(lengths[depth])
                .ok_or_else(|| io::Error::other("the Vorbis codeword tree is invalid"))?;
        }
        if max_length != 0 && nodes[max_length] != 0 {
            return Err(io::Error::other("the Vorbis codeword tree is incomplete"));
        }
        // CodebookBuilder consumes 4–8 bits per table level. Counting all possible
        // live prefixes bounds tables even when a short encoded book expands widely.
        let bits_per_table = max_length.clamp(4, 8);
        for depth in (0..max_length).step_by(bits_per_table) {
            let width = bits_per_table.min(max_length - depth);
            allocated += nodes[depth] * (1u64 << width) * 16;
        }
        if allocated > MAX_BOOK_BYTES {
            return Err(io::Error::other(
                "the Vorbis codebooks exceed the 32 MiB decoder budget",
            ));
        }
        match bits.read_bits_leq32(4)? {
            0 => (),
            lookup @ (1 | 2) => {
                bits.ignore_bits(64)?;
                let value_bits = bits.read_bits_leq32(4)? + 1;
                bits.ignore_bit()?;
                let values = if lookup == 2 {
                    entries * dimensions
                } else {
                    // Greatest integer root, without floating rounding/overflow.
                    let (mut low, mut high) = (0u32, entries);
                    while low < high {
                        let mid = low + (high - low).div_ceil(2);
                        if mid
                            .checked_pow(dimensions)
                            .is_some_and(|power| power <= entries)
                        {
                            low = mid;
                        } else {
                            high = mid - 1;
                        }
                    }
                    low
                };
                bits.ignore_bits(values * value_bits)?;
            }
            _ => return Err(io::Error::other("the Vorbis lookup type is invalid")),
        }
    }
    Ok(())
}

struct ReadBudget<'a> {
    cancelled: &'a AtomicBool,
    remaining: AtomicUsize,
    exceeded: AtomicBool,
}

impl<'a> ReadBudget<'a> {
    fn new(cancelled: &'a AtomicBool) -> Self {
        Self {
            cancelled,
            remaining: AtomicUsize::new(0),
            exceeded: AtomicBool::new(false),
        }
    }

    fn begin(&self) -> Result<(), String> {
        self.check()?;
        self.remaining.store(MAX_DEMUX_BYTES, Ordering::Relaxed);
        Ok(())
    }

    fn check(&self) -> Result<(), String> {
        check_cancelled(self.cancelled)?;
        if self.exceeded.load(Ordering::Relaxed) {
            Err(RESOURCE_ERROR.into())
        } else {
            Ok(())
        }
    }
}

/// Symphonia 0.6 read_boxed_slice grows 4, 8, 16, ... MiB BEFORE reading.
/// At most 4 MiB plus the 64 KiB stream ring can be supplied in one operation:
/// the first 8 MiB growth fails to read, capping that allocation at 12 MiB,
/// rather than trusting a RIFF INFO length (whose parser ignores MetadataOptions).
/// ISO-MP4 tables initially cap capacity at 1024 entries and grow only on reads;
/// Ogg/FLAC packet assembly likewise requires reads. Budget seeks too check cancellation,
/// but skipped audio bytes are not charged, preserving long seekable songs.
struct BudgetSource<'a> {
    inner: Box<dyn MediaSource>,
    budget: &'a ReadBudget<'a>,
}

impl Read for BudgetSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.budget.check().map_err(io::Error::other)?;
        if buf.is_empty() {
            return Ok(0);
        }
        let remaining = self.budget.remaining.load(Ordering::Relaxed);
        if remaining == 0 {
            self.budget.exceeded.store(true, Ordering::Relaxed);
            return Err(io::Error::other(RESOURCE_ERROR));
        }
        let allowed = remaining.min(buf.len());
        let read = self.inner.read(&mut buf[..allowed])?;
        self.budget.remaining.fetch_sub(read, Ordering::Relaxed);
        self.budget.check().map_err(io::Error::other)?;
        Ok(read)
    }
}

impl Seek for BudgetSource<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.budget.check().map_err(io::Error::other)?;
        self.inner.seek(position)
    }
}

impl MediaSource for BudgetSource<'_> {
    fn is_seekable(&self) -> bool {
        self.inner.is_seekable()
    }
    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

/// The frames of a packet that belong to the song (encoder delay and padding removed).
fn trim<'a>(samples: &'a [f32], channels: usize, packet: &Packet) -> &'a [f32] {
    let frames = samples.len() / channels;
    let start = (packet.trim_start.get() as usize).min(frames);
    let end = frames
        .saturating_sub(packet.trim_end.get() as usize)
        .max(start);
    &samples[start * channels..end * channels]
}

/// Ogg Opus, which Symphonia demuxes but does not decode.
fn decode_opus(
    format: &mut dyn FormatReader,
    id: u32,
    channels: usize,
    budget: &ReadBudget<'_>,
    emit: &mut dyn FnMut(Vec<f32>) -> bool,
) -> Result<(), String> {
    admit_channels(channels)?;
    let mut decoder = OpusDecoder::new(RATE as i32, channels).map_err(|error| error.to_string())?;
    let mut framer = Framer::new(RATE)?;
    let mut samples = vec![0.0; MAX_OPUS_FRAME * channels];
    while let Some(packet) = next_packet(format, budget)? {
        if packet.track_id != id {
            continue;
        }
        let Ok(frames) = decoder.decode(&packet.data, MAX_OPUS_FRAME, &mut samples) else {
            continue;
        };
        check_cancelled(budget.cancelled)?;
        let decoded = trim(&samples[..frames * channels], channels, &packet);
        if !framer.push(decoded, channels, emit)? {
            return Ok(());
        }
    }
    check_cancelled(budget.cancelled)?;
    framer.finish(emit)
}

/// Converts decoded audio to 48 kHz stereo and cuts it into frames.
struct Framer {
    input_rate: u32,
    resampler: Option<Fft<f32>>,
    /// Stereo at the input rate, waiting for the resampler.
    input: Vec<f32>,
    /// Stereo at 48 kHz, waiting to fill a frame.
    output: Vec<f32>,
    resampled: Vec<f32>,
}

impl Framer {
    fn new(input_rate: u32) -> Result<Self, String> {
        admit_rate(input_rate)?;
        let resampler = if input_rate == RATE {
            None
        } else {
            Some(
                Fft::new(
                    input_rate as usize,
                    RATE as usize,
                    FRAME,
                    CHANNELS,
                    FixedSync::Output,
                )
                .map_err(|error| format!("cannot resample from {input_rate} Hz: {error}"))?,
            )
        };
        let capacity = resampler
            .as_ref()
            .map_or(0, |r| r.output_frames_max() * CHANNELS);
        Ok(Self {
            input_rate,
            resampler,
            input: Vec::new(),
            output: Vec::with_capacity(FRAME * CHANNELS * 2),
            resampled: vec![0.0; capacity],
        })
    }

    /// Adds interleaved samples with `channels` channels; false once `emit` gave up.
    fn push(
        &mut self,
        samples: &[f32],
        channels: usize,
        emit: &mut dyn FnMut(Vec<f32>) -> bool,
    ) -> Result<bool, String> {
        let target = if self.resampler.is_some() {
            &mut self.input
        } else {
            &mut self.output
        };
        stereo(samples, channels, target);
        if let Some(resampler) = &mut self.resampler {
            while self.input.len() / CHANNELS >= resampler.input_frames_next() {
                let frames = self.input.len() / CHANNELS;
                let input = InterleavedSlice::new(&self.input[..], CHANNELS, frames)
                    .map_err(|error| error.to_string())?;
                let capacity = self.resampled.len() / CHANNELS;
                let mut output =
                    InterleavedSlice::new_mut(&mut self.resampled[..], CHANNELS, capacity)
                        .map_err(|error| error.to_string())?;
                let (used, made) = resampler
                    .process_into_buffer(&input, &mut output, None)
                    .map_err(|error| error.to_string())?;
                self.output
                    .extend_from_slice(&self.resampled[..made * CHANNELS]);
                self.input.drain(..used * CHANNELS);
            }
        }
        Ok(self.emit_frames(emit))
    }

    fn emit_frames(&mut self, emit: &mut dyn FnMut(Vec<f32>) -> bool) -> bool {
        let size = FRAME * CHANNELS;
        let mut start = 0;
        while self.output.len() - start >= size {
            if !emit(self.output[start..start + size].to_vec()) {
                return false;
            }
            start += size;
        }
        self.output.drain(..start);
        true
    }

    /// Resamples what is left, pads the last frame with silence and sends it.
    fn finish(mut self, emit: &mut dyn FnMut(Vec<f32>) -> bool) -> Result<(), String> {
        if let Some(resampler) = &mut self.resampler {
            // Feed the rest, then silence until the resampler's delay has passed.
            let mut remaining = self.input.len() / CHANNELS + resampler.output_delay();
            while remaining > 0 {
                let frames = self.input.len() / CHANNELS;
                let needed = resampler.input_frames_next();
                let partial = frames.min(needed);
                let input = InterleavedSlice::new(&self.input[..], CHANNELS, frames)
                    .map_err(|error| error.to_string())?;
                let capacity = self.resampled.len() / CHANNELS;
                let mut output =
                    InterleavedSlice::new_mut(&mut self.resampled[..], CHANNELS, capacity)
                        .map_err(|error| error.to_string())?;
                let indexing = Indexing {
                    input_offset: 0,
                    output_offset: 0,
                    partial_len: Some(partial),
                    active_channels_mask: None,
                };
                let (_, made) = resampler
                    .process_into_buffer(&input, &mut output, Some(&indexing))
                    .map_err(|error| error.to_string())?;
                self.output
                    .extend_from_slice(&self.resampled[..made * CHANNELS]);
                self.input.drain(..partial * CHANNELS);
                remaining = remaining.saturating_sub(needed.max(1));
            }
        }
        if !self.output.is_empty() {
            self.output.resize(FRAME * CHANNELS, 0.0);
        }
        self.emit_frames(emit);
        Ok(())
    }
}

/// Appends interleaved audio as stereo: mono is doubled, further channels dropped.
fn stereo(samples: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        2 => out.extend_from_slice(samples),
        1 => out.extend(samples.iter().flat_map(|&sample| [sample, sample])),
        _ => out.extend(
            samples
                .chunks_exact(channels)
                .flat_map(|frame| [frame[0], frame[1]]),
        ),
    }
}

/// Symphonia wants a `Sync` source; the reader is used from one thread only,
/// and `get_mut` takes no lock.
struct Unshared<R>(Mutex<R>);

impl<R: Read> Read for Unshared<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .read(buf)
    }
}

/// A 16-bit PCM stereo WAV file of interleaved `samples`.
#[cfg(test)]
pub(crate) fn stereo_wav(rate: u32, samples: impl IntoIterator<Item = i16>) -> Vec<u8> {
    let data: Vec<u8> = samples.into_iter().flat_map(i16::to_le_bytes).collect();
    let mut file = Vec::with_capacity(44 + data.len());
    file.extend_from_slice(b"RIFF");
    file.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    file.extend_from_slice(b"WAVEfmt ");
    file.extend_from_slice(&16u32.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&2u16.to_le_bytes());
    file.extend_from_slice(&rate.to_le_bytes());
    file.extend_from_slice(&(rate * 4).to_le_bytes());
    file.extend_from_slice(&4u16.to_le_bytes());
    file.extend_from_slice(&16u16.to_le_bytes());
    file.extend_from_slice(b"data");
    file.extend_from_slice(&(data.len() as u32).to_le_bytes());
    file.extend(data);
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A WAV file with a sine on the left and silence on the right.
    fn wav(rate: u32, seconds: f32) -> Vec<u8> {
        let frames = (rate as f32 * seconds) as u32;
        let samples = (0..frames).flat_map(|n| {
            let left = (n as f32 / rate as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.5;
            [(left * i16::MAX as f32) as i16, 0]
        });
        stereo_wav(rate, samples)
    }

    fn frames_of(file: Vec<u8>) -> Vec<Vec<f32>> {
        let mut frames = Vec::new();
        let source = Box::new(ReadOnlySource::new(Cursor::new(file)));
        decode_source(source, &Hint::new(), &mut |frame| {
            frames.push(frame);
            true
        })
        .unwrap();
        frames
    }

    #[test]
    fn resampled_song_keeps_its_length_and_channels() {
        let frames = frames_of(wav(44_100, 1.0));
        // One second is 50 frames; the last one is padded.
        assert!((50..=51).contains(&frames.len()), "{} frames", frames.len());
        assert!(frames.iter().all(|frame| frame.len() == FRAME * CHANNELS));
        let middle = &frames[25];
        let left = middle
            .iter()
            .step_by(2)
            .fold(0f32, |peak, s| peak.max(s.abs()));
        let right = middle
            .iter()
            .skip(1)
            .step_by(2)
            .fold(0f32, |peak, s| peak.max(s.abs()));
        assert!((0.45..0.55).contains(&left), "left peak {left}");
        assert!(right < 0.01, "right peak {right}");
    }

    #[test]
    fn song_at_48_khz_is_cut_without_resampling() {
        let frames = frames_of(wav(48_000, 0.5));
        assert_eq!(frames.len(), 25);
    }

    #[test]
    fn decoding_stops_when_nobody_takes_frames() {
        let mut taken = 0;
        let source = Box::new(ReadOnlySource::new(Cursor::new(wav(48_000, 2.0))));
        decode_source(source, &Hint::new(), &mut |_| {
            taken += 1;
            taken < 3
        })
        .unwrap();
        assert_eq!(taken, 3);
    }

    fn reject_wav_rate(rate: u32) {
        let mut file = wav(RATE, 0.02);
        file[24..28].copy_from_slice(&rate.to_le_bytes());
        file[28..32].copy_from_slice(&(rate * 4).to_le_bytes());
        let mut emitted = false;
        decode_source(
            Box::new(ReadOnlySource::new(Cursor::new(file))),
            &Hint::new(),
            &mut |_| {
                emitted = true;
                true
            },
        )
        .unwrap_err();
        assert!(!emitted, "an unsupported rate reached playback");
    }

    #[test]
    fn implausible_wav_rates_fail_before_audio_is_emitted() {
        for rate in [7_999, 384_001, 1_000_000_007] {
            reject_wav_rate(rate);
        }
    }

    #[test]
    fn plausible_but_coprime_wav_rates_reject_large_fft_plans() {
        for rate in [44_101, 47_999, 383_999] {
            reject_wav_rate(rate);
        }
        let frames = frames_of(wav(384_000, 0.5));
        assert!((25..=26).contains(&frames.len()));
        assert!(frames.iter().all(|frame| frame.len() == FRAME * CHANNELS));
        let left = frames[12]
            .iter()
            .step_by(2)
            .fold(0f32, |peak, sample| peak.max(sample.abs()));
        let right = frames[12]
            .iter()
            .skip(1)
            .step_by(2)
            .fold(0f32, |peak, sample| peak.max(sample.abs()));
        assert!((0.45..0.55).contains(&left) && right < 0.01);
    }

    #[test]
    fn excessive_wav_channels_fail_before_audio_is_emitted() {
        let mut file = wav(RATE, 0.02);
        file[22..24].copy_from_slice(&9u16.to_le_bytes());
        file[28..32].copy_from_slice(&(RATE * 18).to_le_bytes());
        file[32..34].copy_from_slice(&18u16.to_le_bytes());
        decode_source(
            Box::new(ReadOnlySource::new(Cursor::new(file))),
            &Hint::new(),
            &mut |_| panic!("nine-channel audio reached playback"),
        )
        .unwrap_err();
    }

    struct Counted<R> {
        inner: R,
        read: Arc<AtomicUsize>,
    }

    impl<R: Read> Read for Counted<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.read.fetch_add(read, Ordering::Relaxed);
            Ok(read)
        }
    }

    fn reject_streaming_metadata(header: Vec<u8>) {
        let read = Arc::new(AtomicUsize::new(0));
        // Only a small header exists in memory; the guard must stop the synthetic
        // body at 4 MiB, not materialize its advertised 8 MiB metadata allocation.
        let source = Counted {
            inner: Cursor::new(header).chain(io::repeat(0)),
            read: read.clone(),
        };
        decode_source(
            Box::new(ReadOnlySource::new(source)),
            &Hint::new(),
            &mut |_| panic!("oversized metadata reached playback"),
        )
        .unwrap_err();
        assert!(read.load(Ordering::Relaxed) <= MAX_DEMUX_BYTES);
    }

    #[test]
    fn streaming_wav_list_info_is_actually_read_bounded() {
        let info_bytes = 8 * 1024 * 1024u32;
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&(info_bytes + 64).to_le_bytes());
        header.extend_from_slice(b"WAVELIST");
        header.extend_from_slice(&(info_bytes + 12).to_le_bytes());
        header.extend_from_slice(b"INFOINAM");
        header.extend_from_slice(&info_bytes.to_le_bytes());
        reject_streaming_metadata(header);
    }

    #[test]
    fn oversized_id3_metadata_is_rejected_without_reading_its_declared_body() {
        let tag_bytes = 8 * 1024 * 1024u32;
        // ID3v2.3, syncsafe tag size, followed by a large title frame.
        let mut header = b"ID3\x03\x00\x00\x04\x00\x00\x00TIT2".to_vec();
        header.extend_from_slice(&(tag_bytes - 10).to_be_bytes());
        header.extend_from_slice(&[0, 0]);
        reject_streaming_metadata(header);
    }

    #[test]
    fn demux_budget_is_not_a_limit_on_the_whole_song() {
        let file = wav(RATE, 25.0);
        assert!(file.len() > MAX_DEMUX_BYTES);
        let mut taken = 0;
        decode_source(
            Box::new(ReadOnlySource::new(Cursor::new(file))),
            &Hint::new(),
            &mut |_| {
                taken += 1;
                true
            },
        )
        .unwrap();
        assert_eq!(taken, 1250);
    }

    fn packed_vorbis_books(fields: &[(u32, u32)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut offset = 0;
        for &(value, width) in fields {
            for bit in 0..width {
                if offset % 8 == 0 {
                    bytes.push(0);
                }
                if value & (1 << bit) != 0 {
                    bytes[offset / 8] |= 1 << (offset % 8);
                }
                offset += 1;
            }
        }
        bytes
    }

    #[test]
    fn compact_vorbis_headers_cannot_request_large_expanded_codebooks() {
        // No large payload is built or decoded: allocation-driving fields alone
        // must be rejected before Symphonia constructs its vectors or VLC tables.
        let books = packed_vorbis_books(&[(0, 8), (0x564342, 24), (32, 16), (128 * 1024, 24)]);
        let error = admit_vorbis_books(&mut BitReaderRtl::new(&books), &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.to_string().contains("32 MiB decoder budget"));
    }

    #[test]
    fn small_complete_vorbis_codebook_prefix_is_admitted() {
        // One scalar codebook with two one-bit codewords, not a decoded-song fixture.
        let books = packed_vorbis_books(&[
            (0, 8),
            (0x564342, 24),
            (1, 16),
            (2, 24),
            (0, 1),
            (0, 1),
            (0, 5),
            (0, 5),
            (0, 4),
        ]);
        admit_vorbis_books(&mut BitReaderRtl::new(&books), &AtomicBool::new(false)).unwrap();
    }

    #[test]
    fn alac_cookie_dimensions_are_admitted_before_decoder_allocation() {
        let mut cookie = vec![0; 24];
        cookie[..4].copy_from_slice(&4096u32.to_be_bytes());
        cookie[5] = 16;
        cookie[9] = 2;
        cookie[20..24].copy_from_slice(&RATE.to_be_bytes());
        admit_alac(&cookie).unwrap();
        cookie[..4].copy_from_slice(&(MAX_DECODED_FRAMES as u32 + 1).to_be_bytes());
        assert!(
            admit_alac(&cookie)
                .unwrap_err()
                .contains("packet is too large")
        );
        cookie[..4].copy_from_slice(&4096u32.to_be_bytes());
        cookie[20..24].copy_from_slice(&1_000_000_007u32.to_be_bytes());
        let mut wrapped = b"\x00\x00\x00\x0cfrma\x00\x00\x00\x00".to_vec();
        wrapped.extend_from_slice(&cookie);
        assert!(
            admit_alac(&wrapped)
                .unwrap_err()
                .contains("outside the supported")
        );
    }

    #[test]
    fn aac_escaped_rate_cannot_bypass_container_rate_admission() {
        admit_aac(&[0x12, 0x10]).unwrap(); // Ordinary AAC-LC, 44.1 kHz stereo.
        // AAC-LC, escaped sample rate 16777215 Hz; only five bytes are needed.
        let error = admit_aac(&[0x17, 0xff, 0xff, 0xff, 0x80]).unwrap_err();
        assert!(error.contains("outside the supported"));
    }
}
