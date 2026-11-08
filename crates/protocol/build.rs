// SPDX-License-Identifier: EUPL-1.2
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/Mumble.proto");
    println!("cargo:rerun-if-changed=proto/MumbleUDP.proto");
    prost_build::Config::new()
        .compile_protos(&["proto/Mumble.proto", "proto/MumbleUDP.proto"], &["proto"])?;
    Ok(())
}
