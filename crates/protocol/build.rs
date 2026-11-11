// SPDX-License-Identifier: EUPL-1.2
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/Mumble.proto");
    println!("cargo:rerun-if-changed=proto/MumbleUDP.proto");
    println!("cargo:rerun-if-changed=proto/nuschel.proto");
    prost_build::Config::new()
        // Passwords, tokens, keys and message contents must never reach logs;
        // these get hand-written redacting `Debug` impls (src/control/redacted.rs,
        // src/nuschel/redacted.rs).
        .skip_debug([
            ".MumbleProto.Authenticate",
            ".MumbleProto.CryptSetup",
            ".MumbleProto.UserState",
            ".MumbleProto.TextMessage",
            ".MumbleProto.ACL.ChanACL",
            ".nuschel.v1.ChatSend",
            ".nuschel.v1.ChatEdit",
            ".nuschel.v1.Message",
            ".nuschel.v1.ShareLinkCreate",
            ".nuschel.v1.ShareLink",
        ])
        .compile_protos(
            &[
                "proto/Mumble.proto",
                "proto/MumbleUDP.proto",
                "proto/nuschel.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
