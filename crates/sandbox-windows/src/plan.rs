// SPDX-License-Identifier: EUPL-1.2
//! Everything the launch stage computes without calling Windows: NUL-terminated
//! UTF-16 strings, the command line, the environment block, paths inside the
//! container's folder, job limits, the handle list, SID and ACL parsing. No
//! unsafe code; the tests run under Miri on every target.
#![forbid(unsafe_code)]

use std::{ffi::c_void, io, ptr};

use nuschel_ipc::windows_sandbox::{Policy, append_argument};
use windows_sys::{
    Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
        Security::{CONTAINER_INHERIT_ACE, INHERIT_ONLY_ACE, INHERITED_ACE, OBJECT_INHERIT_ACE},
        Storage::FileSystem::{
            DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE,
            FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
            FILE_READ_EA, FILE_TRAVERSE, READ_CONTROL, SYNCHRONIZE,
        },
        System::JobObjects::{
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            JOB_OBJECT_LIMIT_PROCESS_TIME, JOB_OBJECT_UILIMIT_DESKTOP,
            JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
            JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES,
            JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
            JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JOBOBJECT_BASIC_UI_RESTRICTIONS,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        },
    },
    core::{HRESULT, PCWSTR},
};

/// `PROCESS_CREATION_CHILD_PROCESS_RESTRICTED` (winbase.h): no child processes.
pub(crate) const CHILD_PROCESS_RESTRICTED: u32 = 0x01;
/// `PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT` (winbase.h): a less
/// privileged AppContainer, outside "ALL APPLICATION PACKAGES".
pub(crate) const ALL_APPLICATION_PACKAGES_OPT_OUT: u32 = 0x01;
/// `PROCESS_CREATION_MITIGATION_POLICY_*_ALWAYS_ON` (winnt.h): no legacy
/// extension DLLs (AppInit and similar, bit 32), no non-system fonts (48), no
/// images from remote shares (52) or with a low mandatory label (56).
pub(crate) const MITIGATIONS: u64 = (1 << 32) | (1 << 48) | (1 << 52) | (1 << 56);
/// `ACCESS_ALLOWED_ACE_TYPE` (winnt.h).
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
/// `ACL_REVISION` and `ACL_REVISION_DS` (winnt.h).
const ACL_REVISIONS: [u8; 2] = [2, 4];
/// The longest command line CreateProcessW accepts: "32,767 characters,
/// including the Unicode terminating null character" (lpCommandLine).
const MAX_COMMAND_LINE: usize = 32_767;

/// The UI restrictions of every worker's job.
pub(crate) const UI_RESTRICTIONS: JOBOBJECT_BASIC_UI_RESTRICTIONS =
    JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
            | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
            | JOB_OBJECT_UILIMIT_EXITWINDOWS
            | JOB_OBJECT_UILIMIT_GLOBALATOMS
            | JOB_OBJECT_UILIMIT_HANDLES
            | JOB_OBJECT_UILIMIT_READCLIPBOARD
            | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
            | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    };

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

/// UTF-16 with exactly one NUL, at the end: what every `PCWSTR` parameter
/// here receives. The buffer is never changed after construction, so
/// [`WideCString::as_ptr`] stays valid for as long as the value is borrowed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WideCString(Box<[u16]>);

impl WideCString {
    /// From units without a NUL; fails on an interior NUL.
    pub(crate) fn new(mut units: Vec<u16>) -> io::Result<Self> {
        if units.contains(&0) {
            return Err(invalid("NUL inside a name"));
        }
        units.push(0);
        Ok(Self(units.into_boxed_slice()))
    }

    pub(crate) fn from_str(text: &str) -> io::Result<Self> {
        Self::new(text.encode_utf16().collect())
    }

    /// For `PCWSTR` parameters: valid for reads up to and including the NUL.
    pub(crate) fn as_ptr(&self) -> PCWSTR {
        self.0.as_ptr()
    }

    /// The units without the terminating NUL.
    pub(crate) fn units(&self) -> &[u16] {
        self.0.split_last().map_or(&[], |(_, units)| units)
    }

    #[cfg(test)]
    pub(crate) fn len_with_nul(&self) -> usize {
        self.0.len()
    }
}

/// `folder\name` in UTF-16.
pub(crate) fn join(folder: &[u16], name: &str) -> Vec<u16> {
    let mut path = folder.to_vec();
    if path.last().is_some_and(|&unit| unit != u16::from(b'\\')) {
        path.push(u16::from(b'\\'));
    }
    path.extend(name.encode_utf16());
    path
}

/// `path` without its last `levels` components (`a\b\c`, 2 gives `a`).
pub(crate) fn ancestor(path: &[u16], levels: usize) -> Option<Vec<u16>> {
    let backslash = u16::from(b'\\');
    let mut end = path.len();
    while end > 0 && path[end - 1] == backslash {
        end -= 1;
    }
    for _ in 0..levels {
        end = path[..end].iter().rposition(|&unit| unit == backslash)?;
        while end > 0 && path[end - 1] == backslash {
            end -= 1;
        }
    }
    (end > 0).then(|| path[..end].to_vec())
}

/// A writable, NUL-terminated command line for CreateProcessW: the program
/// and each argument quoted as the C runtime and `CommandLineToArgvW` parse
/// them, at most [`MAX_COMMAND_LINE`] units.
pub(crate) fn command_line(program: &[u16], arguments: &[Vec<u16>]) -> io::Result<Vec<u16>> {
    let mut line = Vec::new();
    append_argument(&mut line, program);
    for argument in arguments {
        append_argument(&mut line, argument);
    }
    if line.contains(&0) {
        return Err(invalid("NUL in an argument"));
    }
    line.push(0);
    if line.len() > MAX_COMMAND_LINE {
        return Err(invalid("the command line is too long"));
    }
    Ok(line)
}

/// A Unicode environment block (CREATE_UNICODE_ENVIRONMENT): `name=value`
/// strings, each NUL-terminated, sorted by name without regard to case, and a
/// final NUL. Names and values with `=` in a name or a NUL are refused.
pub(crate) fn environment_block(variables: &[(&str, Vec<u16>)]) -> io::Result<Vec<u16>> {
    let mut sorted: Vec<_> = variables.iter().collect();
    sorted.sort_by_key(|(name, _)| name.to_uppercase());
    let mut block = Vec::new();
    for (name, value) in sorted {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains(&0) {
            return Err(invalid("an unusable environment variable"));
        }
        block.extend(name.encode_utf16());
        block.push(u16::from(b'='));
        block.extend_from_slice(value);
        block.push(0);
    }
    // An empty block is two NULs; otherwise the last string's NUL and this one.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// The job limits for `policy`: kill on close, one active process, die on an
/// unhandled exception, and the policy's committed-memory and CPU limits.
pub(crate) fn job_limits(policy: &Policy) -> io::Result<JOBOBJECT_EXTENDED_LIMIT_INFORMATION> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let basic = &mut limits.BasicLimitInformation;
    basic.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    basic.ActiveProcessLimit = 1;
    if let Some(seconds) = policy.cpu_seconds {
        basic.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_TIME;
        // In 100-nanosecond ticks.
        basic.PerProcessUserTimeLimit = seconds
            .checked_mul(10_000_000)
            .and_then(|ticks| i64::try_from(ticks).ok())
            .ok_or_else(|| invalid("CPU limit out of range"))?;
    }
    if policy.memory > 0 {
        basic.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        limits.ProcessMemoryLimit =
            usize::try_from(policy.memory).map_err(|_| invalid("memory limit out of range"))?;
    }
    Ok(limits)
}

/// The handles the worker inherits: the offered file and the standard
/// handles that exist, each once (a handle list must not repeat a handle).
pub(crate) fn handle_list(file: Option<HANDLE>, stdio: [HANDLE; 3]) -> Vec<HANDLE> {
    let mut list = Vec::with_capacity(4);
    for handle in file.into_iter().chain(stdio) {
        if usable(handle) && !list.contains(&handle) {
            list.push(handle);
        }
    }
    list
}

/// A standard handle as STARTUPINFO wants it: null when absent.
pub(crate) fn standard_handle(handle: HANDLE) -> HANDLE {
    if usable(handle) {
        handle
    } else {
        ptr::null_mut()
    }
}

fn usable(handle: HANDLE) -> bool {
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

/// A handle value as the worker receives it (`--file-handle N`). Handle
/// values are small integers; an inherited handle has the same value in the
/// child.
pub(crate) fn handle_argument(handle: HANDLE) -> String {
    handle.addr().to_string()
}

/// The handle value [`handle_argument`] wrote. It is never dereferenced as a
/// pointer, only passed to Windows, so it has no provenance.
pub(crate) fn parse_handle(value: &str) -> Option<HANDLE> {
    let raw: usize = value.parse().ok()?;
    let handle = ptr::without_provenance_mut::<c_void>(raw);
    usable(handle).then_some(handle)
}

/// An HRESULT as an I/O error; HRESULT_FROM_WIN32 codes as their Win32 error.
pub(crate) fn hresult_error(result: HRESULT) -> io::Error {
    let bits = result.cast_unsigned();
    if bits & 0xffff_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((bits & 0xffff).cast_signed())
    } else {
        io::Error::other(format!("HRESULT {bits:#010x}"))
    }
}

/// `HRESULT_FROM_WIN32(code)`.
pub(crate) const fn hresult_from_win32(code: u32) -> HRESULT {
    (0x8007_0000 | (code & 0xffff)).cast_signed()
}

/// A Win32 error code as an I/O error.
pub(crate) fn win32_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code.cast_signed())
}

/// Checks that `bytes` is one whole SID: revision 1, at most 15
/// subauthorities, and exactly `8 + 4 * count` bytes (winnt.h, `SID`).
pub(crate) fn check_sid(bytes: &[u8]) -> io::Result<()> {
    match bytes {
        [1, count, ..] if *count <= 15 && bytes.len() == 8 + 4 * usize::from(*count) => Ok(()),
        _ => Err(invalid("not a SID")),
    }
}

/// The length an ACL says it has (`AclSize`, bytes 2 and 3 of its 8-byte
/// header), if the header is one.
pub(crate) fn acl_size(header: &[u8; 8]) -> Option<usize> {
    let size = usize::from(u16::from_le_bytes([header[2], header[3]]));
    (ACL_REVISIONS.contains(&header[0]) && size >= 8).then_some(size)
}

/// Read and run a file: what a container needs on its own executable.
pub(crate) const READ_EXECUTE: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
/// Read, write, create and delete ("Modify"): what a container gets on
/// everything inside its directory.
pub(crate) const MODIFY: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;
/// What a container gets on its directory itself: list it, traverse it,
/// create files and subdirectories and delete entries in it, read its
/// attributes. Not DELETE (it cannot remove or move the directory), not its
/// attributes or extended attributes, not WRITE_DAC or WRITE_OWNER.
pub(crate) const DIRECTORY: u32 = FILE_LIST_DIRECTORY
    | FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | FILE_READ_EA
    | FILE_TRAVERSE
    | FILE_DELETE_CHILD
    | FILE_READ_ATTRIBUTES
    | READ_CONTROL
    | SYNCHRONIZE;

/// Object and container inheritance: an ACE that reaches the contents.
const CONTENTS: u32 = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;

/// What an AppContainer is granted on a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Grant {
    /// Its own executable: [`READ_EXECUTE`] on the file.
    Executable,
    /// A directory it works in: [`DIRECTORY`] on the directory itself and
    /// [`MODIFY`] (inherit-only) on everything in it.
    Directory,
}

/// One access-allowed entry to add for the container: rights and ACE flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) access: u32,
    pub(crate) flags: u32,
}

impl Grant {
    pub(crate) fn entries(self) -> &'static [Entry] {
        match self {
            Self::Executable => &[Entry {
                access: READ_EXECUTE,
                flags: 0,
            }],
            Self::Directory => &[
                Entry {
                    access: DIRECTORY,
                    flags: 0,
                },
                Entry {
                    access: MODIFY,
                    flags: CONTENTS | INHERIT_ONLY_ACE,
                },
            ],
        }
    }

    fn on_object(self) -> u32 {
        match self {
            Self::Executable => READ_EXECUTE,
            Self::Directory => DIRECTORY,
        }
    }

    fn on_contents(self) -> u32 {
        match self {
            Self::Executable => 0,
            Self::Directory => MODIFY,
        }
    }
}

/// How a DACL stands to a [`Grant`] for one SID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// Its entries give exactly what the grant gives (perhaps in more ACEs).
    Granted,
    /// Something is missing; adding the grant's entries fixes it.
    Missing,
    /// An explicit entry for the SID gives more than the grant (such as an
    /// earlier version's Modify on the directory itself): revoke the SID's
    /// entries, then add the grant's.
    Excessive,
}

/// Where `acl` (a whole ACL, header included) stands for `sid` and `grant`.
/// Only ACCESS_ALLOWED entries count, inherited ones only towards what is
/// granted (an entry inherited from the parent cannot be revoked here). A
/// malformed ACL counts as [`State::Missing`], so the grant is written again.
pub(crate) fn grant_state(acl: &[u8], sid: &[u8], grant: Grant) -> State {
    let Some(entries) = allowed_entries(acl) else {
        return State::Missing;
    };
    let (mut object, mut contents) = (0, 0);
    for (flags, mask, _) in entries
        .into_iter()
        .filter(|&(_, _, ace_sid)| ace_sid == sid)
    {
        let explicit = flags & INHERITED_ACE == 0;
        if flags & INHERIT_ONLY_ACE == 0 {
            object |= mask;
            if explicit && mask & !grant.on_object() != 0 {
                return State::Excessive;
            }
        }
        if flags & CONTENTS != 0 {
            if flags & CONTENTS == CONTENTS {
                contents |= mask;
            }
            if explicit && mask & !grant.on_contents() != 0 {
                return State::Excessive;
            }
        }
    }
    if object & grant.on_object() == grant.on_object()
        && contents & grant.on_contents() == grant.on_contents()
    {
        State::Granted
    } else {
        State::Missing
    }
}

/// The ACCESS_ALLOWED entries of a whole ACL as (flags, mask, SID), or
/// `None` if it is malformed.
fn allowed_entries(acl: &[u8]) -> Option<Vec<(u32, u32, &[u8])>> {
    let header = acl.first_chunk::<8>()?;
    if acl_size(header)? != acl.len() {
        return None;
    }
    let count = usize::from(u16::from_le_bytes([acl[4], acl[5]]));
    let mut entries = Vec::new();
    let mut offset = 8;
    for _ in 0..count {
        // ACE_HEADER: type, flags, size (u16); then the mask and the SID.
        let ace = acl.get(offset..)?.first_chunk::<4>()?;
        let size = usize::from(u16::from_le_bytes([ace[2], ace[3]]));
        if size < 8 {
            return None;
        }
        let body = acl.get(offset..offset.checked_add(size)?)?;
        offset += size;
        if ace[0] == ACCESS_ALLOWED_ACE_TYPE {
            let mask = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            entries.push((u32::from(ace[1]), mask, &body[8..]));
        }
    }
    Some(entries)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    /// An ACCESS_ALLOWED ACE for `sid`.
    pub(crate) fn allowed_ace(flags: u8, mask: u32, sid: &[u8]) -> Vec<u8> {
        let size = u16::try_from(8 + sid.len()).unwrap();
        let mut ace = vec![ACCESS_ALLOWED_ACE_TYPE, flags];
        ace.extend(size.to_le_bytes());
        ace.extend(mask.to_le_bytes());
        ace.extend(sid);
        ace
    }

    /// An ACL holding `aces`.
    pub(crate) fn acl(aces: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = aces.concat();
        let size = u16::try_from(8 + body.len()).unwrap();
        let count = u16::try_from(aces.len()).unwrap();
        let mut acl = vec![2, 0];
        acl.extend(size.to_le_bytes());
        acl.extend(count.to_le_bytes());
        acl.extend([0, 0]);
        acl.extend(body);
        acl
    }

    /// S-1-15-2-<a>-<b>.
    pub(crate) fn sid(a: u32, b: u32) -> Vec<u8> {
        let mut sid = vec![1, 3, 0, 0, 0, 0, 0, 15];
        for part in [2, a, b] {
            sid.extend(part.to_le_bytes());
        }
        sid
    }

    #[test]
    fn wide_strings_have_one_nul_at_the_end() {
        let name = WideCString::from_str("nuschel.content").unwrap();
        assert_eq!(name.units(), wide("nuschel.content"));
        assert_eq!(name.len_with_nul(), 16);
        assert!(WideCString::new(vec![u16::from(b'a'), 0, u16::from(b'b')]).is_err());
        assert_eq!(WideCString::from_str("").unwrap().len_with_nul(), 1);
    }

    #[test]
    fn paths_join_and_climb() {
        let folder = wide(r"C:\Users\a\AppData\Local\Packages\nuschel.content\AC");
        assert_eq!(
            join(&folder, "Temp"),
            wide(r"C:\Users\a\AppData\Local\Packages\nuschel.content\AC\Temp")
        );
        assert_eq!(join(&wide(r"C:\"), "x"), wide(r"C:\x"));
        assert_eq!(
            ancestor(&folder, 3).unwrap(),
            wide(r"C:\Users\a\AppData\Local")
        );
        assert_eq!(ancestor(&wide(r"C:\a\"), 1).unwrap(), wide("C:"));
        assert_eq!(ancestor(&wide("C:"), 1), None);
    }

    #[test]
    fn command_lines_are_terminated_and_bounded() {
        let line = command_line(&wide(r"C:\w x\worker.exe"), &[wide("--live")]).unwrap();
        assert_eq!(
            line,
            [wide(r#""C:\w x\worker.exe" --live"#), vec![0]].concat()
        );
        assert!(command_line(&wide("w.exe"), &[vec![u16::from(b'a'), 0]]).is_err());
        assert!(command_line(&wide("w.exe"), &[vec![u16::from(b'a'); MAX_COMMAND_LINE]]).is_err());
    }

    #[test]
    fn environment_blocks_are_sorted_and_double_terminated() {
        let block = environment_block(&[
            ("TEMP", wide(r"C:\t")),
            ("SystemRoot", wide(r"C:\Windows")),
            ("LOCALAPPDATA", wide(r"C:\l")),
        ])
        .unwrap();
        let expected = "LOCALAPPDATA=C:\\l\0SystemRoot=C:\\Windows\0TEMP=C:\\t\0\0";
        assert_eq!(block, wide(expected));
        assert_eq!(environment_block(&[]).unwrap(), [0, 0]);
        assert!(environment_block(&[("A=B", wide("c"))]).is_err());
        assert!(environment_block(&[("A", vec![0])]).is_err());
    }

    #[test]
    fn job_limits_follow_the_policy() {
        let mut policy = Policy {
            profile: "p".into(),
            memory: 512 << 20,
            cpu_seconds: Some(7),
            ..Policy::default()
        };
        let limits = job_limits(&policy).unwrap();
        assert_eq!(limits.ProcessMemoryLimit, 512 << 20);
        assert_eq!(
            limits.BasicLimitInformation.PerProcessUserTimeLimit,
            70_000_000
        );
        assert_eq!(limits.BasicLimitInformation.ActiveProcessLimit, 1);
        let flags = limits.BasicLimitInformation.LimitFlags;
        assert_ne!(flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, 0);
        assert_ne!(flags & JOB_OBJECT_LIMIT_PROCESS_TIME, 0);
        policy.cpu_seconds = Some(u64::MAX);
        assert!(job_limits(&policy).is_err());
        policy.cpu_seconds = None;
        policy.memory = 0;
        let flags = job_limits(&policy)
            .unwrap()
            .BasicLimitInformation
            .LimitFlags;
        assert_eq!(
            flags & (JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_PROCESS_TIME),
            0
        );
    }

    #[test]
    fn handle_lists_hold_each_usable_handle_once() {
        let handle = |raw| ptr::without_provenance_mut::<c_void>(raw);
        let list = handle_list(
            Some(handle(0x44)),
            [handle(0x8), INVALID_HANDLE_VALUE, handle(0x8)],
        );
        assert_eq!(list, [handle(0x44), handle(0x8)]);
        assert_eq!(handle_list(None, [ptr::null_mut(); 3]), []);
        assert_eq!(
            parse_handle(&handle_argument(handle(0x44))),
            Some(handle(0x44))
        );
        assert_eq!(parse_handle("0"), None);
        assert_eq!(parse_handle("x"), None);
        assert!(standard_handle(INVALID_HANDLE_VALUE).is_null());
    }

    #[test]
    fn errors_keep_their_codes() {
        assert_eq!(
            hresult_error(hresult_from_win32(183)).raw_os_error(),
            Some(183)
        );
        assert_eq!(
            hresult_error(0x8000_4005_u32.cast_signed()).raw_os_error(),
            None
        );
        assert_eq!(win32_error(5).raw_os_error(), Some(5));
    }

    #[test]
    fn sids_are_checked_whole() {
        assert!(check_sid(&sid(1, 2)).is_ok());
        assert!(check_sid(&sid(1, 2)[..19]).is_err());
        assert!(check_sid(&[1, 16, 0, 0, 0, 0, 0, 15]).is_err());
        assert!(check_sid(&[]).is_err());
    }

    #[test]
    fn grant_states_tell_missing_from_excessive() {
        let me = sid(1, 2);
        let other = sid(3, 4);
        let flags = |flags: u32| u8::try_from(flags).unwrap();
        let contents = flags(CONTENTS | INHERIT_ONLY_ACE);
        let directory = acl(&[
            allowed_ace(0, 0x001f_01ff, &other),
            allowed_ace(0, DIRECTORY, &me),
            allowed_ace(contents, MODIFY, &me),
        ]);
        assert_eq!(
            grant_state(&directory, &me, Grant::Directory),
            State::Granted
        );
        assert_eq!(
            grant_state(&directory, &sid(1, 3), Grant::Directory),
            State::Missing
        );
        // The directory alone, or only its contents, is not enough.
        let only_contents = acl(&[allowed_ace(contents, MODIFY, &me)]);
        assert_eq!(
            grant_state(&only_contents, &me, Grant::Directory),
            State::Missing
        );
        let only_itself = acl(&[allowed_ace(0, DIRECTORY, &me)]);
        assert_eq!(
            grant_state(&only_itself, &me, Grant::Directory),
            State::Missing
        );
        // Modify on the directory itself (DELETE, attributes) is too much.
        let broad = acl(&[allowed_ace(flags(CONTENTS), MODIFY, &me)]);
        assert_eq!(grant_state(&broad, &me, Grant::Directory), State::Excessive);
        // ...unless it was inherited from the parent, which only adds.
        let inherited = acl(&[
            allowed_ace(flags(CONTENTS | INHERITED_ACE), MODIFY, &me),
            allowed_ace(0, DIRECTORY, &me),
        ]);
        assert_eq!(
            grant_state(&inherited, &me, Grant::Directory),
            State::Granted
        );
        // The executable: read and execute, nothing more.
        let executable = acl(&[allowed_ace(0, READ_EXECUTE, &me)]);
        assert_eq!(
            grant_state(&executable, &me, Grant::Executable),
            State::Granted
        );
        let writable = acl(&[allowed_ace(0, MODIFY, &me)]);
        assert_eq!(
            grant_state(&writable, &me, Grant::Executable),
            State::Excessive
        );
        assert_eq!(
            grant_state(&acl(&[]), &me, Grant::Executable),
            State::Missing
        );
        // Denials never count as a grant; malformed ACLs read as missing.
        let mut denied = allowed_ace(0, READ_EXECUTE, &me);
        denied[0] = 1;
        assert_eq!(
            grant_state(&acl(&[denied]), &me, Grant::Executable),
            State::Missing
        );
        let mut truncated = executable.clone();
        truncated.pop();
        assert_eq!(
            grant_state(&truncated, &me, Grant::Executable),
            State::Missing
        );
        let mut lying = acl(&[allowed_ace(0, READ_EXECUTE, &other)]);
        lying[4] = 9;
        assert_eq!(grant_state(&lying, &me, Grant::Executable), State::Missing);
        let mut short_ace = executable.clone();
        short_ace[10] = 4;
        assert_eq!(
            grant_state(&short_ace, &me, Grant::Executable),
            State::Missing
        );
        assert_eq!(grant_state(&[], &me, Grant::Executable), State::Missing);
        // The grant's own entries produce the granted state.
        let entries: Vec<Vec<u8>> = Grant::Directory
            .entries()
            .iter()
            .map(|entry| allowed_ace(flags(entry.flags), entry.access, &me))
            .collect();
        assert_eq!(
            grant_state(&acl(&entries), &me, Grant::Directory),
            State::Granted
        );
    }
}
