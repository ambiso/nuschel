// SPDX-License-Identifier: EUPL-1.2
#![forbid(unsafe_code)]
use nuschl_update::verifier_wire::{Algorithm, Request};
fn run() -> Result<(), Box<dyn std::error::Error>> {
    // The Windows bootstrap relaunches this fixed executable inside its LPAC.
    #[cfg(windows)]
    nuschl_sandbox_windows::launch_if_requested();
    if std::env::args_os().len() != 1 {
        return Err("unexpected verifier arguments".into());
    }
    let request = Request::read(std::io::stdin().lock(), Algorithm::Ed25519)?;
    nuschl_update_worker::verify_ed25519_component(
        request.public().try_into()?,
        request.signature().try_into()?,
        request.message(),
    )?;
    request.accepted(std::io::stdout().lock())?;
    Ok(())
}
fn main() {
    if run().is_err() {
        eprintln!("update verification rejected");
        std::process::exit(1);
    }
}
