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
};

use nuschl_ipc::windows_sandbox::Policy;
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
    eprintln!("nuschl sandbox: {message}; the worker was not started");
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
///
/// The value comes from the command line, so nothing proves that this
/// process does not already own the handle it names (it could be the
/// current directory's handle, or a file opened earlier). It is therefore
/// never closed or wrapped: after checking that it names an open disk file
/// that is not a directory or a standard handle, this takes a duplicate, a
/// new non-inheritable handle that only the returned File owns. The
/// inherited handle stays open for the life of the worker.
pub fn inherited_file(value: &OsStr) -> io::Result<File> {
    let value = value
        .to_str()
        .ok_or_else(|| invalid("--file-handle needs a handle"))?;
    let standard = [
        io::stdin().as_raw_handle(),
        io::stdout().as_raw_handle(),
        io::stderr().as_raw_handle(),
    ];
    let handle = os::inherited_handle(&Win32, value, standard)?;
    let duplicate = os::duplicate(&Win32, handle)?.into_raw();
    // SAFETY: DuplicateHandle has just created `duplicate` for this call
    // and its guard has handed over ownership: nothing else in the process
    // knows the value, so the File becomes its only owner and closes it once.
    Ok(unsafe { File::from_raw_handle(duplicate) })
}

/// Retire an old-brand worker: revoke its grants before profile deletion.
/// Only the old application's namespace is accepted. Paths are supplied by the
/// profile migrator, never by a worker. Walk parents first, never following links.
pub fn retire_legacy_profile(name: &str, directories: &[std::path::PathBuf]) -> io::Result<()> {
    if !name.starts_with("nuschel.")
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(invalid("not a legacy Nuschel worker profile"));
    }
    let name = WideCString::from_str(name)?;
    let sid = os::existing_container_sid(&Win32, &name)?;
    fn revoke_tree(path: &std::path::Path, sid: &os::Sid) -> io::Result<()> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        // A link or junction must not lead the migration outside this profile.
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid("a legacy worker folder contains a reparse point"));
        }
        let wide = WideCString::new(wide(path.as_os_str()))?;
        os::revoke(&Win32, &wide, sid, metadata.is_dir())?;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                revoke_tree(&entry?.path(), sid)?;
            }
        }
        Ok(())
    }
    for directory in directories {
        revoke_tree(directory, &sid)?;
    }
    os::delete_container(&Win32, &name)
}
