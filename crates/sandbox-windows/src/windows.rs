// SPDX-License-Identifier: EUPL-1.2
//! The entry points on Windows: gather the inputs from this process (its
//! path, arguments, standard handles, the offered file) and run
//! [`crate::launch::launch`] with the real [`Win32`] layer.
#![allow(unsafe_code)]

use std::{
    env,
    ffi::OsStr,
    fs::{File, OpenOptions},
    io,
    os::windows::{
        ffi::OsStrExt as _,
        fs::OpenOptionsExt as _,
        io::{AsRawHandle as _, FromRawHandle as _},
    },
    sync::atomic::{AtomicBool, Ordering},
};

use nuschel_ipc::windows_sandbox::Policy;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

use crate::{
    api::Win32,
    launch::{Inputs, launch},
    os,
    plan::{WideCString, invalid},
};

/// If this process was started as a worker's launch stage, start the worker
/// confined and exit with its exit code; otherwise return at once.
pub fn launch_if_requested() {
    let (policy, arguments) = match Policy::parse(env::args_os().skip(1)) {
        Ok(None) => return,
        Ok(Some(parsed)) => parsed,
        Err(error) => fail(&error),
    };
    match run(&policy, &arguments) {
        // Exit codes are 32 bits on Windows; `exit` passes the bits on.
        Ok(code) => std::process::exit(code.cast_signed()),
        Err(error) => fail(&error.to_string()),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("nuschel sandbox: {message}; the worker was not started");
    std::process::exit(crate::LAUNCH_FAILED)
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().collect()
}

fn run(policy: &Policy, arguments: &[std::ffi::OsString]) -> io::Result<u32> {
    let executable = WideCString::new(wide(env::current_exe()?.as_os_str()))?;
    let writable = policy
        .writable
        .iter()
        .map(|directory| WideCString::new(wide(directory.as_os_str())))
        .collect::<io::Result<_>>()?;
    // Opened with the user's rights and handed over; the worker's container
    // gets no access to the file itself. Kept open (and so its handle valid)
    // until the launch has finished, then closed here.
    let file = match &policy.file {
        Some(path) => Some(
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(path)
                .map_err(|error| io::Error::other(format!("{}: {error}", path.display())))?,
        ),
        None => None,
    };
    let inputs = Inputs {
        executable,
        arguments: arguments.iter().map(|argument| wide(argument)).collect(),
        writable,
        file: file.as_ref().map(File::as_raw_handle),
        stdio: [
            io::stdin().as_raw_handle(),
            io::stdout().as_raw_handle(),
            io::stderr().as_raw_handle(),
        ],
        system_root: env::var_os("SystemRoot").map(|value| wide(&value)),
        windir: env::var_os("windir").map(|value| wide(&value)),
    };
    let code = launch(&Win32, policy, inputs);
    drop(file);
    code
}

/// The file the launch stage opened for this worker (`--file-handle N`).
/// Only the first call can succeed, and only for an inherited disk file that
/// is not one of the standard handles, so nothing else in this process owns
/// it.
pub fn inherited_file(value: &OsStr) -> io::Result<File> {
    static TAKEN: AtomicBool = AtomicBool::new(false);
    let value = value
        .to_str()
        .ok_or_else(|| invalid("--file-handle needs a handle"))?;
    let standard = [
        io::stdin().as_raw_handle(),
        io::stdout().as_raw_handle(),
        io::stderr().as_raw_handle(),
    ];
    let handle = os::inherited_handle(&Win32, value, standard)?;
    if TAKEN.swap(true, Ordering::SeqCst) {
        return Err(invalid("the inherited file was already taken"));
    }
    // SAFETY: `handle` is open (GetHandleInformation succeeded) and refers to
    // a disk file; the launch stage opened it for this process, inherited it
    // through the handle list and named it on our command line, and nothing
    // else in this process knows it: it is not a standard handle, and `TAKEN`
    // makes this the only place that ever takes ownership, once. From here
    // the File owns and eventually closes it.
    Ok(unsafe { File::from_raw_handle(handle) })
}
