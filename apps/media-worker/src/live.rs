// SPDX-License-Identifier: EUPL-1.2
//! Live audio and all codec/DSP state stay in this process, without server secrets.
//!
//! The device callbacks only move samples through lock-free single-producer
//! rings; they never lock, allocate or block. This thread decodes, buffers,
//! mixes, cancels echo, encodes and talks to the supervisor.
use crate::{
    adaptive::{Controller, WINDOW},
    capture_queue::CaptureQueue,
    dsp::{CapturePipeline, FRAME, RATE},
    mixer::{Mixer, Voice},
    playback::PlaybackConverter,
    probe::EchoProbe,
    reference::{self, Clock, Played, ReferenceConverter, Stamper, Step, Timeline},
    sounds::Effects,
    timing::Timing,
};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use nuschl_core::audio::{AudioDevice, AudioSettings, EchoMode, EchoSource};
use nuschl_ipc::{Envelope, LiveMediaEvent, LiveMediaRequest, read_frame, write_frame};
use opus_pure::{Application, MAX_PACKET_BYTES, OpusEncoder};
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    collections::VecDeque,
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
/// The canceller's initial delay guess: device buffering on both sides plus
/// the acoustic path. Far-end audio reaches it when the device takes it from
/// Nuschl's ring, so the ring's own fill is not part of the delay.
const ECHO_DELAY_HINT_MS: i32 = 30;

const LEVEL_EVERY: u32 = 10;

/// Call a stream builder with the sample type of `format`.
macro_rules! by_format {
    ($format:expr, $build:ident($($argument:expr),* $(,)?)) => {
        match $format {
            SampleFormat::F32 => $build::<f32>($($argument),*),
            SampleFormat::I16 => $build::<i16>($($argument),*),
            SampleFormat::U16 => $build::<u16>($($argument),*),
            SampleFormat::I32 => $build::<i32>($($argument),*),
            format => Err(format!("unsupported sample format {format}")),
        }
    };
}

enum Event {
    Request(LiveMediaRequest),
    DeviceError(String),
    Closed,
}

struct Devices {
    _input: Option<cpal::Stream>,
    _output: cpal::Stream,
    /// The recording of the output device used as the canceller's reference.
    _reference: Option<cpal::Stream>,
    capture: Option<Consumer<f32>>,
    capture_timeline: Timeline,
    playback: Producer<[f32; 2]>,
    /// Output callbacks that found the ring empty; written by the device thread.
    underruns: Arc<AtomicU32>,
    /// What the speakers play, for the canceller (while it is in use).
    reference: Option<Reference>,
}

/// The canceller's reference stream and how far it has been fed.
struct Reference {
    samples: Consumer<f32>,
    timeline: Timeline,
    channels: usize,
    index: u64,
}

struct Transmit {
    encoder: OpusEncoder,
    queue: CaptureQueue,
    packet: [u8; MAX_PACKET_BYTES],
    sequence: u64,
    active: bool,
}

impl Transmit {
    /// Encode exactly 20 ms of already-processed, currently authorized PCM.
    fn encode(&mut self, samples: &[f32; 2 * FRAME]) -> Option<Vec<u8>> {
        let max_bytes = (self.encoder.bitrate_bps as usize / 400).min(self.packet.len());
        let count = self
            .encoder
            .encode(samples, 2 * FRAME, &mut self.packet[..max_bytes])
            .ok()?;
        Some(self.packet[..count].to_vec())
    }
}

/// `--probe-route`: open the default (or configured) devices once, as a call
/// would, and print what Nuschl found upstream of the microphone. A
/// diagnostic for users: it opens the microphone briefly and sends nothing.
pub fn probe_route() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, _rx) = mpsc::sync_channel(64);
    let (_devices, route) = open_devices(
        tx,
        true,
        &AudioSettings::default(),
        None,
        &CALL,
        &Timing::new(),
    )?;
    let report = serde_json::json!({
        "input": route.input,
        "output": route.output,
        "echo_cancellation": route.echo,
        "noise_suppression_upstream": route.noise,
        "echo_reference": route.reference,
        "inputs": route.inputs,
        "outputs": route.outputs,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    // Dropping native streams can block; the OS closes them with the process.
    std::process::exit(0)
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let timing = Timing::new();
    timing.mark("worker_start");
    let (mut first_processed, mut first_transmit, mut first_packet) = (true, true, true);
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
    encoder.bitrate_bps = nuschl_core::audio::DEFAULT_VOICE_BITRATE as i32;
    encoder.use_inband_fec = true;
    encoder.packet_loss_perc = 5;
    let mut transmit = Transmit {
        encoder,
        queue: CaptureQueue::default(),
        packet: [0; MAX_PACKET_BYTES],
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
    // Every server's voice, mixed (see mixer.rs).
    let mut mixer = Mixer::default();
    let mut devices: Option<Devices> = None;
    let mut pipeline: Option<CapturePipeline> = None;
    let mut retry_at = Instant::now();
    let mut last_cleanup = Instant::now();
    let mut frames_seen = 0_u32;
    let mut only_host: Option<String> = None;
    let mut controller = Controller::default();
    let mut last_window = Instant::now();
    // Start-up underruns (the device starts before the ring is filled) are not a signal.
    let mut skip_window = true;
    let mut capture_frame = [0.; FRAME];
    let mut mix = [[0.; 2]; FRAME];
    let mut mono_effects = [0.; FRAME];
    // Local test mode (Settings): capture and process, but never encode or send.
    let mut preview = false;
    let mut loopback = false;
    let mut looped = VecDeque::<f32>::with_capacity(RING);
    let mut probe: Option<EchoProbe> = None;
    // Microphone frames processed since the devices opened, and a buffer for
    // one reference frame (up to two channels).
    let mut mic_index = 0_u64;
    let mut reference_frame = [0_f32; 2 * FRAME];
    let mut far_mono = [0_f32; FRAME];
    // A noise suppressor upstream of the microphone (e.g. RNNoise) replaces ours.
    let mut upstream_noise = false;
    let mut effects = Effects::new();
    loop {
        if devices.is_none() && state_known && !deafened && Instant::now() >= retry_at {
            let labels = if preview { &CHECK } else { &CALL };
            match open_devices(
                tx.clone(),
                !muted,
                &settings,
                only_host.as_deref(),
                labels,
                &timing,
            ) {
                Ok((opened, route)) => {
                    // Without a microphone the canceller has nothing to do.
                    let echo = opened.capture.is_some() && matches!(route.echo, EchoSource::Nuschl);
                    upstream_noise = route.noise.is_some();
                    let channels = opened
                        .reference
                        .as_ref()
                        .map_or(1, |reference| reference.channels);
                    let mut chain = CapturePipeline::with_reference(
                        &chain_settings(&settings, upstream_noise),
                        echo,
                        channels,
                    );
                    chain.set_delay_hint(ECHO_DELAY_HINT_MS);
                    pipeline = Some(chain);
                    timing.mark("pipeline_ready");
                    timing.report_capture();
                    let message = if muted {
                        "Audio ready · microphone muted".to_string()
                    } else if opened.capture.is_some() {
                        match (&route.echo, &route.noise) {
                            (EchoSource::Upstream(name), _) => {
                                format!("Audio ready · system echo cancellation ({name})")
                            }
                            (_, Some(name)) => {
                                format!("Audio ready · system noise suppression ({name})")
                            }
                            (EchoSource::Nuschl, None) => {
                                "Audio ready · echo cancellation on".into()
                            }
                            (EchoSource::None, None) => "Audio ready".into(),
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
                            noise: route.noise,
                            reference: route.reference,
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
                    mic_index = 0;
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
        // Open devices are serviced every few milliseconds; without them the
        // worker waits for the supervisor or its next attempt to open them.
        let timeout = if devices.is_some() {
            Duration::from_millis(4)
        } else if state_known && !deafened {
            retry_at.saturating_duration_since(Instant::now())
        } else {
            Duration::MAX
        };
        let event = match rx.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(_) => break,
        };
        match event {
            Some(Event::Closed | Event::Request(LiveMediaRequest::Shutdown)) => break,
            Some(Event::DeviceError(message)) => {
                transmit.queue.reset();
                if transmit.active {
                    finish(&mut output, &mut transmit, epoch)?;
                }
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
                timing.mark(if mute || deaf {
                    "set_state_muted"
                } else {
                    "set_state_unmuted"
                });
                let was_transmitting = transmit.active;
                if new_epoch != epoch || mute || deaf {
                    transmit.queue.reset();
                }
                let capture_changed =
                    state_known && ((mute || deaf) != muted || (new_epoch != epoch && !muted));
                muted = mute || deaf;
                epoch = new_epoch;
                deafened = deaf;
                state_known = true;
                if deaf {
                    mixer.clear();
                }
                if was_transmitting && muted {
                    // Audio captured before this request arrived may already
                    // be after the user muted: the terminator is silence.
                    transmit.queue.reset();
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
                host,
            })) => {
                timing.mark("configure");
                only_host = host;
                transmit.encoder.bitrate_bps = bitrate.clamp(
                    nuschl_core::audio::MIN_VOICE_BITRATE,
                    nuschl_core::audio::DEFAULT_VOICE_BITRATE,
                ) as i32;
                transmit.encoder.packet_loss_perc = i32::from(packet_loss_percent.clamp(1, 30));
                let requested = requested.sanitized();
                if !requested.same_route(&settings) && devices.is_some() {
                    // A new route needs new streams; the supervisor restarts this process.
                    emit(
                        &mut output,
                        LiveMediaEvent::Status {
                            ready: false,
                            message: "Switching audio devices…".into(),
                        },
                    )?;
                    std::process::exit(0);
                }
                if requested != settings {
                    transmit.queue.reset();
                    if transmit.active {
                        finish(&mut output, &mut transmit, epoch)?;
                    }
                }
                if requested != settings
                    && let Some(chain) = pipeline.as_mut()
                {
                    // The canceller and filters keep what they learned.
                    chain.configure(&chain_settings(&requested, upstream_noise));
                }
                settings = requested;
            }
            Some(Event::Request(LiveMediaRequest::Preview {
                capture,
                loopback: play_back,
            })) => {
                if state_known && capture == muted && devices.is_some() {
                    // Opening or closing the microphone needs a new process (see SetState).
                    emit(
                        &mut output,
                        LiveMediaEvent::Status {
                            ready: false,
                            message: "Restarting audio…".into(),
                        },
                    )?;
                    std::process::exit(0);
                }
                transmit.queue.reset();
                preview = true;
                muted = !capture;
                deafened = false;
                state_known = true;
                loopback = play_back;
                if !loopback {
                    looped.clear();
                }
            }
            Some(Event::Request(LiveMediaRequest::EchoTest)) => {
                if devices.as_ref().is_some_and(|open| open.capture.is_some()) {
                    probe = Some(EchoProbe::new());
                }
            }
            Some(Event::Request(LiveMediaRequest::PushToTalk { active })) => push_to_talk = active,
            Some(Event::Request(LiveMediaRequest::PlaySound { sound, volume })) => {
                effects.play(sound, volume, Instant::now());
            }
            Some(Event::Request(LiveMediaRequest::Playback {
                stream,
                sender,
                sequence,
                opus,
                end,
                volume,
            })) => {
                if deafened {
                    continue;
                }
                let voice = Voice {
                    stream,
                    sender,
                    sequence,
                    opus,
                    end,
                    volume,
                };
                mixer.push(voice, Instant::now())?;
            }
            Some(Event::Request(LiveMediaRequest::EndStream { stream })) => {
                mixer.end_stream(stream)
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
            mix.fill([0.; 2]);
            mono_effects.fill(0.);
            mixer.mix(&mut mix);
            if loopback {
                for (out, sample) in mono_effects
                    .iter_mut()
                    .zip(looped.drain(..looped.len().min(FRAME)))
                {
                    *out += sample;
                }
            }
            if let Some(probe) = probe.as_mut() {
                probe.render(&mut mono_effects);
            }
            // Effects join the output here, so the canceller's reference (a
            // recording of the device, or what this ring hands it) has them.
            effects.mix(&mut mono_effects, Instant::now());
            for (frame, effect) in mix.iter_mut().zip(&mono_effects) {
                for sample in frame {
                    *sample = (*sample + effect).clamp(-1., 1.);
                }
            }
            let (pushed, _) = open.playback.push_partial_slice(&mix);
            if pushed.len() < FRAME {
                break;
            }
            // With a reference recording, far-end activity is judged from it
            // (all applications) when it reaches the canceller, below.
            if open.reference.is_none() {
                // Activity detection must not cancel antiphase LR; this is not
                // the AEC reference, which keeps the actual two channels below.
                for (mono, frame) in far_mono.iter_mut().zip(&mix) {
                    *mono = if frame[0].abs() >= frame[1].abs() {
                        frame[0]
                    } else {
                        frame[1]
                    };
                }
                controller.observe_render(&far_mono);
            }
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
            if let Some(health) = controller.window(residual, underruns) {
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
        // Capture: process every complete 10 ms frame, each after the
        // reference frames that sounded up to it (see reference.rs).
        let Some(capture) = open.capture.as_mut() else {
            continue;
        };
        while capture.slots() >= FRAME {
            let Some(chain) = pipeline.as_mut() else {
                break;
            };
            let mic_time = open.capture_timeline.time_of(mic_index);
            if let (Some(reference), Some(mic_time)) = (open.reference.as_mut(), mic_time) {
                let width = FRAME * reference.channels;
                let mut hold = false;
                loop {
                    let available = reference.samples.slots() >= width;
                    let time = reference.timeline.time_of(reference.index);
                    match reference::step(time, available, mic_time) {
                        decision @ (Step::Feed | Step::Drop) => {
                            let Ok(chunk) = reference.samples.read_chunk(width) else {
                                break;
                            };
                            let (first, second) = chunk.as_slices();
                            reference_frame[..first.len()].copy_from_slice(first);
                            reference_frame[first.len()..width].copy_from_slice(second);
                            chunk.commit_all();
                            reference.index += FRAME as u64;
                            if decision == Step::Feed {
                                let frame = &reference_frame[..width];
                                chain.render(frame);
                                for (mono, samples) in far_mono
                                    .iter_mut()
                                    .zip(frame.chunks_exact(reference.channels))
                                {
                                    // Activity is not an acoustic downmix: opposite
                                    // LR phases must not conceal far-end playback.
                                    *mono = samples
                                        .iter()
                                        .copied()
                                        .max_by(|a, b| a.abs().total_cmp(&b.abs()))
                                        .unwrap_or(0.);
                                }
                                controller.observe_render(&far_mono);
                            }
                        }
                        Step::Later => break,
                        Step::Wait => {
                            // The reference lags: hold the microphone briefly
                            // rather than give the canceller its echo first.
                            hold = capture.slots() < reference::MAX_HOLD_FRAMES * FRAME;
                            break;
                        }
                    }
                }
                if hold {
                    break;
                }
            }
            if capture.pop_entire_slice(&mut capture_frame).is_err() {
                break;
            }
            mic_index += FRAME as u64;
            timing.report_capture();
            if first_processed {
                timing.mark("first_processed_pcm");
                first_processed = false;
            }
            let raw = capture_frame;
            chain.set_far_end(controller.far_active());
            let result = chain.process(&mut capture_frame, push_to_talk, controller.guard_gate());
            controller.observe_capture(&raw, result.voice);
            frames_seen = frames_seen.wrapping_add(1);
            // Only Settings shows a meter.
            if preview && frames_seen.is_multiple_of(LEVEL_EVERY) {
                emit(
                    &mut output,
                    LiveMediaEvent::Level {
                        level_db: result.level_db,
                        voice: result.voice,
                        transmitting: result.transmit,
                    },
                )?;
            }
            if let Some(report) = probe
                .as_mut()
                .and_then(|probe| probe.observe(&raw, &capture_frame, result.transmit))
            {
                probe = None;
                emit(
                    &mut output,
                    LiveMediaEvent::EchoTest {
                        echo_db: report.echo_db,
                        reduction_db: report.reduction_db,
                        transmitted: report.transmitted,
                    },
                )?;
            }
            if loopback && result.transmit {
                // Play back what others would hear; never more than 200 ms behind.
                looped.extend(capture_frame);
                let excess = looped.len().saturating_sub(RING / 2);
                looped.drain(..excess);
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
            if muted || preview {
                continue;
            }
            if result.transmit && first_transmit {
                timing.mark("first_open_gate");
                first_transmit = false;
            }
            transmit.queue.push(
                capture_frame,
                result.transmit,
                settings.transmit == nuschl_core::audio::TransmitMode::VoiceActivity
                    && !controller.guard_gate(),
            );
        }
        if let Some(packet) = transmit.queue.due(Instant::now()) {
            if let Some(opus) = transmit.encode(&packet.samples) {
                if first_packet {
                    timing.mark("first_packet");
                    first_packet = false;
                }
                emit(
                    &mut output,
                    LiveMediaEvent::Packet {
                        epoch,
                        sequence: transmit.sequence,
                        opus,
                        end: packet.end,
                    },
                )?;
                transmit.active = !packet.end;
                if packet.end {
                    let _ = transmit.encoder.reset_state();
                }
            }
            transmit.sequence += 2;
        }
        if last_cleanup.elapsed() > Duration::from_secs(2) {
            mixer.cleanup(Instant::now());
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

/// Close a transmission with a terminator packet so receivers stop promptly.
fn finish(
    output: &mut impl io::Write,
    transmit: &mut Transmit,
    epoch: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    transmit.queue.reset();
    emit(
        output,
        LiveMediaEvent::Packet {
            epoch,
            sequence: transmit.sequence,
            opus: nuschl_core::audio::SILENT_OPUS.to_vec(),
            end: true,
        },
    )?;
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
    noise: Option<String>,
    /// The output recorded as the canceller's reference; `None` when Nuschl
    /// cancels only its own playback (or does not cancel at all).
    reference: Option<String>,
}

/// The processing Nuschl applies itself: a noise suppressor upstream of the
/// microphone (e.g. an RNNoise filter-chain) replaces Nuschl's own.
fn chain_settings(settings: &AudioSettings, upstream_noise: bool) -> AudioSettings {
    let mut settings = settings.clone();
    settings.noise_suppression &= !upstream_noise;
    settings
}

/// What volume mixers (pavucontrol, GNOME Settings, Helvum) call the streams.
struct Labels {
    microphone: &'static str,
    playback: &'static str,
}

const CALL: Labels = Labels {
    microphone: "Microphone",
    playback: "Voice chat",
};
/// The audio check in Settings: echo test and the processed voice played back.
const CHECK: Labels = Labels {
    microphone: "Microphone test",
    playback: "Audio test",
};

fn open_devices(
    tx: SyncSender<Event>,
    capture: bool,
    settings: &AudioSettings,
    only_host: Option<&str>,
    labels: &Labels,
    timing: &Timing,
) -> Result<(Devices, Route), String> {
    let mut hosts = vec![];
    #[cfg(target_os = "linux")]
    {
        let forced = std::env::var("NUSCHL_AUDIO_BACKEND").ok();
        match forced.as_deref().or(only_host) {
            Some("pulse" | "pulseaudio") => hosts.push(cpal::HostId::PulseAudio),
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
    {
        let _ = only_host;
        hosts.push(cpal::default_host().id());
    }
    let mut errors = Vec::new();
    for id in hosts {
        timing.mark("host_connect_begin");
        match connect(id).and_then(|mut host| {
            timing.mark("host_connected");
            open_host(&mut host, tx.clone(), capture, settings, labels, timing)
        }) {
            Ok(opened) => return Ok(opened),
            Err(error) => {
                // Diagnostics only: stderr never carries protocol frames.
                eprintln!("audio host {id:?} unavailable: {error}");
                errors.push(format!("{id:?}: {error}"));
            }
        }
    }
    Err(errors.join("; "))
}

/// Connect to an audio server as Nuschl, the application mixers show.
fn connect(id: cpal::HostId) -> Result<cpal::Host, String> {
    #[cfg(target_os = "linux")]
    let host = {
        let application = || cpal::ApplicationInfo {
            name: "Nuschl".into(),
            icon_name: Some("org.nuschl.Nuschl".into()),
        };
        match id {
            cpal::HostId::PipeWire => {
                cpal::platform::PipeWireHost::with_application(application()).map(cpal::Host::from)
            }
            cpal::HostId::PulseAudio => {
                cpal::platform::PulseAudioHost::with_application(application())
                    .map(cpal::Host::from)
            }
            id => cpal::host_from_id(id),
        }
    };
    #[cfg(not(target_os = "linux"))]
    let host = cpal::host_from_id(id);
    host.map_err(|error| error.to_string())
}

/// Name the streams built next from `host`'s devices. PipeWire also gets the
/// communication role, as stock Mumble sets; PulseAudio's equivalent
/// ("phone") would pause music for as long as Nuschl is in a call
/// (module-role-cork), so it is left out there.
fn label(host: &mut cpal::Host, name: &str) {
    #[cfg(target_os = "linux")]
    {
        let metadata = |role| cpal::StreamMetadata {
            name: Some(name.into()),
            role,
        };
        // Names come from Nuschl's constants and device names: a NUL byte
        // (refused) only costs the label.
        let _ = match host.as_inner_mut() {
            cpal::platform::HostInner::PipeWire(host) => {
                host.set_stream_metadata(metadata(Some(cpal::StreamRole::Communication)))
            }
            cpal::platform::HostInner::PulseAudio(host) => host.set_stream_metadata(metadata(None)),
            _ => Ok(()),
        };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (host, name);
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
    host: &mut cpal::Host,
    tx: SyncSender<Event>,
    capture: bool,
    settings: &AudioSettings,
    labels: &Labels,
    timing: &Timing,
) -> Result<(Devices, Route), String> {
    let all: Vec<cpal::Device> = host
        .devices()
        .map(|devices| devices.collect())
        .unwrap_or_default();
    // PipeWire's node list also contains other applications' streams, sinks
    // offered as (monitor) inputs, and cpal's virtual default entries. The
    // PulseAudio API (pipewire-pulse) knows which nodes are real sinks and
    // sources, so the PipeWire lists keep only those when it is available.
    #[cfg(target_os = "linux")]
    let pipewire = host.id() == cpal::HostId::PipeWire;
    #[cfg(not(target_os = "linux"))]
    let pipewire = false;
    #[cfg(target_os = "linux")]
    let pulse_nodes = pipewire.then(pulse_device_names).flatten();
    #[cfg(not(target_os = "linux"))]
    let pulse_nodes: Option<(Vec<String>, Vec<String>)> = None;
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
                let name = backend_name(&id);
                // Sink monitors are listed as sources by PulseAudio; they are not microphones.
                let keep = if !pipewire {
                    !(input && name.ends_with(".monitor"))
                } else if let Some((sources, sinks)) = &pulse_nodes {
                    let real = if input { sources } else { sinks };
                    real.iter().any(|node| node == name)
                } else {
                    let duplex = device.supports_input() && device.supports_output();
                    !matches!(name, "input_default" | "output_default" | "sink_default")
                        && !(input && duplex)
                };
                keep.then(|| AudioDevice {
                    name: device_name(device),
                    id,
                })
            })
            .take(128)
            .collect()
    };
    timing.mark("devices_listed");
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
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut input_id = input_device.as_ref().and_then(device_id);
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
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
    // The microphone opens first: a filter-chain's capture (as in PipeWire's
    // RNNoise configuration) is passive and is linked to its own source only
    // once someone records from the chain.
    // Every callback stamps its samples on this clock (see reference.rs).
    let clock = Clock::new();
    let (capture_stamper, capture_timeline) = reference::stamped();
    let mut capture_consumer = None;
    label(host, labels.microphone);
    let input = capture
        .then(|| input_device.clone())
        .flatten()
        .and_then(|input| {
            timing.mark("capture_create_begin");
            let config = input.default_input_config().ok()?;
            let (producer, consumer) = RingBuffer::<f32>::new(RING);
            let stamp = (capture_stamper, clock);
            let stream = by_format!(
                config.sample_format(),
                input_stream(
                    &input,
                    stream_config(host, config),
                    tx.clone(),
                    producer,
                    stamp,
                    timing.clone()
                )
            )
            .ok()?;
            timing.mark("capture_created");
            stream.play().ok()?;
            timing.mark("capture_play_ready");
            capture_consumer = Some(consumer);
            Some(stream)
        });
    // Processing that already happens before Nuschl: follow PipeWire's graph
    // upstream (mic → echo-cancel → RNNoise → default source), falling back to
    // recognising an echo-cancel source by name (PulseAudio, other systems).
    #[cfg(target_os = "linux")]
    let found = input_id
        .as_ref()
        .map(|id| upstream_processing(backend_name(id), input.is_some()))
        .unwrap_or_default();
    #[cfg(not(target_os = "linux"))]
    let found = crate::graph::Upstream::default();
    timing.mark("graph_inspected");
    let upstream_echo = found.echo.clone().or_else(|| {
        input_id
            .as_ref()
            .is_some_and(|id| is_echo_cancelled(id, &input_name))
            .then(|| input_name.clone())
    });
    let mode = input_id
        .as_ref()
        .map_or(settings.echo_cancellation, |id| settings.echo_for(id));
    // Reported even without capture, so Settings can explain the route before a test.
    let echo = match (mode, &upstream_echo) {
        (EchoMode::Off, _) => EchoSource::None,
        (EchoMode::On, _) => EchoSource::Nuschl,
        (EchoMode::Auto, Some(name)) => EchoSource::Upstream(name.clone()),
        (EchoMode::Auto, None) => EchoSource::Nuschl,
    };
    // With a system canceller, Nuschl's own playback must reach its reference.
    // Only Nuschl's stream is routed; system defaults and other apps are untouched.
    let reference = if settings.output_device.is_none() && upstream_echo.is_some() {
        found
            .reference
            .as_deref()
            .and_then(|name| {
                outputs
                    .iter()
                    .find(|device| backend_name(&device.id) == name)
            })
            .or_else(|| {
                input_id
                    .as_ref()
                    .and_then(|id| paired_reference(id, &outputs))
            })
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
    let (playback, playback_consumer) = RingBuffer::<[f32; 2]>::new(RING);
    let underruns = Arc::new(AtomicU32::new(0));
    // What the output callback hands to the device: the reference where the
    // output device cannot be recorded.
    let (played_samples, played_consumer) = RingBuffer::<f32>::new(2 * reference::RING_FRAMES);
    let (played_stamper, played_timeline) = reference::stamped();
    let played = (
        Played {
            samples: played_samples,
            stamper: played_stamper,
        },
        clock,
    );
    timing.mark("playback_create_begin");
    label(host, labels.playback);
    let stream = by_format!(
        config.sample_format(),
        output_stream(
            &output_device,
            stream_config(host, config),
            tx.clone(),
            (playback_consumer, underruns.clone()),
            played
        )
    )?;
    timing.mark("playback_created");
    stream.play().map_err(|error| error.to_string())?;
    timing.mark("playback_play_ready");
    // Nuschl's canceller needs to know everything the speakers play: record
    // the output device (all applications) where the system allows it, and
    // only while the microphone is open.
    let mut recording = None;
    let mut reference_name = None;
    let reference = if input.is_some() && matches!(echo, EchoSource::Nuschl) {
        timing.mark("reference_create_begin");
        match record_output(
            host,
            &all,
            &output_device,
            output_id.as_deref(),
            tx.clone(),
            clock,
        ) {
            Ok((stream, reference, name)) => {
                timing.mark("reference_play_ready");
                recording = Some(stream);
                reference_name = Some(name);
                Some(reference)
            }
            Err(error) => {
                eprintln!(
                    "output recording unavailable, using Nuschl's playback as the echo reference: {error}"
                );
                Some(Reference {
                    samples: played_consumer,
                    timeline: played_timeline,
                    channels: 2,
                    index: 0,
                })
            }
        }
    } else {
        None
    };
    Ok((
        Devices {
            _input: input,
            _output: stream,
            _reference: recording,
            capture: capture_consumer,
            capture_timeline,
            playback,
            underruns,
            reference,
        },
        Route {
            inputs,
            outputs,
            input: input_id,
            output: output_id,
            echo,
            noise: found.noise,
            reference: reference_name,
        },
    ))
}

/// Walk PipeWire's graph upstream from `source`. While our own capture has
/// just started, passive filter links may still be on their way: wait for
/// them briefly rather than miss a canceller behind a noise suppressor.
#[cfg(target_os = "linux")]
fn upstream_processing(source: &str, capturing: bool) -> crate::graph::Upstream {
    let mut found = crate::graph::Upstream::default();
    for attempt in 0..8 {
        let Some(graph) = crate::graph::snapshot() else {
            break;
        };
        found = graph.upstream(source);
        if !(capturing && found.dangling) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50 + attempt * 25));
    }
    found
}

/// Whether a PulseAudio server (native or pipewire-pulse) may be listening,
/// checked before cpal waits for a connection.
#[cfg(target_os = "linux")]
fn pulse_running() -> bool {
    std::env::var_os("PULSE_SERVER").is_some_and(|server| !server.is_empty())
        || std::env::var_os("XDG_RUNTIME_DIR")
            .is_some_and(|runtime| std::path::Path::new(&runtime).join("pulse/native").exists())
}

/// Real sources (without monitors) and sinks by node name, from the PulseAudio
/// API (pipewire-pulse) when it is running.
#[cfg(target_os = "linux")]
fn pulse_device_names() -> Option<(Vec<String>, Vec<String>)> {
    if !pulse_running() {
        return None;
    }
    let host = connect(cpal::HostId::PulseAudio).ok()?;
    let (mut sources, mut sinks) = (Vec::new(), Vec::new());
    for device in host.devices().ok()? {
        let Some(id) = device_id(&device) else {
            continue;
        };
        let name = backend_name(&id).to_string();
        if device.supports_input() && !name.ends_with(".monitor") {
            sources.push(name.clone());
        }
        if device.supports_output() {
            sinks.push(name);
        }
    }
    Some((sources, sinks))
}

/// Ask the PulseAudio API (native or pipewire-pulse) for the default node.
#[cfg(target_os = "linux")]
fn resolve_pulse_default(input: bool) -> Option<(String, String)> {
    if !pulse_running() {
        return None;
    }
    let host = connect(cpal::HostId::PulseAudio).ok()?;
    let device = if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }?;
    Some((device_id(&device)?, device_name(&device)))
}

/// The channel count and rate of a device format the converters handle.
fn convertible(config: &cpal::StreamConfig) -> Result<(usize, u32), String> {
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    if channels == 0 || channels > 32 || !(8000..=384_000).contains(&rate) {
        return Err(format!(
            "unsupported device format: {channels} channels at {rate} Hz"
        ));
    }
    Ok((channels, rate))
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
    (mut stamper, clock): (Stamper, Clock),
    timing: Timing,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let (channels, rate) = convertible(&config)?;
    let mut converter = CaptureConverter::new(channels, rate);
    device
        .build_input_stream(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                if !data.is_empty() {
                    timing.capture();
                }
                let timestamp = info.timestamp();
                let latency = timestamp
                    .callback
                    .saturating_duration_since(timestamp.capture);
                stamper.anchor(clock.now_shifted(latency, Duration::ZERO));
                stamper.advance(converter.push(data, &mut ring));
            },
            error_callback(tx),
            None,
        )
        .map_err(|error| error.to_string())
}

/// Record what an output device plays (all applications), as the echo
/// canceller's reference: PipeWire captures the sink, PulseAudio records its
/// monitor source, WASAPI and Core Audio (process tap) record in loopback.
fn record_output(
    host: &mut cpal::Host,
    all: &[cpal::Device],
    output: &cpal::Device,
    output_id: Option<&str>,
    tx: SyncSender<Event>,
    clock: Clock,
) -> Result<(cpal::Stream, Reference, String), String> {
    #[cfg(target_os = "linux")]
    if host.id() == cpal::HostId::Alsa {
        return Err("ALSA cannot record an output device".into());
    }
    // A readable name for what is recorded; the default sink's own name where
    // cpal only offers a virtual "default" entry.
    #[cfg(target_os = "linux")]
    let name = (output_id
        .map(backend_name)
        .is_none_or(|name| name == "output_default"))
    .then(|| resolve_pulse_default(false).map(|(_, name)| name))
    .flatten()
    .unwrap_or_else(|| device_name(output));
    #[cfg(not(target_os = "linux"))]
    let name = device_name(output);
    let description = format!("Echo reference (what {name} plays)");
    label(host, &description);
    #[cfg(target_os = "linux")]
    let pulse_monitor = (host.id() == cpal::HostId::PulseAudio)
        .then(|| {
            let sink = output_id
                .map(backend_name)
                .map(str::to_string)
                .or_else(|| {
                    resolve_pulse_default(false).map(|(id, _)| backend_name(&id).to_string())
                })?;
            let monitor = format!("{sink}.monitor");
            // A connection of its own: cpal's PulseAudio client stops a stream
            // playing into a sink while the same connection records its monitor.
            let mut separate = connect(cpal::HostId::PulseAudio).ok()?;
            label(&mut separate, &description);
            let devices: Vec<cpal::Device> = separate.devices().ok()?.collect();
            devices.into_iter().find(|device| {
                device.supports_input()
                    && device_id(device).as_deref().map(backend_name) == Some(monitor.as_str())
            })
        })
        .flatten();
    // PipeWire: a chosen sink is recorded directly (cpal sets
    // stream.capture.sink); the default output through cpal's "sink_default".
    #[cfg(target_os = "linux")]
    let pipewire_sink = (host.id() == cpal::HostId::PipeWire
        && output_id
            .map(backend_name)
            .is_none_or(|name| name == "output_default"))
    .then(|| {
        all.iter()
            .find(|device| device_id(device).as_deref().map(backend_name) == Some("sink_default"))
            .cloned()
    })
    .flatten();
    #[cfg(not(target_os = "linux"))]
    let (pulse_monitor, pipewire_sink): (Option<cpal::Device>, Option<cpal::Device>) = {
        let _ = (all, output_id);
        (None, None)
    };
    let device = pulse_monitor
        .or(pipewire_sink)
        .unwrap_or_else(|| output.clone());
    // On Linux only a sink (or its monitor) may be recorded here: never risk
    // treating a microphone as the speakers. Windows and macOS record output
    // devices in loopback.
    #[cfg(target_os = "linux")]
    let config = {
        let id = device_id(&device).unwrap_or_default();
        let sink_like = backend_name(&id).ends_with(".monitor")
            || backend_name(&id) == "sink_default"
            || device.supports_output();
        if !sink_like || !device.supports_input() {
            return Err(format!("{id} cannot be recorded as an output"));
        }
        device
            .default_input_config()
            .map_err(|error| error.to_string())?
    };
    #[cfg(not(target_os = "linux"))]
    let config = device
        .default_input_config()
        .or_else(|_| output.default_output_config())
        .map_err(|error| error.to_string())?;
    let format = config.sample_format();
    let stream_config = stream_config(host, config);
    let (channels, rate) = convertible(&stream_config)?;
    let mut converter = ReferenceConverter::new(channels, rate);
    let outputs = converter.outputs();
    let (mut ring, samples) = RingBuffer::<f32>::new(reference::RING_FRAMES * outputs);
    let (mut stamper, timeline) = reference::stamped();
    let callback = move |values: &mut dyn Iterator<Item = f32>, info: &cpal::InputCallbackInfo| {
        let timestamp = info.timestamp();
        let latency = timestamp
            .callback
            .saturating_duration_since(timestamp.capture);
        stamper.anchor(clock.now_shifted(latency, Duration::ZERO));
        stamper.advance(converter.push(values, &mut ring));
    };
    let stream = by_format!(
        format,
        build_recording(&device, stream_config, tx, callback)
    )?;
    stream.play().map_err(|error| error.to_string())?;
    Ok((
        stream,
        Reference {
            samples,
            timeline,
            channels: outputs,
            index: 0,
        },
        name,
    ))
}

fn build_recording<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: SyncSender<Event>,
    mut callback: impl FnMut(&mut dyn Iterator<Item = f32>, &cpal::InputCallbackInfo) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                callback(&mut data.iter().map(|value| f32::from_sample(*value)), info);
            },
            error_callback(tx),
            None,
        )
        .map_err(|error| error.to_string())
}

/// Device-rate, multi-channel capture to 48 kHz mono. Runs on the device
/// thread: it must not lock, allocate or block (see the tests).
struct CaptureConverter {
    channels: usize,
    rate: u32,
    previous: f32,
    accumulator: u64,
}

impl CaptureConverter {
    fn new(channels: usize, rate: u32) -> Self {
        Self {
            channels,
            rate,
            previous: 0.,
            accumulator: 0,
        }
    }

    /// Convert and queue; returns the 48 kHz samples actually queued.
    fn push<T>(&mut self, data: &[T], ring: &mut Producer<f32>) -> u64
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let mut pushed = 0;
        for chunk in data.chunks_exact(self.channels) {
            let sample = chunk
                .iter()
                .map(|value| f32::from_sample(*value))
                .sum::<f32>()
                / self.channels as f32;
            let sample = if sample.is_finite() {
                sample.clamp(-1., 1.)
            } else {
                0.
            };
            self.accumulator += u64::from(RATE);
            while self.accumulator >= u64::from(self.rate) {
                self.accumulator -= u64::from(self.rate);
                let fraction = 1. - self.accumulator as f32 / RATE as f32;
                // A full ring means the worker stalled; dropping is better than blocking.
                if ring
                    .push(self.previous + (sample - self.previous) * fraction)
                    .is_ok()
                {
                    pushed += 1;
                }
            }
            self.previous = sample;
        }
        pushed
    }
}

/// Resample the mixed 48 kHz playback ring to the device. Underruns play silence.
fn output_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: SyncSender<Event>,
    (mut ring, underruns): (Consumer<[f32; 2]>, Arc<AtomicU32>),
    (mut played, clock): (Played, Clock),
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let (channels, rate) = convertible(&config)?;
    let mut converter = PlaybackConverter::new(channels, rate);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                let timestamp = info.timestamp();
                let ahead = timestamp
                    .playback
                    .saturating_duration_since(timestamp.callback);
                played
                    .stamper
                    .anchor(clock.now_shifted(Duration::ZERO, ahead + converter.delay()));
                let (dry, handed) = converter.pull(data, &mut ring, &mut played.samples);
                played.stamper.advance(handed);
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
    #[cfg(target_os = "linux")]
    let buffer = device_buffer(supported.buffer_size());
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut config: cpal::StreamConfig = supported.into();
    // PulseAudio's default can buffer two seconds. These Linux backends control
    // per-stream buffers, without changing a system device's global settings.
    #[cfg(target_os = "linux")]
    if matches!(host.id(), cpal::HostId::PulseAudio | cpal::HostId::PipeWire) {
        config.buffer_size = buffer;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = host;
    config
}

/// About 10 ms of device frames, within what the device accepts. PipeWire
/// refuses streams outside its quantum range, which is raised in virtual
/// machines and by users (e.g. 1024..2048); a fixed request would fail there.
fn device_buffer(supported: &cpal::SupportedBufferSize) -> cpal::BufferSize {
    const WANTED: u32 = 480;
    match supported {
        cpal::SupportedBufferSize::Range { min, max } if min <= max => {
            cpal::BufferSize::Fixed(WANTED.clamp(*min, *max))
        }
        _ => cpal::BufferSize::Fixed(WANTED),
    }
}

#[cfg(test)]
#[path = "output_fixture.rs"]
mod output_fixture;

#[cfg(test)]
mod tests {
    #[test]
    fn real_mono_packets_fit_default_and_reduced_live_targets_without_devices() {
        let mut encoder =
            opus_pure::OpusEncoder::new(48_000, 1, opus_pure::Application::Voip).unwrap();
        encoder.use_inband_fec = true;
        encoder.packet_loss_perc = 5;
        let mut tx = super::Transmit {
            encoder,
            queue: CaptureQueue::default(),
            packet: [0; opus_pure::MAX_PACKET_BYTES],
            sequence: 0,
            active: false,
        };
        let mut decoder = opus_pure::OpusDecoder::new(48_000, 1).unwrap();
        for target in [
            nuschl_core::audio::DEFAULT_VOICE_BITRATE,
            24_000,
            6_000,
            128_000,
        ] {
            tx.encoder.bitrate_bps = target as i32;
            for frame in 0..8 {
                let samples = std::array::from_fn(|i| {
                    (std::f32::consts::TAU * 997. * (frame * 960 + i) as f32 / 48_000.).sin() * 0.3
                });
                let packet = tx.encode(&samples).unwrap();
                assert!(packet.len() <= (target / 400) as usize);
                let mut output = vec![0.; 5760];
                assert_eq!(decoder.decode(&packet, 5760, &mut output).unwrap(), 960);
            }
        }
    }

    use super::*;
    use opus_pure::{MAX_PACKET_SAMPLES, OpusDecoder};

    #[test]
    fn processed_prefix_marker_fixture_uses_the_real_queue_and_encoder() {
        let mut queue = CaptureQueue::default();
        let now = Instant::now();
        let tone = |frame: usize, frequency: f32| {
            std::array::from_fn(|i| {
                (std::f32::consts::TAU * frequency * (frame * FRAME + i) as f32 / 48_000.).sin()
                    * 0.2
            })
        };
        for frame in 0..8 {
            queue.push(tone(frame, 997.), false, true);
            assert!(queue.due(now).is_none(), "a prefix cannot open the gate");
        }
        let mut tx = super::Transmit {
            encoder: OpusEncoder::new(48_000, 1, opus_pure::Application::Voip).unwrap(),
            queue: CaptureQueue::default(),
            packet: [0; opus_pure::MAX_PACKET_BYTES],
            sequence: 0,
            active: false,
        };
        tx.encoder.bitrate_bps = nuschl_core::audio::DEFAULT_VOICE_BITRATE as i32;
        let mut packets = Vec::new();
        for frame in 8..20 {
            queue.push(tone(frame, 3_000.), true, true);
            let at = now + Duration::from_millis((frame as u64 - 8) * 10);
            if let Some(packet) = queue.due(at) {
                packets.push(tx.encode(&packet.samples).unwrap());
            }
        }
        queue.push([0.; FRAME], false, true);
        for frame in 20..40 {
            if let Some(packet) = queue.due(now + Duration::from_millis((frame - 8) * 10)) {
                packets.push(tx.encode(&packet.samples).unwrap());
            }
        }
        assert!(packets.len() >= 10);
        // An independent libopus fixture consumes these public synthetic bytes.
        // No file is produced by ordinary unit tests.
        if let Some(path) = std::env::var_os("NUSCHL_PREFIX_PROOF_OUTPUT") {
            std::fs::write(path, serde_json::to_vec(&serde_json::json!({"packets": packets, "prefix_hz":997, "following_hz":3000, "prefix_samples":8 * FRAME, "sample_rate":48000})).unwrap()).unwrap();
        }
    }

    fn device(id: &str, name: &str) -> AudioDevice {
        AudioDevice {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn device_buffer_stays_inside_the_supported_range() {
        use cpal::{BufferSize, SupportedBufferSize};
        let range = |min, max| SupportedBufferSize::Range { min, max };
        assert_eq!(device_buffer(&range(64, 8192)), BufferSize::Fixed(480));
        assert_eq!(device_buffer(&range(1024, 2048)), BufferSize::Fixed(1024));
        assert_eq!(device_buffer(&range(16, 256)), BufferSize::Fixed(256));
        assert_eq!(
            device_buffer(&SupportedBufferSize::Unknown),
            BufferSize::Fixed(480)
        );
    }

    #[test]
    fn device_callbacks_never_allocate() {
        // Typical device shapes: 44.1 kHz stereo i16 capture, 48 kHz 6-channel
        // f32 playback, and 96 kHz u16 to exercise resampling both ways.
        let (mut capture_ring, mut capture_out) = RingBuffer::<f32>::new(RING);
        let (mut playback_in, mut playback_ring) = RingBuffer::<[f32; 2]>::new(RING);
        let mut capture = CaptureConverter::new(2, 44_100);
        let mut playback = PlaybackConverter::new(6, 48_000);
        let mut resampled = PlaybackConverter::new(1, 96_000);
        let input = [1234_i16; 2 * 441];
        let mut output = [0_f32; 6 * 480];
        let mut output_u16 = [0_u16; 960];
        for _ in 0..10 {
            let _ = playback_in.push_entire_slice(&[[0.25; 2]; FRAME]);
        }
        // The reference paths: what the output callback played, and a stereo
        // recording of the output device, both stamped on the shared clock.
        let (mut played, mut played_out) = RingBuffer::<f32>::new(2 * reference::RING_FRAMES);
        let (mut recorded, mut recorded_out) = RingBuffer::<f32>::new(2 * reference::RING_FRAMES);
        let mut recording = ReferenceConverter::new(2, 44_100);
        let (mut stamper, _timeline) = reference::stamped();
        let clock = Clock::new();
        let device_frames = [0.5_f32; 2 * 441];
        let mut dry = 0;
        let counted = allocation_counter::measure(|| {
            for _ in 0..50 {
                stamper.anchor(clock.now_shifted(Duration::from_millis(5), Duration::ZERO));
                stamper.advance(capture.push(&input, &mut capture_ring));
                while capture_out.pop().is_ok() {}
                let (empty, handed) = playback.pull(&mut output, &mut playback_ring, &mut played);
                dry += usize::from(empty);
                stamper.advance(handed);
                dry += usize::from(
                    resampled
                        .pull(&mut output_u16, &mut playback_ring, &mut played)
                        .0,
                );
                while played_out.pop().is_ok() {}
                recording.push(device_frames.iter().copied(), &mut recorded);
                while recorded_out.pop().is_ok() {}
            }
        });
        assert_eq!(counted.count_total, 0, "a device callback allocated");
        let control = allocation_counter::measure(|| {
            std::hint::black_box(vec![0_u8; 64]);
        });
        assert!(
            control.count_total > 0,
            "the counting allocator is not active"
        );
        assert!(dry > 0, "the test should also cover the underrun path");
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn playback_preserves_stereo_mono_mapping_and_the_actual_fallback_reference() {
        for channels in [1, 2, 6] {
            let (mut input, mut consumer) = RingBuffer::<[f32; 2]>::new(16);
            input.push_entire_slice(&[[0.5, -0.25]; 8]).unwrap();
            let (mut reference, mut samples) = RingBuffer::<f32>::new(32);
            let mut converter = PlaybackConverter::new(channels, 48_000);
            let mut output = vec![0_f32; channels * 6];
            let (dry, handed) = converter.pull(&mut output, &mut consumer, &mut reference);
            assert!(!dry);
            assert_eq!(handed, 6, "reference timestamps count frames, not scalars");
            let last = &output[output.len() - channels..];
            if channels == 1 {
                assert_eq!(last, [0.125]);
            } else {
                assert_eq!(&last[..2], [0.5, -0.25]);
                assert!(last[2..].iter().all(|value| *value == 0.));
            }
            let expected = if channels == 1 {
                [0.125; 2]
            } else {
                [0.5, -0.25]
            };
            for _ in 0..handed {
                assert_eq!([samples.pop().unwrap(), samples.pop().unwrap()], expected);
            }
            assert!(samples.pop().is_err());
        }
    }

    #[test]
    fn the_terminator_sent_on_mute_is_silence() {
        let mut decoder = OpusDecoder::new(48_000, 1).unwrap();
        let mut pcm = [1_f32; MAX_PACKET_SAMPLES];
        let count = decoder
            .decode(
                &nuschl_core::audio::SILENT_OPUS,
                MAX_PACKET_SAMPLES,
                &mut pcm,
            )
            .unwrap();
        assert_eq!(count, 2 * FRAME, "20 ms");
        // Far below one 24-bit step; CELT's float decoder adds a tiny offset
        // against denormals instead of returning exact zeros.
        let peak = pcm[..count]
            .iter()
            .fold(0_f32, |peak, sample| peak.max(sample.abs()));
        assert!(peak < 1e-20, "peak {peak}");
    }

    #[test]
    fn capture_resamples_to_48_khz_mono() {
        let (mut ring, mut out) = RingBuffer::<f32>::new(RING);
        let mut capture = CaptureConverter::new(2, 44_100);
        capture.push(&[0.5_f32, -0.5].repeat(4410), &mut ring);
        assert_eq!(out.slots(), 4800, "100 ms in, 100 ms out");
        assert!(std::iter::from_fn(|| out.pop().ok()).all(|sample| sample.abs() < 1e-6));
        let mut loud = CaptureConverter::new(1, 48_000);
        loud.push(&[f32::NAN, 3.0, -3.0], &mut ring);
        let values: Vec<f32> = std::iter::from_fn(|| out.pop().ok()).collect();
        assert_eq!(
            values,
            [0., 1., -1.],
            "non-finite and out-of-range input is sanitised"
        );
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
