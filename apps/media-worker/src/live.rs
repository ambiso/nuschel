// SPDX-License-Identifier: EUPL-1.2
//! Live audio and all codec/DSP state stay in this process, without server secrets.
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use nuschel_ipc::{Envelope, LiveMediaEvent, LiveMediaRequest, read_frame, write_frame};
use opus_pure::{Application, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OpusDecoder, OpusEncoder};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

enum Event {
    Request(LiveMediaRequest),
    Capture(u64, Vec<f32>),
    DeviceError(String),
    Closed,
}
struct Devices {
    _input: Option<cpal::Stream>,
    _output: cpal::Stream,
}
struct AudioQueue {
    samples: VecDeque<f32>,
    ready: bool,
    end: bool,
}
#[derive(Default)]
struct Mixer {
    speakers: BTreeMap<u32, AudioQueue>,
}
impl Mixer {
    fn next(&mut self) -> f32 {
        let mut mixed = 0.;
        for queue in self.speakers.values_mut() {
            if !queue.ready && (queue.samples.len() >= 1440 || queue.end) {
                queue.ready = true;
            }
            if queue.ready {
                if let Some(sample) = queue.samples.pop_front() {
                    mixed += sample;
                } else {
                    queue.ready = false;
                }
            }
        }
        mixed.clamp(-1., 1.)
    }
    fn push(&mut self, sender: u32, samples: &[f32], volume: f32, end: bool) {
        let queue = self.speakers.entry(sender).or_insert_with(|| AudioQueue {
            samples: VecDeque::new(),
            ready: false,
            end: false,
        });
        if queue.samples.len() + samples.len() > 9600 {
            queue.samples.clear();
            queue.ready = false;
        }
        queue.samples.extend(
            samples
                .iter()
                .map(|sample| (sample * volume).clamp(-1., 1.)),
        );
        queue.end = end;
    }
}
struct Speaker {
    decoder: OpusDecoder,
    expected: u64,
    last_packet: Instant,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::sync_channel(96);
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
    let muted = Arc::new(AtomicBool::new(true));
    let mixer = Arc::new(Mutex::new(Mixer::default()));
    let mut output = io::stdout().lock();
    let mut encoder = OpusEncoder::new(48_000, 1, Application::Voip)?;
    encoder.bitrate_bps = 48_000;
    let mut denoise = nnnoiseless::DenoiseState::new();
    let mut noise_suppression = false;
    let mut deafened = false;
    let mut sequence = 0;
    let mut epoch = 0;
    let mut speakers = BTreeMap::<u32, Speaker>::new();
    let mut devices = None;
    let mut retry_at = Instant::now();
    let mut last_cleanup = Instant::now();
    loop {
        if devices.is_none() && !deafened && Instant::now() >= retry_at {
            match open_devices(tx.clone(), muted.clone(), mixer.clone(), epoch) {
                Ok(opened) => {
                    let message = if muted.load(Ordering::Relaxed) {
                        "Audio ready · microphone muted"
                    } else if opened._input.is_some() {
                        "Audio ready"
                    } else {
                        "Playback ready; microphone unavailable"
                    };
                    emit(
                        &mut output,
                        LiveMediaEvent::Status {
                            ready: true,
                            message: message.into(),
                        },
                    )?;
                    devices = Some(opened);
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
        let event = match rx.recv_timeout(Duration::from_millis(20)) {
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
                let was_muted = muted.swap(mute || deaf, Ordering::Relaxed);
                epoch = new_epoch;
                devices.take();
                retry_at = Instant::now();
                deafened = deaf;
                if deaf && let Ok(mut mixer) = mixer.lock() {
                    mixer.speakers.clear();
                }
                if !was_muted && (mute || deaf) {
                    let mut packet = vec![0; MAX_PACKET_BYTES];
                    let count = encoder.encode(&[0.; 960], 960, &mut packet)?;
                    packet.truncate(count);
                    emit(
                        &mut output,
                        LiveMediaEvent::Packet {
                            epoch,
                            sequence,
                            opus: packet,
                            end: true,
                        },
                    )?;
                    sequence += 2;
                    encoder.reset_state()?;
                }
            }
            Some(Event::Request(LiveMediaRequest::Configure {
                bitrate,
                noise_suppression: suppress,
            })) => {
                encoder.bitrate_bps = bitrate.clamp(6000, 64_000) as i32;
                noise_suppression = suppress;
                denoise = nnnoiseless::DenoiseState::new();
            }
            Some(Event::Request(LiveMediaRequest::Playback {
                sender,
                sequence: frame,
                opus,
                end,
                volume,
            })) => {
                if deafened || opus.is_empty() || opus.len() > 1000 || !volume.is_finite() {
                    continue;
                }
                if !speakers.contains_key(&sender) && speakers.len() >= 64 {
                    continue;
                }
                let speaker = if let Some(speaker) = speakers.get_mut(&sender) {
                    speaker
                } else {
                    speakers.insert(
                        sender,
                        Speaker {
                            decoder: OpusDecoder::new(48_000, 1)?,
                            expected: frame,
                            last_packet: Instant::now(),
                        },
                    );
                    speakers.get_mut(&sender).unwrap()
                };
                if frame < speaker.expected {
                    if speaker.last_packet.elapsed() < Duration::from_secs(1) {
                        continue;
                    }
                    speaker.decoder.reset_state()?;
                }
                let mut samples = vec![0.; MAX_PACKET_SAMPLES];
                if let Ok(count) = speaker
                    .decoder
                    .decode(&opus, MAX_PACKET_SAMPLES, &mut samples)
                {
                    samples.truncate(count);
                    if samples.iter().any(|sample| !sample.is_finite()) {
                        continue;
                    }
                    speaker.expected = frame.saturating_add((count / 480) as u64);
                    speaker.last_packet = Instant::now();
                    if let Ok(mut mixer) = mixer.lock() {
                        mixer.push(sender, &samples, volume.clamp(0., 4.), end);
                    }
                }
            }
            Some(Event::Capture(capture_epoch, mut samples)) => {
                if capture_epoch != epoch || muted.load(Ordering::Relaxed) || samples.len() != 960 {
                    continue;
                }
                if noise_suppression {
                    let mut input = [0.; 480];
                    let mut clean = [0.; 480];
                    for frame in samples.as_chunks_mut::<480>().0 {
                        for (output, sample) in input.iter_mut().zip(frame.iter()) {
                            *output = sample * 32768.;
                        }
                        denoise.process_frame(&mut clean, &input);
                        for (sample, clean) in frame.iter_mut().zip(clean.iter()) {
                            *sample = (*clean / 32768.).clamp(-1., 1.);
                        }
                    }
                }
                let mut packet = vec![0; MAX_PACKET_BYTES];
                let count = encoder.encode(&samples, 960, &mut packet)?;
                packet.truncate(count);
                emit(
                    &mut output,
                    LiveMediaEvent::Packet {
                        epoch,
                        sequence,
                        opus: packet,
                        end: false,
                    },
                )?;
                sequence += 2;
            }
            None => {}
        }
        if last_cleanup.elapsed() > Duration::from_secs(2) {
            speakers.retain(|_, speaker| speaker.last_packet.elapsed() < Duration::from_secs(5));
            if let Ok(mut mixer) = mixer.lock() {
                mixer
                    .speakers
                    .retain(|sender, _| speakers.contains_key(sender));
            }
            last_cleanup = Instant::now();
        }
    }
    muted.store(true, Ordering::Relaxed);
    drop(devices);
    Ok(())
}
fn emit(
    output: &mut impl io::Write,
    event: LiveMediaEvent,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(output, &Envelope::new(0, event))?;
    Ok(())
}

fn open_devices(
    tx: SyncSender<Event>,
    muted: Arc<AtomicBool>,
    mixer: Arc<Mutex<Mixer>>,
    epoch: u64,
) -> Result<Devices, String> {
    let mut hosts = vec![];
    #[cfg(target_os = "linux")]
    {
        match std::env::var("NUSCHEL_AUDIO_BACKEND").ok().as_deref() {
            Some("pulse") => hosts.push(cpal::HostId::PulseAudio),
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
            .and_then(|host| open_host(&host, tx.clone(), muted.clone(), mixer.clone(), epoch))
        {
            Ok(devices) => return Ok(devices),
            Err(error) => errors.push(format!("{id:?}: {error}")),
        }
    }
    Err(errors.join("; "))
}
fn open_host(
    host: &cpal::Host,
    tx: SyncSender<Event>,
    muted: Arc<AtomicBool>,
    mixer: Arc<Mutex<Mixer>>,
    epoch: u64,
) -> Result<Devices, String> {
    let output = host
        .default_output_device()
        .ok_or("no default output device")?;
    let config = output
        .default_output_config()
        .map_err(|error| error.to_string())?;
    let stream = match config.sample_format() {
        SampleFormat::F32 => {
            output_stream::<f32>(&output, stream_config(host, config), tx.clone(), mixer)
        }
        SampleFormat::I16 => {
            output_stream::<i16>(&output, stream_config(host, config), tx.clone(), mixer)
        }
        SampleFormat::U16 => {
            output_stream::<u16>(&output, stream_config(host, config), tx.clone(), mixer)
        }
        SampleFormat::I32 => {
            output_stream::<i32>(&output, stream_config(host, config), tx.clone(), mixer)
        }
        _ => return Err("unsupported output sample format".into()),
    }?;
    stream.play().map_err(|error| error.to_string())?;
    let input = (!muted.load(Ordering::Relaxed))
        .then(|| host.default_input_device())
        .flatten()
        .and_then(|input| {
            let config = input.default_input_config().ok()?;
            let stream = match config.sample_format() {
                SampleFormat::F32 => input_stream::<f32>(
                    &input,
                    stream_config(host, config),
                    tx.clone(),
                    muted,
                    epoch,
                ),
                SampleFormat::I16 => input_stream::<i16>(
                    &input,
                    stream_config(host, config),
                    tx.clone(),
                    muted,
                    epoch,
                ),
                SampleFormat::U16 => input_stream::<u16>(
                    &input,
                    stream_config(host, config),
                    tx.clone(),
                    muted,
                    epoch,
                ),
                SampleFormat::I32 => input_stream::<i32>(
                    &input,
                    stream_config(host, config),
                    tx.clone(),
                    muted,
                    epoch,
                ),
                _ => return None,
            }
            .ok()?;
            stream.play().ok()?;
            Some(stream)
        });
    Ok(Devices {
        _input: input,
        _output: stream,
    })
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
fn input_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: SyncSender<Event>,
    muted: Arc<AtomicBool>,
    epoch: u64,
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
    let errors = tx.clone();
    let mut frame = Vec::with_capacity(960);
    let mut previous = 0.;
    let mut accumulator = 0_u64;
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                if muted.load(Ordering::Relaxed) {
                    frame.clear();
                    return;
                }
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
                    accumulator += 48_000;
                    while accumulator >= u64::from(rate) {
                        accumulator -= u64::from(rate);
                        let fraction = 1. - accumulator as f32 / 48_000.;
                        frame.push(previous + (sample - previous) * fraction);
                        if frame.len() == 960 {
                            let complete = std::mem::replace(&mut frame, Vec::with_capacity(960));
                            let _ = tx.try_send(Event::Capture(epoch, complete));
                        }
                    }
                    previous = sample;
                }
            },
            error_callback(errors),
            None,
        )
        .map_err(|error| error.to_string())
}
fn output_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: SyncSender<Event>,
    mixer: Arc<Mutex<Mixer>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    if channels == 0 || channels > 32 || !(8000..=384_000).contains(&rate) {
        return Err("unsupported output configuration".into());
    }
    let step = 48_000. / rate as f64;
    let mut position = 0.;
    let mut previous = 0.;
    let mut next = 0.;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let Ok(mut mixer) = mixer.try_lock() else {
                    data.fill(T::from_sample(0.));
                    return;
                };
                for frame in data.chunks_exact_mut(channels) {
                    let sample = previous + (next - previous) * position as f32;
                    frame.fill(T::from_sample(sample));
                    position += step;
                    while position >= 1. {
                        position -= 1.;
                        previous = next;
                        next = mixer.next();
                    }
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
