// SPDX-License-Identifier: EUPL-1.2
//! Audio checks in Settings, independent of any server: a media worker of its
//! own lists devices, meters the microphone, plays the processed voice back and
//! runs the echo test. It never encodes or sends anything, so testing needs
//! neither a connection nor unmuting.
use nuschl_core::audio::{AudioDevice, AudioSettings, EchoSource};
use nuschl_ipc::{LiveMediaEvent, LiveMediaRequest};
use nuschl_session::{config::ClientSettings, workers::Media};
use std::time::Duration;
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

/// An echo test that has not answered by then has failed (no microphone).
const TEST_DEADLINE: Duration = Duration::from_secs(12);

#[derive(Debug)]
pub enum Command {
    /// Open or close the microphone, and play the processed voice back.
    Set {
        capture: bool,
        loopback: bool,
    },
    EchoTest,
    StopEchoTest,
}

#[derive(Debug)]
pub enum Event {
    Level {
        level_db: f32,
        voice: f32,
        transmitting: bool,
    },
    Devices {
        inputs: Vec<AudioDevice>,
        outputs: Vec<AudioDevice>,
        input: Option<String>,
        output: Option<String>,
        echo: EchoSource,
        noise: Option<String>,
        reference: Option<String>,
    },
    Status(String),
    EchoTest(Result<EchoReport, String>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EchoReport {
    pub echo_db: Option<f32>,
    pub reduction_db: Option<f32>,
    pub transmitted: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum EchoTestState {
    #[default]
    Idle,
    Running,
    Done(EchoReport),
    Failed(String),
}

/// What Settings shows about the local audio route.
#[derive(Clone, Debug)]
pub struct PreviewView {
    pub capture: bool,
    pub loopback: bool,
    pub level_db: f32,
    pub voice: f32,
    pub transmitting: bool,
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub echo: Option<EchoSource>,
    pub noise: Option<String>,
    /// The output Nuschl's canceller records (`None`: its own playback only).
    pub reference: Option<String>,
    pub status: Option<String>,
    pub echo_test: EchoTestState,
    /// The echo test opened the microphone and closes it when it ends.
    pub test_opened_capture: bool,
}

impl Default for PreviewView {
    fn default() -> Self {
        Self {
            capture: false,
            loopback: false,
            // Silence until the first measurement, not a full meter.
            level_db: -100.,
            voice: 0.,
            transmitting: false,
            inputs: Vec::new(),
            outputs: Vec::new(),
            input: None,
            output: None,
            echo: None,
            noise: None,
            reference: None,
            status: None,
            echo_test: EchoTestState::Idle,
            test_opened_capture: false,
        }
    }
}

impl PreviewView {
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Level {
                level_db,
                voice,
                transmitting,
            } => {
                self.level_db = level_db;
                self.voice = voice;
                self.transmitting = transmitting;
            }
            Event::Devices {
                inputs,
                outputs,
                input,
                output,
                echo,
                noise,
                reference,
            } => {
                self.reference = reference;
                self.inputs = inputs;
                self.outputs = outputs;
                self.input = input;
                self.output = output;
                self.echo = Some(echo);
                self.noise = noise;
            }
            Event::Status(message) => self.status = Some(message),
            Event::EchoTest(result) => {
                // A stopped test's late result is not shown.
                if self.echo_test == EchoTestState::Running {
                    self.echo_test = match result {
                        Ok(report) => EchoTestState::Done(report),
                        Err(message) => EchoTestState::Failed(message),
                    };
                }
                if std::mem::take(&mut self.test_opened_capture) {
                    self.close_capture();
                }
            }
        }
    }

    /// The microphone is off: no level, no playback of it.
    pub fn close_capture(&mut self) {
        self.capture = false;
        self.loopback = false;
        self.level_db = -100.;
        self.voice = 0.;
        self.transmitting = false;
    }
}

fn configure(media: &Media, audio: &AudioSettings) {
    let _ = media.commands.try_send(LiveMediaRequest::Configure {
        bitrate: 48_000,
        packet_loss_percent: 5,
        settings: audio.clone(),
        host: None,
    });
}

/// Run until `commands` closes (Settings closed).
pub async fn run(
    mut commands: mpsc::Receiver<Command>,
    mut settings: watch::Receiver<ClientSettings>,
    events: mpsc::Sender<crate::services::ServiceEvent>,
) {
    let send = |event: Event| {
        let _ = events.try_send(crate::services::ServiceEvent::Preview(event));
    };
    let (mut capture, mut loopback) = (false, false);
    let mut media: Option<Media> = None;
    let mut retry = Instant::now();
    let mut applied = settings.borrow().audio.clone();
    // An echo test waiting for the microphone to open, and its deadline.
    let mut test_pending = false;
    let mut test_deadline: Option<Instant> = None;
    // The echo test opened the microphone: close it again when the test ends
    // (PreviewView tracks the same).
    let mut test_opened_capture = false;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => break,
                Some(Command::Set { capture: wanted, loopback: play_back }) => {
                    loopback = play_back;
                    test_opened_capture = false;
                    if !wanted {
                        // Without the microphone there is no echo test.
                        test_deadline = None;
                        test_pending = false;
                    }
                    if wanted != capture {
                        // The microphone opens and closes with a new process.
                        capture = wanted;
                        media.take();
                        retry = Instant::now();
                    } else if let Some(media) = &media {
                        let _ = media.commands.try_send(LiveMediaRequest::Preview { capture, loopback });
                    }
                }
                Some(Command::EchoTest) => {
                    test_deadline = Some(Instant::now() + TEST_DEADLINE);
                    if !capture {
                        capture = true;
                        test_opened_capture = true;
                        media.take();
                        retry = Instant::now();
                        test_pending = true;
                    } else if let Some(media) = &media {
                        let _ = media.commands.try_send(LiveMediaRequest::EchoTest);
                    } else {
                        test_pending = true;
                    }
                }
                Some(Command::StopEchoTest) => {
                    test_deadline = None;
                    test_pending = false;
                    // A new worker ends the test sound at once.
                    capture &= !std::mem::take(&mut test_opened_capture);
                    media.take();
                    retry = Instant::now();
                }
            },
            changed = settings.changed() => {
                if changed.is_err() { break; }
                let audio = settings.borrow_and_update().audio.clone();
                if !audio.same_route(&applied) {
                    media.take();
                    retry = Instant::now();
                } else if audio != applied && let Some(media) = &media {
                    configure(media, &audio);
                }
                applied = audio;
            },
            _ = tokio::time::sleep_until(retry), if media.is_none() => {
                match Media::start() {
                    Ok(worker) => {
                        configure(&worker, &applied);
                        let _ = worker.commands.try_send(LiveMediaRequest::Preview { capture, loopback });
                        media = Some(worker);
                    }
                    Err(error) => {
                        tracing::debug!(%error, "settings audio worker did not start");
                        retry = Instant::now() + Duration::from_secs(3);
                        send(Event::Status(error.to_string()));
                    }
                }
            },
            _ = async { tokio::time::sleep_until(test_deadline.unwrap_or_else(Instant::now)).await }, if test_deadline.is_some() => {
                test_deadline = None;
                test_pending = false;
                if std::mem::take(&mut test_opened_capture) {
                    capture = false;
                    media.take();
                    retry = Instant::now();
                }
                send(Event::EchoTest(Err("The echo test needs a working microphone and speakers.".into())));
            },
            event = async { media.as_mut().unwrap().events.recv().await }, if media.is_some() => match event {
                Some(LiveMediaEvent::Level { level_db, voice, transmitting }) => send(Event::Level { level_db, voice, transmitting }),
                Some(LiveMediaEvent::Devices { inputs, outputs, input, output, echo, noise, reference }) => {
                    send(Event::Devices { inputs, outputs, input, output, echo, noise, reference });
                }
                Some(LiveMediaEvent::EchoTest { echo_db, reduction_db, transmitted }) => {
                    test_deadline = None;
                    if std::mem::take(&mut test_opened_capture) {
                        capture = false;
                        media.take();
                        retry = Instant::now();
                    }
                    send(Event::EchoTest(Ok(EchoReport { echo_db, reduction_db, transmitted })));
                }
                Some(LiveMediaEvent::Status { ready, message }) => {
                    tracing::debug!(ready, %message, "settings audio check");
                    if ready {
                        if std::mem::take(&mut test_pending) && let Some(media) = &media {
                            let _ = media.commands.try_send(LiveMediaRequest::EchoTest);
                        }
                    } else {
                        media.take();
                        retry = Instant::now() + Duration::from_millis(if message.starts_with("Audio reconnecting") { 2000 } else { 200 });
                        if test_deadline.take().is_some() {
                            // No sound system: say so now rather than at the deadline.
                            test_pending = false;
                            capture &= !std::mem::take(&mut test_opened_capture);
                            send(Event::EchoTest(Err("The echo test needs a working microphone and speakers.".into())));
                        }
                    }
                    send(Event::Status(message));
                }
                Some(_) => {}
                None => {
                    // The worker exited without a word (it could not start, or crashed).
                    tracing::debug!("settings audio worker exited");
                    media.take();
                    retry = Instant::now() + Duration::from_secs(1);
                    send(Event::Status("The audio worker stopped; retrying…".into()));
                }
            },
        }
    }
    if let Some(media) = media {
        let _ = media.commands.try_send(LiveMediaRequest::Shutdown);
    }
}
