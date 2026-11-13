// SPDX-License-Identifier: EUPL-1.2
//! Compiles the application icon (assets/nuschl.rc) into nuschl.exe with the
//! target's resource compiler, as GPUI's own build script already requires;
//! other targets do nothing.

fn main() {
    println!("cargo:rerun-if-changed=assets/nuschl.rc");
    println!("cargo:rerun-if-changed=assets/nuschl.ico");
    embed_resource::compile("assets/nuschl.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
