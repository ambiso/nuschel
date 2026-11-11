// SPDX-License-Identifier: EUPL-1.2
//! The application's one media worker (docs/clients.md, "Voice on several
//! servers"). Every connection attaches to it once connected: the voice it
//! receives is mixed into one output, at the server's volume, and the one
//! microphone is encoded once and handed to every server that is unmuted (the
//! talk servers; the client's voice policy decides which). A deafened server
//! gets nothing and plays nothing; with every server deafened no worker runs.
//!
//! The microphone opens and closes with a new worker process, as before
//! (native stream teardown can block inside a backend; replacing the
//! isolated process closes its audio capability even then): the worker is
//! replaced when the microphone's state or its destinations change. Speech
//! buffered for one set of listeners must not reach a newly added listener.
use super::{Event, HOST_PIN, MAX_SOUND_DELAY, VoiceKeys, chime, configure, pinned_host, play};
use nuschl_core::audio::{Sound, TransmitMode};
use nuschl_ipc::{LiveMediaEvent, LiveMediaRequest};
use nuschl_session::{config::ClientSettings, workers::Media};
use std::collections::BTreeMap;
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

/// Encoded microphone audio for one server's session.
pub struct Outgoing {
    pub revision: u64,
    pub opus: Vec<u8>,
    pub end: bool,
}

/// A server's state as it sees it, and the link the encoder plans for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    /// This connection's local mute/deafen revision. Buffered packets from
    /// an earlier routing decision must not survive a mute/unmute round trip.
    pub revision: u64,
    /// Plain Mumble voice is forbidden on an E2EE-only server until SFrame.
    pub voice_allowed: bool,
    pub muted: bool,
    pub deafened: bool,
    pub bitrate: u32,
    pub loss_percent: u8,
}

/// What connections and the driver tell the worker's supervisor.
pub enum Control {
    /// A connection is up; its share of the microphone goes to `voice`.
    Attach {
        server: String,
        generation: u64,
        link: Link,
        voice: mpsc::Sender<Outgoing>,
    },
    /// The server's state changed, or its link; `cue` is the sound for it.
    Update {
        server: String,
        generation: u64,
        link: Link,
        cue: Option<Sound>,
    },
    /// The connection ended.
    Detach { server: String, generation: u64 },
    /// A sound effect from a connection that is not deafened.
    Sound(Sound),
    /// A sound that plays even without a worker (a disconnection).
    Chime(Sound),
}

/// Voice from a server, for the mix.
pub struct Playback {
    pub server: String,
    pub generation: u64,
    pub sender: u32,
    pub sequence: u64,
    pub opus: Vec<u8>,
    pub end: bool,
    pub volume: f32,
}

/// The way to the supervisor: control messages are few and must not be
/// lost; voice is many and may be dropped when the worker falls behind.
#[derive(Clone)]
pub struct Hub {
    control: mpsc::UnboundedSender<Control>,
    playback: mpsc::Sender<Playback>,
}

impl Hub {
    pub fn send(&self, control: Control) {
        let _ = self.control.send(control);
    }

    pub fn play(&self, playback: Playback) {
        let _ = self.playback.try_send(playback);
    }
}

/// Detaches a connection from the worker however its task ends (an abort
/// included).
pub struct Attached {
    pub hub: Hub,
    pub server: String,
    pub generation: u64,
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.hub.send(Control::Detach {
            server: self.server.clone(),
            generation: self.generation,
        });
    }
}

/// One attached server.
struct Stream {
    generation: u64,
    /// The worker's number for it (new for every connection).
    number: u32,
    link: Link,
    voice: mpsc::Sender<Outgoing>,
}

/// What the worker does for the attached servers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Plan {
    /// Some server is not deafened: a worker runs.
    listen: bool,
    /// Some server gets the microphone: it is open.
    capture: bool,
    /// The lowest bitrate and the highest loss among those servers.
    bitrate: u32,
    loss_percent: u8,
}

fn plan<'a>(links: impl Iterator<Item = &'a Link>) -> Plan {
    let mut plan = Plan {
        listen: false,
        capture: false,
        bitrate: 48_000,
        loss_percent: 1,
    };
    let mut speaking = Vec::new();
    for link in links {
        plan.listen |= link.voice_allowed && !link.deafened;
        if speaking_to(link) {
            speaking.push(link);
        }
    }
    plan.capture = !speaking.is_empty();
    if plan.capture {
        plan.bitrate = speaking
            .iter()
            .map(|link| link.bitrate)
            .min()
            .unwrap_or(48_000);
        plan.loss_percent = speaking
            .iter()
            .map(|link| link.loss_percent)
            .max()
            .unwrap_or(1);
    }
    plan
}

/// The microphone goes to this server.
fn speaking_to(link: &Link) -> bool {
    link.voice_allowed && !link.muted && !link.deafened
}

/// A destination started/stopped receiving the microphone, or changed its
/// privacy state and returned to speaking before the worker could settle.
fn capture_route_changed(old: &Link, next: &Link) -> bool {
    speaking_to(old) != speaking_to(next) || (speaking_to(next) && old.revision != next.revision)
}

/// Supervise the worker until the driver goes away.
pub async fn run(
    (mut control, mut playback): (mpsc::UnboundedReceiver<Control>, mpsc::Receiver<Playback>),
    events: mpsc::Sender<Event>,
    (mut settings, mut voice_keys): (watch::Receiver<ClientSettings>, watch::Receiver<VoiceKeys>),
) {
    let mut streams = BTreeMap::<String, Stream>::new();
    let mut numbers = 0_u32;
    let mut media: Option<Media> = None;
    let mut retry = Instant::now();
    // Packets from an earlier worker, or from before the microphone's state
    // changed, never leave.
    let mut epoch = 0_u64;
    let mut planned = plan(std::iter::empty());
    let mut applied = settings.borrow().audio.clone();
    let mut talk_key = voice_keys.borrow().talk;
    // The audio server that last worked, pinned for a while after a failure (see pinned_host).
    let mut audio_host: (Option<String>, Instant) = (None, Instant::now());
    // Sound effects for the next worker while this one is being replaced.
    let mut pending = Vec::<(Sound, Instant)>::new();
    // Changes not applied to the worker yet, and their sounds (and whether
    // each plays without a worker, see `Control::Chime`).
    let mut settle: Option<Instant> = None;
    let mut cues = Vec::<(Sound, bool)>::new();
    loop {
        tokio::select! {
            message = control.recv() => {
                let Some(message) = message else { break };
                let capture_changed = match &message {
                    Control::Attach { server, link, .. } => speaking_to(link)
                        || streams.get(server).is_some_and(|old| speaking_to(&old.link)),
                    Control::Update { server, generation, link, .. } => streams.get(server)
                        .filter(|old| old.generation == *generation)
                        .is_some_and(|old| capture_route_changed(&old.link, link)),
                    Control::Detach { server, generation } => streams.get(server)
                        .is_some_and(|old| old.generation == *generation && speaking_to(&old.link)),
                    Control::Sound(_) | Control::Chime(_) => false,
                };
                if capture_changed {
                    // Invalidate even a packet already in the worker's IPC
                    // queue before admitting another speaking destination.
                    // Replacing the process also discards its queued PCM.
                    media.take();
                    epoch += 1;
                    retry = Instant::now();
                }
                match message {
                    Control::Attach { server, generation, link, voice } => {
                        numbers = numbers.wrapping_add(1);
                        if let Some(old) = streams.insert(server, Stream { generation, number: numbers, link, voice })
                            && let Some(media) = &media {
                            let _ = media.commands.try_send(LiveMediaRequest::EndStream { stream: old.number });
                        }
                    }
                    Control::Update { server, generation, link, cue } => {
                        // Routing follows at once; the worker after a moment.
                        if let Some(stream) = streams.get_mut(&server).filter(|stream| stream.generation == generation) {
                            if link.deafened && !stream.link.deafened && let Some(media) = &media {
                                // What it had buffered is not played.
                                let _ = media.commands.try_send(LiveMediaRequest::EndStream { stream: stream.number });
                            }
                            stream.link = link;
                        }
                        cues.extend(cue.map(|sound| (sound, true)));
                    }
                    Control::Detach { server, generation } => {
                        if streams.get(&server).is_some_and(|stream| stream.generation == generation)
                            && let Some(stream) = streams.remove(&server)
                            && let Some(media) = &media {
                            let _ = media.commands.try_send(LiveMediaRequest::EndStream { stream: stream.number });
                        }
                    }
                    Control::Sound(sound) => cues.push((sound, false)),
                    Control::Chime(sound) => cues.push((sound, true)),
                }
                // Moving your voice changes two servers at once: settle first.
                settle.get_or_insert_with(|| Instant::now() + SETTLE);
            },
            _ = tokio::time::sleep_until(settle.unwrap_or_else(Instant::now)), if settle.is_some() => {
                settle = None;
                let next = plan(streams.values().map(|stream| &stream.link));
                let opened = !planned.capture && next.capture;
                let closed = planned.capture && !next.capture;
                if opened || closed || !next.listen {
                    // The microphone opens or closes with a new process; with
                    // nothing to listen to, none runs.
                    media.take();
                    epoch += 1;
                    retry = Instant::now();
                } else if (next.bitrate, next.loss_percent) != (planned.bitrate, planned.loss_percent)
                    && let Some(media) = &media {
                    configure(media, &applied, (next.bitrate, next.loss_percent), pinned_host(&audio_host));
                }
                planned = next;
                let current = settings.borrow();
                for (sound, chimes) in sounds_to_play(std::mem::take(&mut cues), opened, closed) {
                    match &media {
                        Some(media) => play(media, &current, sound),
                        None if planned.listen => queue(&mut pending, sound),
                        // No worker will run: one that opens only the output plays it.
                        None if chimes && current.sound_effects => {
                            tokio::spawn(chime(current.audio.clone(), current.sound_volume, pinned_host(&audio_host), sound));
                        }
                        None => {}
                    }
                }
            },
            Some(voice) = playback.recv() => {
                if let (Some(media), Some(stream)) = (&media, streams.get(&voice.server))
                    && stream.generation == voice.generation
                    && stream.link.voice_allowed
                    && !stream.link.deafened
                {
                    let _ = media.commands.try_send(LiveMediaRequest::Playback {
                        stream: stream.number,
                        sender: voice.sender,
                        sequence: voice.sequence,
                        opus: voice.opus,
                        end: voice.end,
                        volume: voice.volume,
                    });
                }
            },
            changed = settings.changed() => {
                if changed.is_err() { break; }
                let audio = settings.borrow_and_update().audio.clone();
                if !audio.same_route(&applied) {
                    // New devices need new native streams: replace the worker.
                    media.take();
                    epoch += 1;
                    retry = Instant::now();
                } else if audio != applied && let Some(media) = &media {
                    configure(media, &audio, (planned.bitrate, planned.loss_percent), pinned_host(&audio_host));
                }
                applied = audio;
            },
            changed = voice_keys.changed() => {
                if changed.is_err() { break; }
                let keys = *voice_keys.borrow_and_update();
                // The talk key's own changes reach the worker, with their sound.
                if std::mem::replace(&mut talk_key, keys.talk) != keys.talk && let Some(media) = &media {
                    let _ = media.commands.try_send(LiveMediaRequest::PushToTalk { active: keys.talk });
                    let current = settings.borrow();
                    if planned.capture && current.audio.transmit == TransmitMode::PushToTalk {
                        play(media, &current, if keys.talk { Sound::TalkOn } else { Sound::TalkOff });
                    }
                }
            },
            _ = tokio::time::sleep_until(retry), if settle.is_none() && planned.listen && media.is_none() => {
                epoch += 1;
                // Until the devices are open, nobody hears you yet.
                broadcast(&streams, &events, |server, generation| Event::AudioStatus { server, generation, message: "Starting audio…".into() }).await;
                match Media::start() {
                    Ok(worker) => {
                        configure(&worker, &applied, (planned.bitrate, planned.loss_percent), pinned_host(&audio_host));
                        let _ = worker.commands.try_send(LiveMediaRequest::SetState { epoch, muted: !planned.capture, deafened: false });
                        let _ = worker.commands.try_send(LiveMediaRequest::PushToTalk { active: voice_keys.borrow().talk });
                        for (sound, requested) in pending.drain(..) {
                            if requested.elapsed() < MAX_SOUND_DELAY { play(&worker, &settings.borrow(), sound); }
                        }
                        media = Some(worker);
                    },
                    Err(error) => {
                        retry = Instant::now() + std::time::Duration::from_secs(3);
                        broadcast(&streams, &events, |server, generation| Event::AudioStatus { server, generation, message: error.to_string() }).await;
                    },
                }
            },
            event = async { media.as_mut().unwrap().events.recv().await }, if media.is_some() => match event {
                Some(LiveMediaEvent::Packet { epoch: sent, opus, end, .. }) => {
                    // Only servers that are unmuted get it (each connection checks again).
                    if sent != epoch || voice_keys.borrow().mute { continue; }
                    for stream in streams.values().filter(|stream| speaking_to(&stream.link)) {
                        let _ = stream.voice.try_send(Outgoing { revision: stream.link.revision, opus: opus.clone(), end });
                    }
                },
                Some(LiveMediaEvent::Devices { inputs, outputs, input, output, echo, noise, reference }) => {
                    if let Some((host, _)) = output.as_deref().and_then(|id| id.split_once(':')) {
                        audio_host.0 = Some(host.to_string());
                    }
                    broadcast(&streams, &events, |server, generation| Event::AudioDevices {
                        server, generation, inputs: inputs.clone(), outputs: outputs.clone(), input: input.clone(),
                        output: output.clone(), echo: echo.clone(), noise: noise.clone(), reference: reference.clone(),
                    }).await;
                },
                Some(LiveMediaEvent::Echo { erle_db, residual_likelihood, .. }) => {
                    for (server, stream) in &streams {
                        let _ = events.try_send(Event::Echo { server: server.clone(), generation: stream.generation, erle_db, residual_likelihood });
                    }
                },
                Some(LiveMediaEvent::Health { clipping, echo_guard, double_talk, underruns, playback_ms }) => {
                    let health = crate::server::AudioHealth { clipping, echo_guard, double_talk, underruns, playback_ms };
                    broadcast(&streams, &events, |server, generation| Event::AudioHealth { server, generation, health }).await;
                },
                Some(LiveMediaEvent::Status { ready, message }) => {
                    if !ready {
                        audio_host.1 = Instant::now() + HOST_PIN;
                        media.take();
                        epoch += 1;
                        retry = Instant::now() + std::time::Duration::from_millis(if message.starts_with("Audio reconnecting") { 2000 } else { 200 });
                    }
                    // A worker for listening only says so; a server you listen
                    // to has no microphone, wherever the one microphone goes.
                    broadcast(&streams, &events, |server, generation| Event::AudioStatus { server, generation, message: message.clone() }).await;
                },
                // The meter and the echo test are the Settings check's (preview.rs).
                Some(LiveMediaEvent::Level { .. } | LiveMediaEvent::EchoTest { .. }) => {},
                None => {
                    media.take();
                    epoch += 1;
                    retry = Instant::now() + std::time::Duration::from_secs(2);
                    broadcast(&streams, &events, |server, generation| Event::AudioStatus { server, generation, message: "Audio worker restarting…".into() }).await;
                },
            },
        }
    }
}

/// How long changes gather before the worker follows them: the client
/// moves your voice by muting one server and unmuting another.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(30);

/// The sounds for changes that settled. Moving your voice mutes one server
/// and unmutes another: what you hear is whether the microphone opened or
/// closed, if either, not both sounds.
fn sounds_to_play(cues: Vec<(Sound, bool)>, opened: bool, closed: bool) -> Vec<(Sound, bool)> {
    let has = |wanted| cues.iter().any(|(sound, _)| *sound == wanted);
    let moved = has(Sound::Muted) && has(Sound::Unmuted);
    let mut seen = Vec::new();
    cues.into_iter()
        .filter(|(sound, _)| match sound {
            Sound::Muted if moved => closed,
            Sound::Unmuted if moved => opened,
            _ => true,
        })
        .filter(|(sound, _)| {
            let first = !seen.contains(sound);
            seen.push(*sound);
            first
        })
        .collect()
}

/// Report a worker event on every attached server.
async fn broadcast(
    streams: &BTreeMap<String, Stream>,
    events: &mpsc::Sender<Event>,
    event: impl Fn(String, u64) -> Event,
) {
    for (server, stream) in streams
        .iter()
        .filter(|(_, stream)| stream.link.voice_allowed)
    {
        let _ = events.send(event(server.clone(), stream.generation)).await;
    }
}

/// Keep a sound effect for the worker that is starting (a few at most).
fn queue(pending: &mut Vec<(Sound, Instant)>, sound: Sound) {
    if pending.len() < 4 {
        pending.push((sound, Instant::now()));
    }
}

/// The supervisor's two inputs and the task that runs it.
pub fn start(
    handle: &tokio::runtime::Handle,
    events: mpsc::Sender<Event>,
    watches: (watch::Receiver<ClientSettings>, watch::Receiver<VoiceKeys>),
) -> (Hub, tokio::task::JoinHandle<()>) {
    let (control, control_rx) = mpsc::unbounded_channel();
    // About a second of voice from four talkers on each of eight servers.
    let (playback, playback_rx) = mpsc::channel(1024);
    let task = handle.spawn(run((control_rx, playback_rx), events, watches));
    (Hub { control, playback }, task)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(muted: bool, deafened: bool, bitrate: u32, loss_percent: u8) -> Link {
        Link {
            revision: 0,
            voice_allowed: true,
            muted,
            deafened,
            bitrate,
            loss_percent,
        }
    }

    #[test]
    fn routing_revisions_invalidate_buffered_capture_even_after_rapid_toggles() {
        let initial = link(false, false, 40_000, 3);
        let muted = Link {
            revision: 1,
            muted: true,
            ..initial
        };
        let unmuted = Link {
            revision: 2,
            ..initial
        };
        assert!(capture_route_changed(&initial, &muted));
        assert!(capture_route_changed(&muted, &unmuted));
        // Coalescing may see only the initial and final state: flags alone
        // cannot distinguish this privacy transition from no change.
        assert!(capture_route_changed(&initial, &unmuted));
        let adapted = Link {
            bitrate: 24_000,
            loss_percent: 12,
            ..initial
        };
        assert!(!capture_route_changed(&initial, &adapted));
        let listening = Link {
            muted: true,
            ..initial
        };
        let deafened = Link {
            deafened: true,
            revision: 1,
            ..listening
        };
        assert!(!capture_route_changed(&listening, &deafened));
    }

    #[test]
    fn an_e2ee_only_server_never_gets_plain_capture_or_playback() {
        let protected = Link {
            voice_allowed: false,
            ..link(false, false, 40_000, 3)
        };
        assert!(!speaking_to(&protected));
        let alone = plan([protected].iter());
        assert!(!alone.listen && !alone.capture);
        let compatible = link(false, false, 24_000, 5);
        let mixed = plan([protected, compatible].iter());
        assert!(mixed.listen && mixed.capture);
        assert_eq!((mixed.bitrate, mixed.loss_percent), (24_000, 5));
        assert!(capture_route_changed(
            &compatible,
            &Link {
                voice_allowed: false,
                ..compatible
            }
        ));
    }

    #[test]
    fn the_microphone_goes_only_to_unmuted_servers() {
        let talk = link(false, false, 40_000, 3);
        let listen = link(true, false, 24_000, 20);
        let deaf = link(true, true, 6_000, 30);
        assert!(speaking_to(&talk));
        assert!(!speaking_to(&listen) && !speaking_to(&deaf));
        let both = plan([talk, listen, deaf].iter());
        assert_eq!(
            both,
            Plan {
                listen: true,
                capture: true,
                // Listening servers do not slow the encoder down.
                bitrate: 40_000,
                loss_percent: 3,
            }
        );
    }

    #[test]
    fn moving_your_voice_sounds_like_the_microphone() {
        let moved = vec![(Sound::Muted, true), (Sound::Unmuted, true)];
        assert!(sounds_to_play(moved.clone(), false, false).is_empty());
        assert_eq!(sounds_to_play(moved, true, false), [(Sound::Unmuted, true)]);
        // On its own, a mute is heard even while another server keeps the
        // microphone; the same sound twice plays once.
        let muted = vec![
            (Sound::Muted, true),
            (Sound::UserJoined, false),
            (Sound::Muted, true),
        ];
        assert_eq!(
            sounds_to_play(muted, false, false),
            [(Sound::Muted, true), (Sound::UserJoined, false)]
        );
    }

    #[test]
    fn the_encoder_suits_every_talk_server() {
        let one = link(false, false, 40_000, 3);
        let two = link(false, false, 24_000, 12);
        let plan = plan([one, two].iter());
        assert_eq!((plan.bitrate, plan.loss_percent), (24_000, 12));
    }

    #[test]
    fn listening_needs_no_microphone_and_deafness_no_worker() {
        let listening = plan([link(true, false, 40_000, 3)].iter());
        assert!(listening.listen && !listening.capture);
        let deafened = plan([link(true, true, 40_000, 3), link(true, true, 40_000, 3)].iter());
        assert!(!deafened.listen && !deafened.capture);
        let nothing = plan(std::iter::empty());
        assert!(!nothing.listen && !nothing.capture);
    }
}
