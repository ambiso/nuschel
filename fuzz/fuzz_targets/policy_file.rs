// SPDX-License-Identifier: EUPL-1.2
//! The sticky extension policy file (`nuschl-policy.json`, docs/extension.md
//! §1) as it may be found on disk, damaged or edited: loading never panics,
//! and a file that loads still loads, with the new record, after the client
//! raises a requirement and saves it. Files live in a scratch directory under
//! the system's temporary directory.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_session::sticky::PolicyStore;
use std::path::PathBuf;

thread_local! {
    static DIRECTORY: PathBuf = {
        let directory = std::env::temp_dir().join(format!("nuschl-fuzz-policy-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a scratch directory");
        directory
    };
}

fuzz_target!(|data: &[u8]| {
    DIRECTORY.with(|directory| {
        std::fs::write(directory.join("nuschl-policy.json"), data).expect("scratch file");
        let Ok(mut store) = PolicyStore::load(directory) else {
            return;
        };
        let capabilities = ["history/1".to_owned()];
        if store
            .raise("fuzz.example", 64738, true, &capabilities)
            .is_err()
        {
            // Only a full store refuses a new record.
            return;
        }
        let expected = store.get("fuzz.example", 64738);
        assert!(expected.nuschl);
        let reloaded = PolicyStore::load(directory).expect("what was saved loads");
        assert_eq!(reloaded.get("FUZZ.example", 64738), expected);
    });
});
