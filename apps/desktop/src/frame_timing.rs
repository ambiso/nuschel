// SPDX-License-Identifier: EUPL-1.2
//! Frame timings for measuring smoothness: build with `--features
//! frame-timing` and set `NUSCHEL_FRAME_TIMING=<file>`. Each line is one
//! event, times in microseconds since startup:
//! `draw <start> <request_layout> <prepaint> <paint> <total>` (GPUI's own
//! phases; request_layout includes rendering views into elements),
//! `render <start> <duration>` (`Desktop::render` alone) and
//! `present <start> <duration>` (handing the scene to the GPU). The first
//! line, `origin Instant { tv_sec: …, tv_nsec: … }`, places startup on the
//! monotonic clock, to line events up with the compositor's.
use gpui_profiler::{FrameEvent, FrameTimingCollector, set_trace_enabled};
use std::{
    cell::RefCell,
    fs::File,
    io::{BufWriter, Write},
    time::{Duration, Instant},
};

struct Log {
    file: BufWriter<File>,
    collector: FrameTimingCollector,
    origin: Instant,
}

thread_local! {
    static LOG: RefCell<Option<Log>> = const { RefCell::new(None) };
}

pub fn init() {
    let Some(path) = std::env::var_os("NUSCHEL_FRAME_TIMING") else {
        return;
    };
    let file = match File::create(&path) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(%error, "frame timing log");
            return;
        }
    };
    set_trace_enabled(true);
    let origin = Instant::now();
    let mut file = BufWriter::new(file);
    let _ = writeln!(file, "origin {origin:?}");
    LOG.set(Some(Log {
        file,
        collector: FrameTimingCollector::new(),
        origin,
    }));
}

/// After `Desktop::render`, which began at `start`: write it and the frames
/// GPUI finished since the last call.
pub fn rendered(start: Instant) {
    let duration = start.elapsed();
    LOG.with_borrow_mut(|log| {
        let Some(log) = log else { return };
        let at = |instant: Instant| instant.saturating_duration_since(log.origin).as_micros();
        let us = |duration: Duration| duration.as_micros();
        for event in log.collector.collect_unseen() {
            let _ = match event {
                FrameEvent::Draw(frame) => writeln!(
                    log.file,
                    "draw {} {} {} {} {}",
                    at(frame.draw_start),
                    us(frame.phases.request_layout),
                    us(frame.phases.prepaint),
                    us(frame.phases.paint),
                    us(frame.draw_duration()),
                ),
                FrameEvent::Present(present) => writeln!(
                    log.file,
                    "present {} {}",
                    at(present.present_start),
                    us(present.present_duration()),
                ),
            };
        }
        let _ = writeln!(log.file, "render {} {}", at(start), us(duration));
        let _ = log.file.flush();
    });
}
