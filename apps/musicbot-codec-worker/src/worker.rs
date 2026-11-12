// SPDX-License-Identifier: EUPL-1.2
//! Codec role entry points. The credential-bearing bot starts this separate
//! executable confined, with only its IPC pipes; it links no credentials,
//! session, network or configuration code. No role opens files, URLs, devices
//! or a network connection.
use nuschl_ipc::{
    Envelope,
    musicbot::{self as codec, CHANNELS, FRAME, RATE, Request, Response, Role},
};
use opus_pure::{Application, OpusEncoder, RateControl};
use std::io::{self, Read, Write};

/// Run one fixed role with inherited standard input/output only. Decoder
/// lifetimes and pipe backpressure belong to the parent supervisor: retained
/// output is bounded there, rather than imposing a duration limit on a song.
pub fn run(role: Role) -> Result<(), String> {
    let input = io::stdin();
    let mut output = io::stdout().lock();
    match role {
        Role::Decode => decode(input, &mut output),
        Role::Encode => encode(&mut input.lock(), &mut output),
        Role::Duck => duck(&mut input.lock(), &mut output),
    }
}

fn error(message: &str) -> Response {
    let mut end = message.len().min(codec::MAX_ERROR_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    Response::Error {
        message: message[..end].to_owned(),
    }
}

fn reply(output: &mut impl Write, id: u64, response: Response) -> Result<(), String> {
    codec::write_message(output, &Envelope::new(id, response))
}

fn next(input: &mut impl Read, expected: u64) -> Result<Option<Request>, String> {
    let Some(request) = codec::read_message::<Request>(input)? else {
        return Ok(None);
    };
    if request.request_id != expected {
        return Err("unexpected codec request id".into());
    }
    Ok(Some(request.payload))
}

fn decode(mut input: impl Read + Send + 'static, output: &mut impl Write) -> Result<(), String> {
    let Some(Request::Decode {
        mime,
        extension,
        mut skip_frames,
    }) = next(&mut input, 1)?
    else {
        return Err("decode role requires one decode request".into());
    };
    if mime.as_ref().is_some_and(|mime| mime.len() > 256)
        || extension
            .as_ref()
            .is_some_and(|extension| extension.len() > 64)
    {
        return reply(output, 1, error("audio format hint exceeds its budget"));
    }
    let mut failure = None;
    let result = crate::decode::decode_stream(
        input,
        mime.as_deref(),
        extension.as_deref(),
        &mut |samples| {
            if !codec::valid_pcm(&samples) {
                failure = Some("decoder produced invalid PCM".to_owned());
                return false;
            }
            if skip_frames > 0 {
                skip_frames -= 1;
                return true;
            }
            match reply(output, 1, Response::Pcm { samples }) {
                Ok(()) => true,
                Err(error) => {
                    failure = Some(error);
                    false
                }
            }
        },
    );
    if let Some(failure) = failure {
        return Err(failure);
    }
    reply(
        output,
        1,
        match result {
            Ok(()) => Response::Done,
            Err(message) => error(&message),
        },
    )
}

fn new_encoder() -> Result<OpusEncoder, String> {
    let mut encoder = OpusEncoder::new(RATE as i32, CHANNELS, Application::Audio)
        .map_err(|_| "could not create the audio encoder".to_owned())?;
    encoder.rate_control = RateControl::ConstrainedVbr;
    Ok(encoder)
}

fn encode(input: &mut impl Read, output: &mut impl Write) -> Result<(), String> {
    let mut encoder = new_encoder()?;
    let mut packet = [0; codec::MAX_OPUS_BYTES];
    let mut id = 1;
    while let Some(request) = next(input, id)? {
        let response = match request {
            Request::Encode { samples, bitrate }
                if codec::valid_pcm(&samples) && (6_000..=510_000).contains(&bitrate) =>
            {
                encoder.bitrate_bps = bitrate as i32;
                match encoder.encode(&samples, FRAME, &mut packet) {
                    Ok(length) if length > 0 && length <= packet.len() => Response::Packet {
                        opus: packet[..length].to_vec(),
                    },
                    _ => error("the audio could not be encoded"),
                }
            }
            Request::Clear => {
                encoder = new_encoder()?;
                Response::Ack
            }
            _ => return Err("invalid request for encode role".into()),
        };
        reply(output, id, response)?;
        id = id.checked_add(1).ok_or("codec request id exhausted")?;
    }
    Ok(())
}

fn duck(input: &mut impl Read, output: &mut impl Write) -> Result<(), String> {
    let mut detector = crate::vad::Detector::default();
    let mut id = 1;
    while let Some(request) = next(input, id)? {
        let response = match request {
            Request::Hear {
                sender,
                opus,
                threshold,
            } if !opus.is_empty()
                && opus.len() <= codec::MAX_VOICE_BYTES
                && threshold.is_finite()
                && (0.0..=1.0).contains(&threshold) =>
            {
                Response::Speech {
                    active: detector.hear(sender, &opus, threshold),
                }
            }
            Request::Forget { sender } => {
                detector.forget(sender);
                Response::Ack
            }
            Request::Clear => {
                detector.clear();
                Response::Ack
            }
            _ => return Err("invalid request for duck role".into()),
        };
        reply(output, id, response)?;
        id = id.checked_add(1).ok_or("codec request id exhausted")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn messages(requests: impl IntoIterator<Item = (u64, Request)>) -> Cursor<Vec<u8>> {
        let mut input = Vec::new();
        for (id, request) in requests {
            codec::write_message(&mut input, &Envelope::new(id, request)).unwrap();
        }
        Cursor::new(input)
    }

    fn responses(bytes: Vec<u8>) -> Vec<Envelope<Response>> {
        let mut input = Cursor::new(bytes);
        let mut responses = Vec::new();
        while let Some(response) = codec::read_message(&mut input).unwrap() {
            responses.push(response);
        }
        responses
    }

    fn audio() -> Vec<f32> {
        (0..FRAME * CHANNELS)
            .map(|index| {
                (index as f32 / CHANNELS as f32 * std::f32::consts::TAU * 440. / RATE as f32).sin()
                    * 0.25
            })
            .collect()
    }

    #[test]
    fn encoder_replies_match_ids_and_survive_a_state_reset() {
        let frame = || Request::Encode {
            samples: audio(),
            bitrate: 128_000,
        };
        let mut input = messages([(1, frame()), (2, Request::Clear), (3, frame())]);
        let mut output = Vec::new();
        encode(&mut input, &mut output).unwrap();
        let replies = responses(output);
        assert_eq!(
            replies
                .iter()
                .map(|reply| reply.request_id)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(matches!(replies[1].payload, Response::Ack));
        let mut decoded = [0.0; FRAME * CHANNELS];
        for reply in [&replies[0], &replies[2]] {
            let Response::Packet { opus } = &reply.payload else {
                panic!("encoder did not return a packet");
            };
            assert!(!opus.is_empty() && opus.len() <= codec::MAX_OPUS_BYTES);
            let mut decoder = opus_pure::OpusDecoder::new(RATE as i32, CHANNELS).unwrap();
            assert_eq!(decoder.decode(opus, FRAME, &mut decoded).unwrap(), FRAME);
            assert!(decoded.iter().all(|sample| sample.is_finite()));
            assert!(decoded.iter().any(|sample| sample.abs() > 0.01));
        }
    }

    #[test]
    fn role_and_sequence_rejection_precedes_a_reply() {
        for request in [Request::Clear, Request::Forget { sender: 1 }] {
            let mut output = Vec::new();
            assert!(encode(&mut messages([(2, request)]), &mut output).is_err());
            assert!(output.is_empty());
        }
        let mut output = Vec::new();
        assert!(
            duck(
                &mut messages([(
                    1,
                    Request::Encode {
                        samples: audio(),
                        bitrate: 128_000
                    }
                )]),
                &mut output,
            )
            .is_err()
        );
        assert!(output.is_empty());
        let mut output = Vec::new();
        assert!(
            encode(
                &mut messages([(1, Request::Clear), (1, Request::Clear)]),
                &mut output,
            )
            .is_err()
        );
        assert_eq!(responses(output).len(), 1);
    }

    #[test]
    fn invalid_encode_and_duck_shapes_are_not_processed() {
        for (samples, bitrate) in [(vec![0.0; FRAME], 128_000), (audio(), 510_001)] {
            assert!(
                encode(
                    &mut messages([(1, Request::Encode { samples, bitrate })]),
                    &mut Vec::new(),
                )
                .is_err()
            );
        }
        for (opus, threshold) in [(Vec::new(), 0.6), (vec![1; 1025], 0.6), (vec![1], 1.1)] {
            assert!(
                duck(
                    &mut messages([(
                        1,
                        Request::Hear {
                            sender: 7,
                            opus,
                            threshold
                        }
                    )]),
                    &mut Vec::new(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn duck_control_replies_keep_the_original_ids() {
        let mut output = Vec::new();
        duck(
            &mut messages([(1, Request::Forget { sender: 7 }), (2, Request::Clear)]),
            &mut output,
        )
        .unwrap();
        let replies = responses(output);
        assert_eq!(replies.len(), 2);
        for (index, reply) in replies.into_iter().enumerate() {
            assert_eq!(reply.request_id, index as u64 + 1);
            assert!(matches!(reply.payload, Response::Ack));
        }
    }

    fn wav() -> Vec<u8> {
        let pcm: Vec<u8> = (0..3)
            .flat_map(|_| audio())
            .flat_map(|sample| ((sample * f32::from(i16::MAX)) as i16).to_le_bytes())
            .collect();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&(CHANNELS as u16).to_le_bytes());
        bytes.extend_from_slice(&RATE.to_le_bytes());
        bytes.extend_from_slice(&(RATE * CHANNELS as u32 * 2).to_le_bytes());
        bytes.extend_from_slice(&(CHANNELS as u16 * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        bytes.extend(pcm);
        bytes
    }

    #[test]
    fn decoder_consumes_framed_request_then_raw_media_with_bounded_replies() {
        let mut input = messages([(
            1,
            Request::Decode {
                mime: Some("audio/wav".into()),
                extension: Some("wav".into()),
                skip_frames: 1,
            },
        )])
        .into_inner();
        input.extend(wav());
        let mut output = Vec::new();
        decode(Cursor::new(input), &mut output).unwrap();
        let replies = responses(output);
        assert!(replies.iter().all(|reply| reply.request_id == 1));
        assert_eq!(replies.len(), 3);
        for reply in &replies[..2] {
            let Response::Pcm { samples } = &reply.payload else {
                panic!("decoder did not return PCM");
            };
            assert!(codec::valid_pcm(samples));
            assert!(samples.iter().any(|sample| sample.abs() > 0.01));
        }
        assert!(matches!(replies[2].payload, Response::Done));
    }

    #[test]
    fn invalid_audio_has_one_bounded_error_and_no_done_or_pcm() {
        let input = messages([(
            1,
            Request::Decode {
                mime: None,
                extension: None,
                skip_frames: 0,
            },
        )]);
        let mut output = Vec::new();
        decode(input, &mut output).unwrap();
        let replies = responses(output);
        assert_eq!(replies.len(), 1);
        let Response::Error { message } = &replies[0].payload else {
            panic!("invalid media did not fail");
        };
        assert!(message.len() <= codec::MAX_ERROR_BYTES);
        let Response::Error { message } = error(&"é".repeat(400)) else {
            unreachable!()
        };
        assert_eq!(message.len(), codec::MAX_ERROR_BYTES);
    }

    /// Leaves 32 bytes for Mumble framing within its 1 KiB voice packet limit.
    #[test]
    fn high_bitrate_transients_fit_mumble_packets() {
        use nuschl_protocol::voice::{PROTOBUF_VERSION, VoicePacket, encode_voice};
        let mut encoder = OpusEncoder::new(RATE as i32, CHANNELS, Application::Audio).unwrap();
        encoder.bitrate_bps = 320_000;
        encoder.rate_control = RateControl::ConstrainedVbr;
        let mut decoder = opus_pure::OpusDecoder::new(RATE as i32, CHANNELS).unwrap();
        let mut input = [0f32; FRAME * CHANNELS];
        let mut output = [0u8; codec::MAX_OPUS_BYTES];
        let mut decoded = [0f32; FRAME * CHANNELS];
        let mut random = 0x2545_f491_u32;
        for frame in 0..600 {
            for sample in 0..FRAME {
                let t = (frame * FRAME + sample) as f32 / RATE as f32;
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                let noise = random as f32 / u32::MAX as f32 - 0.5;
                let attack = if frame % 17 < 3 { 0.65 } else { 0.02 };
                let envelope = if frame % 31 < 12 { 0.75 } else { 0.12 };
                let mut left = noise * attack;
                let mut right = noise * attack * 0.8;
                for partial in 1..=8 {
                    let frequency = (110.0 + (frame / 31 % 4) as f32 * 55.0) * partial as f32;
                    left += (t * frequency * std::f32::consts::TAU).sin() * envelope
                        / partial as f32
                        * 0.18;
                    right += (t * frequency * 1.013 * std::f32::consts::TAU).sin() * envelope
                        / partial as f32
                        * 0.18;
                }
                input[sample * 2] = left;
                input[sample * 2 + 1] = right;
            }
            let length = encoder.encode(&input, FRAME, &mut output).unwrap();
            let packet = VoicePacket {
                sender: 0,
                sequence: u64::MAX - 1200 + frame as u64 * 2,
                opus: output[..length].to_vec(),
                end: frame == 599,
                volume: 1.0,
            };
            for version in [0, PROTOBUF_VERSION] {
                assert!(
                    encode_voice(version, 31, &packet).is_ok(),
                    "frame {frame}: {length} Opus bytes cannot be sent"
                );
            }
            assert_eq!(
                decoder.decode(&packet.opus, FRAME, &mut decoded).unwrap(),
                FRAME,
                "packet duration changed at frame {frame}"
            );
        }
    }
}
