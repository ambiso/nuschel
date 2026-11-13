// SPDX-License-Identifier: EUPL-1.2
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/Mumble.proto");
    println!("cargo:rerun-if-changed=proto/MumbleUDP.proto");
    println!("cargo:rerun-if-changed=proto/nuschl.proto");
    prost_build::Config::new()
        // Passwords, tokens, keys and message contents must never reach logs;
        // these get hand-written redacting `Debug` impls (src/control/redacted.rs,
        // src/nuschl/redacted.rs).
        .skip_debug([
            ".MumbleProto.Authenticate",
            ".MumbleProto.CryptSetup",
            ".MumbleProto.UserState",
            ".MumbleProto.TextMessage",
            ".MumbleProto.ACL.ChanACL",
            ".nuschl.v1.ChatSend",
            ".nuschl.v1.ChatEdit",
            ".nuschl.v1.Message",
            ".nuschl.v1.Preview",
            ".nuschl.v1.PreviewImage",
            ".nuschl.v1.ShareLinkCreate",
            ".nuschl.v1.ShareLink",
        ])
        .compile_protos(
            &[
                "proto/Mumble.proto",
                "proto/MumbleUDP.proto",
                "proto/nuschl.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
