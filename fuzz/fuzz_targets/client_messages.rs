// SPDX-License-Identifier: EUPL-1.2
//! Control frames from a hostile server into the client session's state
//! (`nuschel_session::connection::fuzzing`), Mumble's messages and the
//! extension's, as after a negotiation: nothing may panic, however the
//! messages contradict each other or exceed their bounds.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    nuschel_session::connection::fuzzing::run(data);
});
