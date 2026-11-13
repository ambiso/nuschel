// SPDX-License-Identifier: EUPL-1.2
//! The music bot's credential-free codec roles (docs/musicbot.md): song
//! decoding and resampling, Opus encoding and speech detection for ducking.
//! `nuschl-musicbot` starts this executable from its fixed libexec path in the
//! Linux sandbox as `--codec-worker <decode|encode|duck>` and speaks
//! `nuschl_ipc::musicbot` over standard input and output; nothing else.
mod decode;
mod vad;
mod worker;

use nuschl_ipc::musicbot::Role;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<_> = std::env::args_os().collect();
    let role = (arguments.len() == 3 && arguments[1] == "--codec-worker")
        .then(|| arguments[2].to_str().and_then(Role::parse))
        .flatten();
    let Some(role) = role else {
        return ExitCode::FAILURE;
    };
    match worker::run(role) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
