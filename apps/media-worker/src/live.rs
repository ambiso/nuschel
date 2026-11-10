// SPDX-License-Identifier: EUPL-1.2
//! Live audio and all codec/DSP state stay in this process, without server secrets.
//!
//! The device callbacks only move samples through lock-free single-producer
//! rings; they never lock, allocate or block. This thread decodes, buffers,
//! mixes, cancels echo, encodes and talks to the supervisor.
use crate::{
    adaptive::{Controller, WINDOW},
    dsp::{CapturePipeline, FRAME, RATE},
    jitter::{JitterBuffer, Pull},
};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use nuschel_core::audio::{AudioDevice, AudioSettings, EchoMode, EchoSource};
use nuschel_ipc::{Envelope, LiveMediaEvent, LiveMediaRequest, read_frame, write_frame};
use opus_pure::{Application, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OpusDecoder, OpusEncoder};
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

/// Mixed audio is kept ready in the playback ring; the adaptive controller
/// sizes it (device buffering is extra). The ring must hold the largest fill.
const RING: usize = 20 * FRAME;
/// The canceller's delay hint for the smallest playback fill.
const BASE_DELAY_MS: i32 = 50;

const MAX_SPEAKERS: usize = 64;
const LEVEL_EVERY: u32 = 10;

enum Event {
    Request(LiveMediaRequest),
    DeviceError(String),
    Closed,
}

struct Devices {
    _input: Option<cpal::Stream>,
    _output: cpal::Stream,
    capture: Option<Consumer<f32>>,
    playback: Producer<f32>,
    /// Output callbacks that found the ring empty; written by the device thread.
    underruns: Arc<AtomicU32>,
}

struct Speaker {
    decoder: OpusDecoder,
    jitter: JitterBuffer,
    pcm: VecDeque<f32>,
    volume: f32,
    last_packet: Instant,
}

impl Speaker {
    /// Top up decoded audio for one output frame.
    fn fill(&mut self, scratch: &mut [f32]) {
        while self.pcm.len() < FRAME {
            match self.jitter.pull() {
                Pull::Packet { opus, end } => {
                    if let Ok(count) = self.decoder.decode(&opus, MAX_PACKET_SAMPLES, scratch) {
                        self.jitter.observe_frames((count / FRAME) as u64);
                        self.extend(&scratch[..count]);
                    }
                    if end {
                        let _ = self.decoder.reset_state();
                    }
                }
                Pull::Lost { fec, frames } => {
                    let size = (frames as usize * FRAME).min(MAX_PACKET_SAMPLES);
                    let decoded = match fec {
                        Some(next) => self.decoder.decode_fec(&next, size, scratch),
                        None => self.decoder.decode(&[], size, scratch),
                    };
                    if let Ok(count) = decoded {
                        self.extend(&scratch[..count]);
                    } else {
                        break;
                    }
                }
                Pull::Idle => break,
            }
        }
    }

    fn extend(&mut self, samples: &[f32]) {
        let volume = self.volume;
        self.pcm.extend(samples.iter().map(|sample| {
            let value = sample * volume;
            if value.is_finite() { value } else { 0. }
        }));
        // Never accumulate more than 200 ms of decoded audio per speaker.
        let excess = self.pcm.len().saturating_sub(RING / 2);
        self.pcm.drain(..excess);
    }
}

struct Transmit {
    encoder: OpusEncoder,
    pending: Vec<f32>,
    sequence: u64,
    active: bool,
}

impl Transmit {
    fn encode(&mut self, samples: &[f32]) -> Option<Vec<u8>> {
        let mut packet = vec![0; MAX_PACKET_BYTES];
        let count = self
            .encoder
            .encode(samples, samples.len(), &mut packet)
            .ok()?;
        packet.truncate(count);
        Some(packet)
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::sync_channel(256);
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        while let Ok(Some(frame)) = read_frame::<LiveMediaRequest>(&mut input) {
            if input_tx.send(Event::Request(frame.payload)).is_err() {
                return;
            }
        }
        let _ = input_tx.send(Event::Closed);
    });
    let mut output = io::stdout().lock();
    let mut settings = AudioSettings::default();
    let mut encoder = OpusEncoder::new(48_000, 1, Application::Voip)?;
    encoder.bitrate_bps = 48_000;
    encoder.use_inband_fec = true;
    encoder.packet_loss_perc = 5;
    let mut transmit = Transmit {
        encoder,
        pending: Vec::with_capacity(2 * FRAME),
        sequence: 0,
        active: false,
    };
    let mut muted = true;
    let mut deafened = false;
    // Devices open only after the supervisor states mute/deafen: dropping a live
    // native stream can block, so streams are never reopened for a state change.
    let mut state_known = false;
    let mut epoch = 0;
    let mut push_to_talk = false;
    let mut speakers = BTreeMap::<u32, Speaker>::new();
    let mut devices: Option<Devices> = None;
    let mut pipeline: Option<CapturePipeline> = None;
    let mut retry_at = Instant::now();
    let mut last_cleanup = Instant::now();
    let mut frames_seen = 0_u32;
    let mut controller = Controller::default();
    let mut last_window = Instant::now();
    // Start-up underruns (the device starts before the ring is filled) are not a signal.
    let mut skip_window = true;
    let mut capture_frame = [0.; FRAME];
    let mut mix = [0.; FRAME];
    let mut scratch = vec![0.; MAX_PACKET_SAMPLES];
    loop {
        if devices.is_none() && state_known && !deafened && Instant::now() >= retry_at {
            match open_devices(tx.clone(), !muted, &settings) {
                Ok((opened, route)) => {
                    let echo = match &route.echo {
                        EchoSource::Nuschel => true,
                        EchoSource::Upstream(_) | EchoSource::None => false,
                    };
                    let mut chain = CapturePipeline::new(&settings, echo);
                    chain.set_delay_hint(delay_hint(&controller));
                    pipeline = Some(chain);
                    let message = if muted {
                        "Audio ready · microphone muted".to_string()
                    } else if opened.capture.is_some() {
                        match &route.echo {
                            EchoSource::Upstream(name) => {
                                format!("Audio ready · system echo cancellation ({name})")
                            }
                            EchoSource::Nuschel => "Audio ready · echo cancellation on".into(),
                            EchoSource::None => "Audio ready".into(),
                        }
                    } else {
                        "Playback ready; microphone unavailable".into()
                    };
                    emit(
                        &mut output,
                        LiveMediaEvent::Devices {
                            inputs: route.inputs,
                            outputs: route.outputs,
                            input: route.input,
                            output: route.output,
                            echo: route.echo,
                        },
                    )?;
                    emit(
                        &mut output,
                        LiveMediaEvent::Status {
                            ready: true,
                            message,
                        },
                    )?;
                    devices = Some(opened);
                    last_window = Instant::now();
                    skip_window = true;
                }
                Err(error) => {
                    emit(
                        &mut output,
                        LiveMediaEvent::Status {
                            ready: false,
                            message: format!("Audio reconnecting: {error}"),
                        },
                    )?;
                    retry_at = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        let event = match rx.recv_timeout(Duration::from_millis(4)) {
            Ok(event) => Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(_) => break,
        };
        match event {
            Some(Event::Closed | Event::Request(LiveMediaRequest::Shutdown)) => break,
            Some(Event::DeviceError(message)) => {
                devices.take();
                retry_at = Instant::now() + Duration::from_secs(1);
                emit(
                    &mut output,
                    LiveMediaEvent::Status {
                        ready: false,
                        message: format!("Audio reconnecting: {message}"),
                    },
                )?;
            }
            Some(Event::Request(LiveMediaRequest::SetState {
                epoch: new_epoch,
                muted: mute,
                deafened: deaf,
            })) => {
                let was_transmitting = transmit.active;
                let capture_changed = state_known && (mute || deaf) != muted;
                muted = mute || deaf;
                epoch = new_epoch;
                deafened = deaf;
                state_known = true;
                if deaf {
                    speakers.clear();
                }
                if was_transmitting && muted {
                    finish(&mut output, &mut transmit, epoch)?;
                }
                if capture_changed && devices.is_some() {
                    // The supervisor replaces this process for capture changes; never
                    // risk a blocking stream teardown while the microphone may be open.
                    emit(
                        &mut output,
                        LiveMediaEvent::Status {
                            ready: false,
                            message: "Restarting audio…".into(),
                        },
                    )?;
                    std::process::exit(0);
                }
            }
            Some(Event::Request(LiveMediaRequest::Configure {
                bitrate,
                packet_loss_percent,
                settings: requested,
            })) => {
                transmit.encoder.bitrate_bps = bitrate.clamp(6000, 64_000) as i32;
                transmit.encoder.packet_loss_perc = i32::from(packet_loss_percent.clamp(1, 30));
                let requested = requested.sanitized();
                let route_changed = requested.input_device != settings.input_device
                    || requested.output_device != settings.output_device
                    || requested.echo_cancellation != settings.echo_cancellation
                    || requested.device_overrides != settings.device_overrides;
                settings = requested;
                if route_changed && devices.is_some() {
                    // A new route needs new streams; the supervisor restarts this process.
                    emit(
                        &mut output,
                        LiveMediaEvent::Status {
                            ready: false,
                            message: "Switching audio devices…".into(),
                        },
                    )?;
                    std::process::exit(0);
                } else if let Some(chain) = pipeline.as_mut() {
                    // Rebuild the chain with the new processing choices on the same route.
                    let mut rebuilt = CapturePipeline::new(&settings, chain.echo_cancellation());
                    rebuilt.set_delay_hint(delay_hint(&controller));
                    *chain = rebuilt;
                }
            }
            Some(Event::Request(LiveMediaRequest::PushToTalk { active })) => push_to_talk = active,
            Some(Event::Request(LiveMediaRequest::Playback {
                sender,
                sequence,
                opus,
                end,
                volume,
            })) => {
                if deafened || opus.is_empty() || opus.len() > 1024 || !volume.is_finite() {
                    continue;
                }
                if !speakers.contains_key(&sender) && speakers.len() >= MAX_SPEAKERS {
                    continue;
                }
                let speaker = match speakers.entry(sender) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => entry.insert(Speaker {
                        decoder: OpusDecoder::new(48_000, 1)?,
                        jitter: JitterBuffer::default(),
                        pcm: VecDeque::with_capacity(RING / 2),
                        volume: 1.,
                        last_packet: Instant::now(),
                    }),
                };
                // A restarted stream (sequence reset after silence) gets a fresh buffer.
                if speaker.last_packet.elapsed() > Duration::from_secs(1)
                    && !speaker.jitter.is_playing()
                {
                    speaker.jitter = JitterBuffer::default();
                    let _ = speaker.decoder.reset_state();
                }
                speaker.volume = volume.clamp(0., 4.);
                speaker.last_packet = Instant::now();
                speaker.jitter.push(sequence, opus, end, Instant::now());
            }
            None => {}
        }
        let Some(open) = devices.as_mut() else {
            continue;
        };
        // Playback: keep a short, steady amount of mixed audio queued.
        while open.playback.slots() >= FRAME
            && RING - open.playback.slots() < controller.playback_samples()
        {
            mix.fill(0.);
            for speaker in speakers.values_mut() {
                speaker.fill(&mut scratch);
                for (out, sample) in mix
                    .iter_mut()
                    .zip(speaker.pcm.drain(..speaker.pcm.len().min(FRAME)))
                {
                    *out += sample;
                }
            }
            for sample in &mut mix {
                *sample = sample.clamp(-1., 1.);
            }
            let (pushed, _) = open.playback.push_partial_slice(&mix);
            if pushed.len() < FRAME {
                break;
            }
            if let Some(chain) = pipeline.as_mut() {
                chain.render(&mix);
            }
            controller.observe_render(&mix);
        }
        if last_window.elapsed() >= WINDOW {
            last_window = Instant::now();
            let underruns = open.underruns.swap(0, Ordering::Relaxed);
            let underruns = if std::mem::take(&mut skip_window) {
                0
            } else {
                underruns
            };
            let residual = pipeline
                .as_ref()
                .filter(|chain| chain.echo_cancellation())
                .and_then(|chain| chain.echo_metrics().residual_likelihood);
            let before = controller.playback_samples();
            if let Some(health) = controller.window(residual, underruns) {
                if controller.playback_samples() != before
                    && let Some(chain) = pipeline.as_mut()
                {
                    chain.set_delay_hint(delay_hint(&controller));
                }
                emit(
                    &mut output,
                    LiveMediaEvent::Health {
                        clipping: health.clipping,
                        echo_guard: health.echo_guard,
                        double_talk: health.double_talk,
                        underruns: health.underruns,
                        playback_ms: health.playback_ms,
                    },
                )?;
            }
        }
        // Capture: process every complete 10 ms frame.
        let Some(capture) = open.capture.as_mut() else {
            continue;
        };
        while capture.slots() >= FRAME {
            if capture.pop_entire_slice(&mut capture_frame).is_err() {
                break;
            }
            let Some(chain) = pipeline.as_mut() else {
                break;
            };
            let raw = capture_frame;
            let result = chain.process(&mut capture_frame, push_to_talk, controller.guard_gate());
            controller.observe_capture(&raw, result.voice);
            frames_seen = frames_seen.wrapping_add(1);
            if frames_seen.is_multiple_of(LEVEL_EVERY) {
                emit(
                    &mut output,
                    LiveMediaEvent::Level {
                        level_db: result.level_db,
                        voice: result.voice,
                        transmitting: result.transmit && !muted,
                    },
                )?;
            }
            if frames_seen.is_multiple_of(200) && chain.echo_cancellation() {
                let metrics = chain.echo_metrics();
                emit(
                    &mut output,
                    LiveMediaEvent::Echo {
                        erle_db: metrics.erle_db,
                        residual_likelihood: metrics.residual_likelihood,
                        delay_ms: metrics.delay_ms,
                    },
                )?;
            }
            if muted {
                continue;
            }
            if result.transmit {
                transmit.active = true;
                transmit.pending.extend_from_slice(&capture_frame);
                if transmit.pending.len() >= 2 * FRAME {
                    let samples: Vec<f32> = transmit.pending.drain(..2 * FRAME).collect();
                    if let Some(opus) = transmit.encode(&samples) {
                        emit(
                            &mut output,
                            LiveMediaEvent::Packet {
                                epoch,
                                sequence: transmit.sequence,
                                opus,
                                end: false,
                            },
                        )?;
                    }
                    transmit.sequence += 2;
                }
            } else if transmit.active {
                finish(&mut output, &mut transmit, epoch)?;
            }
        }
        if last_cleanup.elapsed() > Duration::from_secs(2) {
            speakers.retain(|_, speaker| {
                speaker.last_packet.elapsed() < Duration::from_secs(5)
                    || speaker.jitter.is_playing()
            });
            last_cleanup = Instant::now();
        }
    }
    output.flush()?;
    if devices.is_some() {
        // Dropping native streams can block; the OS closes them with the process.
        std::process::exit(0);
    }
    Ok(())
}

/// Device buffering plus Nuschel's own playback fill above the minimum.
fn delay_hint(controller: &Controller) -> i32 {
    let extra = controller.playback_samples() - crate::adaptive::PLAYBACK_MIN_FRAMES * FRAME;
    BASE_DELAY_MS + (extra / (RATE as usize / 1000)) as i32
}

/// Close a transmission with a terminator packet so receivers stop promptly.
fn finish(
    output: &mut impl io::Write,
    transmit: &mut Transmit,
    epoch: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut samples = std::mem::take(&mut transmit.pending);
    samples.resize(2 * FRAME, 0.);
    if let Some(opus) = transmit.encode(&samples) {
        emit(
            output,
            LiveMediaEvent::Packet {
                epoch,
                sequence: transmit.sequence,
                opus,
                end: true,
            },
        )?;
    }
    transmit.sequence += 2;
    transmit.active = false;
    let _ = transmit.encoder.reset_state();
    Ok(())
}

fn emit(
    output: &mut impl io::Write,
    event: LiveMediaEvent,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(output, &Envelope::new(0, event))?;
    Ok(())
}

struct Route {
    inputs: Vec<AudioDevice>,
    outputs: Vec<AudioDevice>,
    input: Option<String>,
    output: Option<String>,
    echo: EchoSource,
}

fn open_devices(
    tx: SyncSender<Event>,
    capture: bool,
    settings: &AudioSettings,
) -> Result<(Devices, Route), String> {
    let mut hosts = vec![];
    #[cfg(target_os = "linux")]
    {
        match std::env::var("NUSCHEL_AUDIO_BACKEND").ok().as_deref() {
            Some("pulse") => hosts.push(cpal::HostId::PulseAudio),
            Some("pipewire") => hosts.push(cpal::HostId::PipeWire),
            Some("alsa") => hosts.push(cpal::HostId::Alsa),
            _ => {
                hosts.push(cpal::HostId::PipeWire);
                hosts.push(cpal::HostId::PulseAudio);
                hosts.push(cpal::HostId::Alsa);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    hosts.push(cpal::default_host().id());
    let mut errors = Vec::new();
    for id in hosts {
        match cpal::host_from_id(id)
            .map_err(|error| error.to_string())
            .and_then(|host| open_host(&host, tx.clone(), capture, settings))
        {
            Ok(opened) => return Ok(opened),
            Err(error) => errors.push(format!("{id:?}: {error}")),
        }
    }
    Err(errors.join("; "))
}

fn device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| device_id(device).unwrap_or_else(|| "Unknown device".into()))
}

/// The backend-local part of a device identifier, e.g. a PulseAudio name.
fn backend_name(id: &str) -> &str {
    id.split_once(':').map_or(id, |(_, name)| name)
}

/// Recognise PipeWire's and PulseAudio's echo-cancel nodes by name. Anything
/// else is unknown: missing metadata is not proof that no canceller exists.
pub fn is_echo_cancelled(id: &str, name: &str) -> bool {
    let text = format!("{} {}", backend_name(id), name).to_lowercase();
    ["echo-cancel", "echo_cancel", "echocancel", "echo cancel"]
        .iter()
        .any(|needle| text.contains(needle))
}

/// The playback node feeding an echo-cancel source's reference, if any.
pub fn paired_reference<'a>(source_id: &str, sinks: &'a [AudioDevice]) -> Option<&'a AudioDevice> {
    let source = backend_name(source_id);
    let swapped = source.replace("source", "sink").replace("input", "output");
    sinks
        .iter()
        .find(|sink| backend_name(&sink.id) == swapped)
        .or_else(|| {
            let candidates: Vec<_> = sinks
                .iter()
                .filter(|sink| is_echo_cancelled(&sink.id, &sink.name))
                .collect();
            (candidates.len() == 1).then(|| candidates[0])
        })
}

fn open_host(
    host: &cpal::Host,
    tx: SyncSender<Event>,
    capture: bool,
    settings: &AudioSettings,
) -> Result<(Devices, Route), String> {
    let all: Vec<cpal::Device> = host
        .devices()
        .map(|devices| devices.collect())
        .unwrap_or_default();
    let listed = |input: bool| -> Vec<AudioDevice> {
        all.iter()
            .filter(|device| {
                if input {
                    device.supports_input()
                } else {
                    device.supports_output()
                }
            })
            .filter_map(|device| {
                let id = device_id(device)?;
                // Sink monitors are listed as sources by PulseAudio; they are not microphones.
                (!(input && backend_name(&id).ends_with(".monitor"))).then(|| AudioDevice {
                    name: device_name(device),
                    id,
                })
            })
            .take(128)
            .collect()
    };
    let inputs = listed(true);
    let outputs = listed(false);
    let find = |id: &Option<String>, input: bool| {
        id.as_ref().and_then(|wanted| {
            all.iter().find(|device| {
                (if input {
                    device.supports_input()
                } else {
                    device.supports_output()
                }) && device_id(device).as_deref() == Some(wanted.as_str())
            })
        })
    };
    let input_device = find(&settings.input_device, true)
        .cloned()
        .or_else(|| host.default_input_device());
    let mut input_id = input_device.as_ref().and_then(device_id);
    let mut input_name = input_device.as_ref().map(device_name).unwrap_or_default();
    // PipeWire's default devices are virtual nodes that follow the session
    // default; resolve the real node through the PulseAudio API so a system
    // echo canceller can be recognised and its reference fed.
    #[cfg(target_os = "linux")]
    if host.id() == cpal::HostId::PipeWire
        && settings.input_device.is_none()
        && let Some((id, name)) = resolve_pulse_default(true)
    {
        input_id = Some(format!("pipewire:{}", backend_name(&id)));
        input_name = name;
    }
    let upstream = input_id
        .as_ref()
        .is_some_and(|id| is_echo_cancelled(id, &input_name));
    let mode = input_id
        .as_ref()
        .map_or(settings.echo_cancellation, |id| settings.echo_for(id));
    let echo = if !capture {
        EchoSource::None
    } else {
        match mode {
            EchoMode::Off => EchoSource::None,
            EchoMode::On => EchoSource::Nuschel,
            EchoMode::Auto if upstream => EchoSource::Upstream(input_name.clone()),
            EchoMode::Auto => EchoSource::Nuschel,
        }
    };
    // With a system canceller, Nuschel's own playback must reach its reference.
    // Only Nuschel's stream is routed; system defaults and other apps are untouched.
    let reference = if settings.output_device.is_none() && upstream {
        input_id
            .as_ref()
            .and_then(|id| paired_reference(id, &outputs))
            .map(|device| device.id.clone())
    } else {
        None
    };
    let output_device = find(&settings.output_device, false)
        .or_else(|| find(&reference, false))
        .cloned()
        .or_else(|| host.default_output_device())
        .ok_or("no default output device")?;
    let output_id = device_id(&output_device);
    let config = output_device
        .default_output_config()
        .map_err(|error| error.to_string())?;
    let (playback, playback_consumer) = RingBuffer::<f32>::new(RING);
    let underruns = Arc::new(AtomicU32::new(0));
    let stream = match config.sample_format() {
        SampleFormat::F32 => output_stream::<f32>(
            &output_device,
            stream_config(host, config),
            tx.clone(),
            playback_consumer,
            underruns.clone(),
        ),
        SampleFormat::I16 => output_stream::<i16>(
            &output_device,
            stream_config(host, config),
            tx.clone(),
            playback_consumer,
            underruns.clone(),
        ),
        SampleFormat::U16 => output_stream::<u16>(
            &output_device,
            stream_config(host, config),
            tx.clone(),
            playback_consumer,
            underruns.clone(),
        ),
        SampleFormat::I32 => output_stream::<i32>(
            &output_device,
            stream_config(host, config),
            tx.clone(),
            playback_consumer,
            underruns.clone(),
        ),
        _ => return Err("unsupported output sample format".into()),
    }?;
    stream.play().map_err(|error| error.to_string())?;
    let mut capture_consumer = None;
    let input = capture.then_some(input_device).flatten().and_then(|input| {
        let config = input.default_input_config().ok()?;
        let (producer, consumer) = RingBuffer::<f32>::new(RING);
        let stream = match config.sample_format() {
            SampleFormat::F32 => {
                input_stream::<f32>(&input, stream_config(host, config), tx.clone(), producer)
            }
            SampleFormat::I16 => {
                input_stream::<i16>(&input, stream_config(host, config), tx.clone(), producer)
            }
            SampleFormat::U16 => {
                input_stream::<u16>(&input, stream_config(host, config), tx.clone(), producer)
            }
            SampleFormat::I32 => {
                input_stream::<i32>(&input, stream_config(host, config), tx.clone(), producer)
            }
            _ => return None,
        }
        .ok()?;
        stream.play().ok()?;
        capture_consumer = Some(consumer);
        Some(stream)
    });
    Ok((
        Devices {
            _input: input,
            _output: stream,
            capture: capture_consumer,
            playback,
            underruns,
        },
        Route {
            inputs,
            outputs,
            input: input_id,
            output: output_id,
            echo,
        },
    ))
}

/// Ask the PulseAudio API (native or pipewire-pulse) for the default node.
#[cfg(target_os = "linux")]
fn resolve_pulse_default(input: bool) -> Option<(String, String)> {
    if !std::env::var_os("PULSE_SERVER").is_some_and(|server| !server.is_empty())
        && !std::env::var_os("XDG_RUNTIME_DIR")
            .is_some_and(|runtime| std::path::Path::new(&runtime).join("pulse/native").exists())
    {
        return None;
    }
    let host = cpal::host_from_id(cpal::HostId::PulseAudio).ok()?;
    let device = if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }?;
    Some((device_id(&device)?, device_name(&device)))
}

fn error_callback(tx: SyncSender<Event>) -> impl FnMut(cpal::Error) + Send + 'static {
    move |error| {
        if !matches!(
            error.kind(),
            cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied
        ) {
            let _ = tx.try_send(Event::DeviceError(error.to_string()));
        }
    }
}

/// Downmix and resample to 48 kHz mono into the capture ring. Runs on the
/// device thread: no locks, allocation or blocking.
fn input_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: SyncSender<Event>,
    mut ring: Producer<f32>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    if channels == 0 || channels > 32 || !(8000..=384_000).contains(&rate) {
        return Err("unsupported input configuration".into());
    }
    let mut previous = 0.;
    let mut accumulator = 0_u64;
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                for chunk in data.chunks_exact(channels) {
                    let sample = chunk
                        .iter()
                        .map(|value| f32::from_sample(*value))
                        .sum::<f32>()
                        / channels as f32;
                    let sample = if sample.is_finite() {
                        sample.clamp(-1., 1.)
                    } else {
                        0.
                    };
                    accumulator += u64::from(RATE);
                    while accumulator >= u64::from(rate) {
                        accumulator -= u64::from(rate);
                        let fraction = 1. - accumulator as f32 / RATE as f32;
                        // A full ring means the worker stalled; dropping is better than blocking.
                        let _ = ring.push(previous + (sample - previous) * fraction);
                    }
                    previous = sample;
                }
            },
            error_callback(tx),
            None,
        )
        .map_err(|error| error.to_string())
}

/// Resample the mixed 48 kHz playback ring to the device. Underruns play silence.
fn output_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: SyncSender<Event>,
    mut ring: Consumer<f32>,
    underruns: Arc<AtomicU32>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    if channels == 0 || channels > 32 || !(8000..=384_000).contains(&rate) {
        return Err("unsupported output configuration".into());
    }
    let step = f64::from(RATE) / f64::from(rate);
    let mut position = 0.;
    let mut previous = 0.;
    let mut next = 0.;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let mut dry = false;
                for frame in data.chunks_exact_mut(channels) {
                    let sample = previous + (next - previous) * position as f32;
                    frame.fill(T::from_sample(sample));
                    position += step;
                    while position >= 1. {
                        position -= 1.;
                        previous = next;
                        next = ring.pop().unwrap_or_else(|_| {
                            dry = true;
                            0.
                        });
                    }
                }
                if dry {
                    // Lock-free; the worker thread reads and resets it.
                    underruns.fetch_add(1, Ordering::Relaxed);
                }
            },
            error_callback(tx),
            None,
        )
        .map_err(|error| error.to_string())
}

fn stream_config(host: &cpal::Host, supported: cpal::SupportedStreamConfig) -> cpal::StreamConfig {
    let mut config: cpal::StreamConfig = supported.into();
    // PulseAudio's default can buffer two seconds. These Linux backends control
    // per-stream buffers, without changing a system device's global settings.
    #[cfg(target_os = "linux")]
    if matches!(host.id(), cpal::HostId::PulseAudio | cpal::HostId::PipeWire) {
        config.buffer_size = cpal::BufferSize::Fixed(480);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = host;
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> AudioDevice {
        AudioDevice {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn recognises_pipewire_and_pulseaudio_echo_cancel_nodes() {
        assert!(is_echo_cancelled(
            "pulseaudio:echo-cancel-source",
            "Echo-Cancel Source"
        ));
        assert!(is_echo_cancelled(
            "pulseaudio:alsa_input.usb-mic.echo-cancel",
            "USB Mic (echo cancelled with Speakers)"
        ));
        assert!(!is_echo_cancelled(
            "pulseaudio:alsa_input.pci",
            "Built-in Audio Analog Stereo"
        ));
    }

    #[test]
    fn routes_playback_to_the_matching_canceller_reference() {
        let sinks = vec![
            device("pulseaudio:alsa_output.pci", "Speakers"),
            device("pulseaudio:echo-cancel-sink", "Echo-Cancel Sink"),
        ];
        assert_eq!(
            paired_reference("pulseaudio:echo-cancel-source", &sinks).map(|d| d.id.as_str()),
            Some("pulseaudio:echo-cancel-sink")
        );
        let pulse = vec![
            device("pulseaudio:alsa_output.pci", "Speakers"),
            device(
                "pulseaudio:alsa_output.pci.echo-cancel",
                "Speakers (echo cancelled with Mic)",
            ),
        ];
        assert_eq!(
            paired_reference("pulseaudio:alsa_input.usb.echo-cancel", &pulse)
                .map(|d| d.id.as_str()),
            Some("pulseaudio:alsa_output.pci.echo-cancel")
        );
        assert!(paired_reference("pulseaudio:alsa_input.usb", &sinks[..1]).is_none());
    }
}
