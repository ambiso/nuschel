// SPDX-License-Identifier: EUPL-1.2
//! Hostile control messages (and tunneled voice) from several authenticated
//! connections into the server state: nothing may panic, and the channel tree
//! and memberships must stay consistent (checked by the harness).
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschld::fuzzing::Harness;

fuzz_target!(|data: &[u8]| {
    Harness::new().run(data);
});
