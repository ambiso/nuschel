// SPDX-License-Identifier: EUPL-1.2
//! Credential-bearing parent: only bounded IPC and opaque transport live here.
use crate::codec::{self, Request, Response, Role};
use nuschl_ipc::{Envelope, PROTOCOL_VERSION};
use std::{ffi::OsString, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
};

pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
pub const DECODE_TIMEOUT: Duration = Duration::from_secs(35);

pub struct Process {
    pub child: Child,
    pub input: Option<ChildStdin>,
    pub output: Option<ChildStdout>,
    _containment: nuschl_ipc::host::Containment,
}

impl Process {
    /// A kill request is not a released slot: callers retain this object until
    /// the OS has confirmed exit. Errors leave ownership with the caller.
    pub async fn retire(&mut self) -> Result<(), String> {
        if self.child.try_wait().map_err(|_| "cannot inspect codec worker")?.is_none() {
            self.child.kill().await.map_err(|_| "cannot stop codec worker")?;
        }
        self.child.wait().await.map_err(|_| "cannot reap codec worker")?;
        self.input.take();
        self.output.take();
        Ok(())
    }
}

pub async fn spawn(role: Role) -> Result<Process, String> {
    let executable = std::env::current_exe().map_err(|_| "cannot locate musicbot executable")?;
    let arguments = [OsString::from("--codec-worker"), OsString::from(role.argument())];
    let mut command = nuschl_ipc::host::musicbot_codec_sandbox_command(&executable, &arguments)
        .map_err(|_| "secure music codec sandbox is unavailable")?;
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped())
        .stderr(Stdio::null()).kill_on_drop(true).spawn()
        .map_err(|_| "cannot start isolated music codec worker")?;
    let containment = match nuschl_ipc::host::contain(&child) {
        Ok(containment) => containment,
        Err(_) => {
            child.kill().await.map_err(|_| "cannot stop uncontained codec worker")?;
            return Err("cannot contain music codec worker".into());
        }
    };
    let input = child.stdin.take();
    let output = child.stdout.take();
    let mut process = Process { child, input, output, _containment: containment };
    if process.input.is_none() || process.output.is_none() {
        process.retire().await?;
        return Err("codec worker pipes are unavailable".into());
    }
    Ok(process)
}

pub async fn write_request(input: &mut ChildStdin, id: u64, request: &Request) -> Result<(), String> {
    let mut bytes = Vec::new();
    codec::write_message(&mut bytes, &Envelope::new(id, request))?;
    input.write_all(&bytes).await.map_err(|_| "codec request write failed")?;
    input.flush().await.map_err(|_| "codec request flush failed".into())
}

pub async fn read_response(output: &mut ChildStdout, id: u64) -> Result<Response, String> {
    let size = output.read_u32().await.map_err(|_| "codec response header missing")? as usize;
    if size == 0 || size > codec::MAX_MESSAGE_BYTES {
        return Err("codec response exceeds its budget".into());
    }
    let mut bytes = vec![0; size];
    output.read_exact(&mut bytes).await.map_err(|_| "codec response truncated")?;
    let reply: Envelope<Response> = serde_json::from_slice(&bytes)
        .map_err(|_| "codec response is invalid")?;
    if reply.version != PROTOCOL_VERSION || reply.request_id != id {
        return Err("codec response belongs to another request or protocol".into());
    }
    if let Response::Error { message } = &reply.payload
        && (message.len() > codec::MAX_ERROR_BYTES || message.chars().any(char::is_control))
    {
        return Err("codec error response is invalid".into());
    }
    Ok(reply.payload)
}

/// One unique mutable owner per encoder/connection; no request queue or spawned
/// per-request tasks. A cancelled future leaves `busy` set and must retire the
/// old process before another request. Failure to reap never creates a new one.
pub struct Client {
    role: Role,
    process: Option<Process>,
    next: u64,
    busy: bool,
}

impl Client {
    pub fn new(role: Role) -> Self {
        Self { role, process: None, next: 1, busy: false }
    }
    pub async fn close(&mut self) -> Result<(), String> {
        if let Some(process) = self.process.as_mut() {
            process.retire().await?;
            self.process = None;
        }
        self.busy = false;
        Ok(())
    }
    pub fn started(&self) -> bool { self.process.is_some() }

    async fn request(&mut self, request: Request) -> Result<Response, String> {
        if self.busy { self.close().await?; }
        if self.process.is_none() {
            self.process = Some(spawn(self.role).await?);
            self.next = 1;
        }
        let id = self.next;
        self.next = self.next.checked_add(1).ok_or("codec request ids exhausted")?;
        self.busy = true;
        let process = self.process.as_mut().unwrap();
        let exchange = async {
            write_request(process.input.as_mut().unwrap(), id, &request).await?;
            read_response(process.output.as_mut().unwrap(), id).await
        };
        match tokio::time::timeout(RESPONSE_TIMEOUT, exchange).await {
            Ok(Ok(response)) => { self.busy = false; Ok(response) }
            result => {
                // Keep process/credit if this cannot confirm termination.
                self.close().await?;
                Err(match result {
                    Ok(Err(error)) => error,
                    _ => "codec worker did not answer in time".into(),
                })
            }
        }
    }

    async fn unexpected<T>(&mut self) -> Result<T, String> {
        self.close().await?;
        Err("codec worker returned an unexpected response".into())
    }

    pub async fn encode(&mut self, samples: Vec<f32>, bitrate: u32) -> Result<Vec<u8>, String> {
        if !codec::valid_pcm(&samples) || samples.iter().any(|v| v.abs() > 1.) {
            return Err("invalid encoder PCM".into());
        }
        match self.request(Request::Encode { samples, bitrate }).await? {
            Response::Packet { opus } if !opus.is_empty() && opus.len() <= codec::MAX_OPUS_BYTES => Ok(opus),
            Response::Error { message } => Err(message),
            _ => self.unexpected().await
        }
    }
    pub async fn hear(&mut self, sender: u32, opus: Vec<u8>, threshold: f32) -> Result<bool, String> {
        if opus.len() > codec::MAX_VOICE_BYTES || !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
            return Err("invalid ducking input".into());
        }
        match self.request(Request::Hear { sender, opus, threshold }).await? {
            Response::Speech { active } => Ok(active),
            Response::Error { message } => Err(message),
            _ => self.unexpected().await
        }
    }
    pub async fn forget(&mut self, sender: u32) -> Result<(), String> {
        if !self.started() { return Ok(()); }
        self.ack(Request::Forget { sender }).await
    }
    pub async fn clear(&mut self) -> Result<(), String> {
        if !self.started() { return Ok(()); }
        self.ack(Request::Clear).await
    }
    async fn ack(&mut self, request: Request) -> Result<(), String> {
        match self.request(request).await? {
            Response::Ack => Ok(()),
            Response::Error { message } => Err(message),
            _ => self.unexpected().await,
        }
    }
}
