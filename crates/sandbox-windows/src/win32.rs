// SPDX-License-Identifier: EUPL-1.2
//! The Win32 calls behind the launch stage: AppContainer profiles and SIDs,
//! file permissions for the container, a job object, and `CreateProcessW` with
//! an attribute list. This is the one module in Nuschel's own code allowed to
//! use `unsafe` (SECURITY.md, "OS sandbox/process APIs"): there is no
//! maintained safe wrapper that creates AppContainer processes with an
//! explicit handle list (docs/windows-sandbox.md explains the choice).
//!
//! Rules for this module: one unsafe operation per block, each with a
//! `SAFETY:` comment; memory Windows allocates is copied into Rust-owned
//! values and freed where it was received; handles become `OwnedHandle` as
//! soon as they are created; pointers stored by Windows (the attribute list)
//! borrow their values for the list's lifetime.
#![allow(unsafe_code)]

use std::{
    ffi::{OsStr, OsString, c_void},
    fs::{File, OpenOptions},
    io,
    marker::PhantomData,
    mem::{size_of, size_of_val},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    },
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

use nuschel_ipc::windows_sandbox::{FILE_HANDLE_FLAG, Policy, append_argument};
use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
            INVALID_HANDLE_VALUE, LocalFree, SetHandleInformation, WAIT_OBJECT_0,
        },
        Security::{
            ACL,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
                GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW,
                SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
            },
            DACL_SECURITY_INFORMATION, DeriveCapabilitySidsFromName, FreeSid, GetLengthSid,
            Isolation::{
                CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
                GetAppContainerFolderPath,
            },
            NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        },
        Storage::FileSystem::{
            DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
            FILE_TYPE_DISK, GetFileType,
        },
        System::{
            Com::CoTaskMemFree,
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
                JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
                JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS,
                JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS,
                JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
                JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
                JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            SystemServices::SE_GROUP_ENABLED,
            Threading::{
                CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS,
                DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
                INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
                PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
                PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
                STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
                UpdateProcThreadAttribute, WaitForSingleObject,
            },
        },
    },
    core::{HRESULT, PCWSTR, PWSTR},
};

/// Read, write, create and delete inside a granted directory ("Modify").
const MODIFY: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;
/// `PROCESS_CREATION_CHILD_PROCESS_RESTRICTED` (winbase.h): no child processes.
const CHILD_PROCESS_RESTRICTED: u32 = 0x01;
/// `PROCESS_CREATION_ALL_APPLICATION_PACKAGES_OPT_OUT` (winbase.h): a less
/// privileged AppContainer, outside "ALL APPLICATION PACKAGES".
const ALL_APPLICATION_PACKAGES_OPT_OUT: u32 = 0x01;
/// `PROCESS_CREATION_MITIGATION_POLICY_*_ALWAYS_ON` (winnt.h): no legacy
/// extension DLLs (AppInit and similar), no non-system fonts, no images from
/// remote shares or with a low mandatory label.
const MITIGATIONS: u64 = (1 << 32) | (1 << 48) | (1 << 52) | (1 << 56);

/// If this process was started as a worker's launch stage, start the worker
/// confined and exit with its exit code; otherwise return at once.
pub fn launch_if_requested() {
    let (policy, arguments) = match Policy::parse(std::env::args_os().skip(1)) {
        Ok(None) => return,
        Ok(Some(parsed)) => parsed,
        Err(error) => fail(&error),
    };
    match launch(&policy, arguments) {
        // Exit codes are 32-bit on Windows; `exit` passes the bits through.
        Ok(code) => std::process::exit(code as i32),
        Err(error) => fail(&error.to_string()),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("nuschel sandbox: {message}; the worker was not started");
    std::process::exit(crate::LAUNCH_FAILED)
}

/// The file the launch stage opened for this worker (`--file-handle N`).
/// Only the first call can succeed, and only for an inherited disk file that
/// is not one of the standard handles, so no other owner of it exists here.
pub fn inherited_file(value: &OsStr) -> io::Result<File> {
    static TAKEN: AtomicBool = AtomicBool::new(false);
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_owned());
    let raw: usize = value
        .to_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid("--file-handle needs a handle value"))?;
    let handle = raw as HANDLE;
    let standard = [
        io::stdin().as_raw_handle(),
        io::stdout().as_raw_handle(),
        io::stderr().as_raw_handle(),
    ];
    if standard.contains(&handle) {
        return Err(invalid("--file-handle names a standard handle"));
    }
    let mut flags = 0;
    // SAFETY: only reads this process's handle table entry for `handle`;
    // an invalid value makes the call fail, nothing else.
    if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above, a query of the object behind a valid handle.
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
        return Err(invalid("--file-handle is not a file"));
    }
    if TAKEN.swap(true, Ordering::SeqCst) {
        return Err(invalid("the inherited file was already taken"));
    }
    // SAFETY: the launch stage opened this file for this process and put its
    // inherited handle value on our command line; it is a valid handle to a
    // disk file (checked above) that is not a standard handle, and `TAKEN`
    // makes this the only place that ever takes ownership of it.
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

fn launch(policy: &Policy, mut arguments: Vec<OsString>) -> io::Result<u32> {
    let executable = std::env::current_exe()?;
    let container = Sid::app_container(&policy.profile).map_err(context("AppContainer profile"))?;
    let capabilities = policy
        .capabilities
        .iter()
        .map(|name| Sid::capability(name).map_err(context(name)))
        .collect::<io::Result<Vec<_>>>()?;
    // The container must read and run its own image. Where the user may not
    // change the file's permissions (an installation under Program Files),
    // every AppContainer can usually read it already; if not, CreateProcessW
    // below fails and nothing runs unconfined.
    let _ = grant(
        &executable,
        &container,
        FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
        false,
    );
    for directory in &policy.writable {
        grant(directory, &container, MODIFY, true).map_err(context(&directory.display()))?;
    }
    // The container's own folder (%LOCALAPPDATA%\Packages\<profile>\AC) is
    // its working directory and holds its temporary files.
    let folder = container.folder().map_err(context("AppContainer folder"))?;
    let temp = folder.join("Temp");
    std::fs::create_dir_all(&temp)?;
    let file = match &policy.file {
        Some(path) => Some(
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(path)
                .map_err(context(&path.display()))?,
        ),
        None => None,
    };
    let mut inherited = Vec::new();
    if let Some(file) = &file {
        inheritable(file.as_raw_handle())?;
        inherited.push(file.as_raw_handle());
        arguments.push(FILE_HANDLE_FLAG.into());
        arguments.push((file.as_raw_handle() as usize).to_string().into());
    }
    let stdio = [
        io::stdin().as_raw_handle(),
        io::stdout().as_raw_handle(),
        io::stderr().as_raw_handle(),
    ]
    .map(|handle| {
        if handle == INVALID_HANDLE_VALUE {
            ptr::null_mut()
        } else {
            handle
        }
    });
    for handle in stdio {
        if !handle.is_null() && !inherited.contains(&handle) {
            inheritable(handle)?;
            inherited.push(handle);
        }
    }
    let job = Job::new(policy).map_err(context("job object"))?;
    let child = spawn(
        &executable,
        &arguments,
        &container,
        &capabilities,
        policy.less_privileged,
        &inherited,
        stdio,
        &environment(&folder),
        &folder,
    )
    .map_err(context("CreateProcessW"))?;
    if let Err(error) = job.assign(&child.process) {
        child.terminate();
        return Err(context("job assignment")(error));
    }
    child.resume()?;
    drop(file);
    child.wait()
}

fn context<E: std::fmt::Display>(step: impl std::fmt::Display) -> impl FnOnce(E) -> io::Error {
    move |error| io::Error::other(format!("{step}: {error}"))
}

/// Mark a handle of this process inheritable, so it may be in a handle list.
fn inheritable(handle: HANDLE) -> io::Result<()> {
    // SAFETY: changes one flag of a handle this process holds; an invalid
    // value makes the call fail.
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The worker's whole environment: Windows' own directory, the container's
/// temporary directory, and `LOCALAPPDATA` (three levels above the container's
/// folder), without which CreateProcessW refuses an AppContainer process
/// (ERROR_ENVVAR_NOT_FOUND). Sorted as CreateProcessW expects.
fn environment(folder: &Path) -> Vec<u16> {
    let mut variables: Vec<(OsString, OsString)> = ["SystemRoot", "windir"]
        .into_iter()
        .filter_map(|name| Some((name.into(), std::env::var_os(name)?)))
        .collect();
    let temp = folder.join("Temp");
    for name in ["TEMP", "TMP"] {
        variables.push((name.into(), temp.clone().into()));
    }
    if let Some(local) = folder.ancestors().nth(3) {
        variables.push(("LOCALAPPDATA".into(), local.into()));
    }
    variables.sort_by_key(|(name, _)| name.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (name, value) in variables {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    block
}

/// A NUL-terminated UTF-16 copy of `text`, which must contain no NUL.
fn wide(text: &OsStr) -> io::Result<Vec<u16>> {
    let mut units: Vec<u16> = text.encode_wide().collect();
    if units.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "NUL in a name"));
    }
    units.push(0);
    Ok(units)
}

/// # Safety
/// `text` must point to a readable, NUL-terminated UTF-16 string.
unsafe fn read_wide(text: *const u16) -> OsString {
    let mut length = 0;
    loop {
        // SAFETY: every unit up to and including the NUL is readable (caller).
        if unsafe { *text.wrapping_add(length) } == 0 {
            break;
        }
        length += 1;
    }
    // SAFETY: the `length` units before the NUL are readable (caller).
    OsString::from_wide(unsafe { std::slice::from_raw_parts(text, length) })
}

fn hresult(result: HRESULT) -> io::Error {
    // HRESULT_FROM_WIN32 wraps a Win32 error code; show that code's message.
    if result as u32 & 0xffff_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error(result & 0xffff)
    } else {
        io::Error::other(format!("HRESULT {result:#010x}"))
    }
}

/// Memory Windows allocated with `LocalAlloc` and handed to us.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the API that returned this pointer allocated it with
            // LocalAlloc for the caller to free; this is the only free.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// A SID copied into memory this module owns, aligned as SIDs require.
struct Sid(Vec<u32>);

impl Sid {
    /// # Safety
    /// `sid` must point to a valid SID.
    unsafe fn copy(sid: PSID) -> Self {
        // SAFETY: `sid` is a valid SID (caller).
        let length = unsafe { GetLengthSid(sid) } as usize;
        let mut words = vec![0_u32; length.div_ceil(4)];
        // SAFETY: a valid SID is `length` bytes long, and `words` has room
        // for at least that many; the two cannot overlap.
        unsafe {
            ptr::copy_nonoverlapping(sid.cast::<u8>(), words.as_mut_ptr().cast::<u8>(), length)
        };
        Self(words)
    }

    /// For APIs that take a `PSID`; none of those used here write through it.
    fn as_psid(&self) -> PSID {
        self.0.as_ptr().cast_mut().cast()
    }

    /// The SID of the AppContainer profile `name`, created if it is new.
    fn app_container(name: &str) -> io::Result<Self> {
        let name = wide(OsStr::new(name))?;
        let display = wide(OsStr::new("Nuschel worker"))?;
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: NUL-terminated strings, no capabilities (null, 0); `sid`
        // receives a SID allocated for us to release with FreeSid.
        let mut result = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                display.as_ptr(),
                display.as_ptr(),
                ptr::null(),
                0,
                &mut sid,
            )
        };
        if result == (0x8007_0000 | ERROR_ALREADY_EXISTS) as HRESULT {
            // SAFETY: as above.
            result = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        }
        if result < 0 || sid.is_null() {
            return Err(hresult(result));
        }
        // SAFETY: Windows just returned this SID.
        let owned = unsafe { Self::copy(sid) };
        // SAFETY: allocated by the profile API for us; freed only here.
        unsafe { FreeSid(sid) };
        Ok(owned)
    }

    /// A capability SID: the well-known ones the firewall checks for network
    /// access, and derived from the name for device capabilities.
    fn capability(name: &str) -> io::Result<Self> {
        match name {
            "internetClient" => Self::parse("S-1-15-3-1"),
            "internetClientServer" => Self::parse("S-1-15-3-2"),
            "privateNetworkClientServer" => Self::parse("S-1-15-3-3"),
            _ => Self::derive_capability(name),
        }
    }

    fn parse(text: &str) -> io::Result<Self> {
        let text = wide(OsStr::new(text))?;
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: a NUL-terminated string; `sid` receives a LocalAlloc'd SID.
        if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _free = Local(sid);
        // SAFETY: Windows just returned this SID.
        Ok(unsafe { Self::copy(sid) })
    }

    fn derive_capability(name: &str) -> io::Result<Self> {
        let name = wide(OsStr::new(name))?;
        let (mut groups, mut group_count) = (ptr::null_mut(), 0_u32);
        let (mut sids, mut sid_count) = (ptr::null_mut(), 0_u32);
        // SAFETY: a NUL-terminated name and out-pointers to locals; Windows
        // allocates both arrays and every SID in them with LocalAlloc.
        let derived = unsafe {
            DeriveCapabilitySidsFromName(
                name.as_ptr(),
                &mut groups,
                &mut group_count,
                &mut sids,
                &mut sid_count,
            )
        };
        if derived == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success `groups` holds `group_count` SIDs.
        let _groups = unsafe { LocalSids::new(groups, group_count) };
        // SAFETY: on success `sids` holds `sid_count` SIDs.
        let sids = unsafe { LocalSids::new(sids, sid_count) };
        let first = sids
            .first()
            .ok_or_else(|| io::Error::other("no capability SID"))?;
        // SAFETY: a SID from the array Windows just returned.
        Ok(unsafe { Self::copy(first) })
    }

    /// `%LOCALAPPDATA%\Packages\<profile>\AC`, which the container may use.
    fn folder(&self) -> io::Result<PathBuf> {
        let mut text: PWSTR = ptr::null_mut();
        // SAFETY: a valid SID; `text` receives a LocalAlloc'd string.
        if unsafe { ConvertSidToStringSidW(self.as_psid(), &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _free = Local(text.cast());
        let mut path: PWSTR = ptr::null_mut();
        // SAFETY: `text` is the SID's NUL-terminated string form; `path`
        // receives a string allocated with CoTaskMemAlloc.
        let result = unsafe { GetAppContainerFolderPath(text, &mut path) };
        if result < 0 || path.is_null() {
            return Err(hresult(result));
        }
        // SAFETY: Windows returned a NUL-terminated string.
        let folder = unsafe { read_wide(path) };
        // SAFETY: allocated by GetAppContainerFolderPath for us; freed once.
        unsafe { CoTaskMemFree(path.cast()) };
        Ok(folder.into())
    }
}

/// An array of SIDs that Windows allocated, with each SID, by LocalAlloc.
struct LocalSids(*mut PSID, usize);

impl LocalSids {
    /// # Safety
    /// `array` must be null or hold `count` SIDs, all freed with LocalFree.
    unsafe fn new(array: *mut PSID, count: u32) -> Self {
        Self(array, if array.is_null() { 0 } else { count as usize })
    }

    fn first(&self) -> Option<PSID> {
        if self.1 == 0 {
            return None;
        }
        // SAFETY: the array holds `self.1 > 0` SIDs (`new`).
        Some(unsafe { *self.0 })
    }
}

impl Drop for LocalSids {
    fn drop(&mut self) {
        for index in 0..self.1 {
            // SAFETY: index < count, so this reads an element of the array (`new`).
            let sid = unsafe { *self.0.wrapping_add(index) };
            drop(Local(sid));
        }
        drop(Local(self.0.cast()));
    }
}

/// Let `sid` access `path` (and, `inherit`ing, everything in it) with
/// `access`, keeping every other entry. Nothing is written if the
/// container already has that access.
fn grant(path: &Path, sid: &Sid, access: u32, inherit: bool) -> io::Result<()> {
    let name = wide(path.as_os_str())?;
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: a NUL-terminated path and out-pointers to locals; on success
    // `descriptor` is LocalAlloc'd and `dacl` points into it.
    let error = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let _descriptor = Local(descriptor);
    if dacl.is_null() {
        // No DACL at all: everyone, the container included, has full access.
        return Ok(());
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: if inherit {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
            ptstrName: sid.as_psid().cast(),
        },
    };
    let mut updated: *mut ACL = ptr::null_mut();
    // SAFETY: one entry whose trustee SID outlives the call; `dacl` is the
    // object's ACL, valid while `descriptor` lives; `updated` receives a
    // LocalAlloc'd ACL.
    let error = unsafe { SetEntriesInAclW(1, &entry, dacl, &mut updated) };
    if error != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let _updated = Local(updated.cast());
    // SAFETY: both are valid ACLs: the object's, and the one just built.
    if unsafe { same_acl(dacl, updated) } {
        return Ok(());
    }
    // SAFETY: a NUL-terminated path and a valid ACL; only the DACL changes,
    // and entries inherited from the parent are re-derived by Windows.
    let error = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            updated,
            ptr::null(),
        )
    };
    if error != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    Ok(())
}

/// # Safety
/// Both must point to valid ACLs.
unsafe fn same_acl(old: *const ACL, new: *const ACL) -> bool {
    // SAFETY: a valid ACL starts with its header (caller).
    let old_size = usize::from(unsafe { (*old).AclSize });
    // SAFETY: as above.
    let new_size = usize::from(unsafe { (*new).AclSize });
    // SAFETY: a valid ACL is `AclSize` bytes long (caller).
    let old = unsafe { std::slice::from_raw_parts(old.cast::<u8>(), old_size) };
    // SAFETY: as above.
    let new = unsafe { std::slice::from_raw_parts(new.cast::<u8>(), new_size) };
    old == new
}

/// The worker's job: it ends the worker when the launch stage exits or is
/// killed, admits no second process, caps memory (and CPU time if the policy
/// says so), and keeps the worker away from other processes' windows, the
/// clipboard, global atoms, desktops and system settings.
struct Job(OwnedHandle);

impl Job {
    fn new(policy: &Policy) -> io::Result<Self> {
        // SAFETY: default security, no name.
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new handle that nothing else owns.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let basic = &mut limits.BasicLimitInformation;
        basic.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        basic.ActiveProcessLimit = 1;
        if let Some(seconds) = policy.cpu_seconds {
            basic.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_TIME;
            // In 100-nanosecond units.
            basic.PerProcessUserTimeLimit =
                i64::try_from(seconds.saturating_mul(10_000_000)).unwrap_or(i64::MAX);
        }
        if policy.memory > 0 {
            basic.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            limits.ProcessMemoryLimit = usize::try_from(policy.memory).unwrap_or(usize::MAX);
        }
        // SAFETY: the information class matches the structure and its size.
        let set = unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        let interface = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
        };
        // SAFETY: the information class matches the structure and its size.
        let set = unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectBasicUIRestrictions,
                ptr::from_ref(&interface).cast(),
                size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn assign(&self, process: &OwnedHandle) -> io::Result<()> {
        // SAFETY: both handles are owned and open.
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), process.as_raw_handle()) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// A process attribute list. Values are borrowed for the list's lifetime,
/// because Windows keeps pointers to them until the list is deleted.
struct Attributes<'a> {
    buffer: Vec<u64>,
    values: PhantomData<&'a ()>,
}

impl<'a> Attributes<'a> {
    fn new(count: u32) -> io::Result<Self> {
        let mut size = 0;
        // SAFETY: with a null list, Windows only stores the size it needs.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut size) };
        let mut buffer = vec![0_u64; size.div_ceil(8)];
        // SAFETY: `buffer` holds at least `size` bytes, 8-byte aligned.
        let initialized = unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &mut size)
        };
        if initialized == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            buffer,
            values: PhantomData,
        })
    }

    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }

    /// # Safety
    /// `value` must point to `size` bytes of what `attribute` expects, valid
    /// for `'a`.
    unsafe fn update(
        &mut self,
        attribute: u32,
        value: *const c_void,
        size: usize,
    ) -> io::Result<()> {
        // SAFETY: an initialized list; the value is the caller's to vouch for.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                self.list(),
                0,
                attribute as usize,
                value,
                size,
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if updated == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn security_capabilities(&mut self, value: &'a SECURITY_CAPABILITIES) -> io::Result<()> {
        // SAFETY: the attribute takes a SECURITY_CAPABILITIES, borrowed for 'a.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                ptr::from_ref(value).cast(),
                size_of::<SECURITY_CAPABILITIES>(),
            )
        }
    }

    fn handle_list(&mut self, handles: &'a [HANDLE]) -> io::Result<()> {
        // SAFETY: the attribute takes an array of handles, borrowed for 'a.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                handles.as_ptr().cast(),
                size_of_val(handles),
            )
        }
    }

    fn child_process_policy(&mut self, value: &'a u32) -> io::Result<()> {
        // SAFETY: the attribute takes a DWORD, borrowed for 'a.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
                ptr::from_ref(value).cast(),
                size_of::<u32>(),
            )
        }
    }

    fn all_application_packages_policy(&mut self, value: &'a u32) -> io::Result<()> {
        // SAFETY: the attribute takes a DWORD, borrowed for 'a.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
                ptr::from_ref(value).cast(),
                size_of::<u32>(),
            )
        }
    }

    fn mitigation_policy(&mut self, value: &'a u64) -> io::Result<()> {
        // SAFETY: the attribute takes a DWORD64, borrowed for 'a.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
                ptr::from_ref(value).cast(),
                size_of::<u64>(),
            )
        }
    }
}

impl Drop for Attributes<'_> {
    fn drop(&mut self) {
        let list = self.list();
        // SAFETY: initialized in `new`, deleted only here.
        unsafe { DeleteProcThreadAttributeList(list) };
    }
}

/// A worker created suspended, not yet running.
struct Child {
    process: OwnedHandle,
    thread: OwnedHandle,
}

impl Child {
    fn resume(&self) -> io::Result<()> {
        // SAFETY: an owned thread handle.
        if unsafe { ResumeThread(self.thread.as_raw_handle()) } == u32::MAX {
            self.terminate();
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn terminate(&self) {
        // SAFETY: an owned process handle with terminate access.
        unsafe { TerminateProcess(self.process.as_raw_handle(), 1) };
    }

    fn wait(&self) -> io::Result<u32> {
        // SAFETY: an owned process handle.
        if unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut code = 0;
        // SAFETY: an owned process handle and an out-pointer to a local.
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(code)
    }
}

/// Create `executable` suspended in the AppContainer `container` with
/// `capabilities`, inheriting exactly `inherited`.
#[allow(clippy::too_many_arguments)]
fn spawn(
    executable: &Path,
    arguments: &[OsString],
    container: &Sid,
    capabilities: &[Sid],
    less_privileged: bool,
    inherited: &[HANDLE],
    stdio: [HANDLE; 3],
    environment: &[u16],
    directory: &Path,
) -> io::Result<Child> {
    // Everything the attribute list points to lives in this function's
    // frame (or in `container`, `capabilities` and `inherited`, which outlive
    // it) until after CreateProcessW returns.
    let mut enabled: Vec<SID_AND_ATTRIBUTES> = capabilities
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.as_psid(),
            Attributes: SE_GROUP_ENABLED as u32,
        })
        .collect();
    let security = SECURITY_CAPABILITIES {
        AppContainerSid: container.as_psid(),
        Capabilities: if enabled.is_empty() {
            ptr::null_mut()
        } else {
            enabled.as_mut_ptr()
        },
        CapabilityCount: enabled.len() as u32,
        Reserved: 0,
    };
    let child_policy = CHILD_PROCESS_RESTRICTED;
    let mitigations = MITIGATIONS;
    let packages_policy = ALL_APPLICATION_PACKAGES_OPT_OUT;
    let mut attributes = Attributes::new(5)?;
    attributes.security_capabilities(&security)?;
    if !inherited.is_empty() {
        attributes.handle_list(inherited)?;
    }
    attributes.child_process_policy(&child_policy)?;
    attributes.mitigation_policy(&mitigations)?;
    if less_privileged {
        attributes.all_application_packages_policy(&packages_policy)?;
    }
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: size_of::<STARTUPINFOEXW>() as u32,
            lpReserved: ptr::null_mut(),
            lpDesktop: ptr::null_mut(),
            lpTitle: ptr::null_mut(),
            dwX: 0,
            dwY: 0,
            dwXSize: 0,
            dwYSize: 0,
            dwXCountChars: 0,
            dwYCountChars: 0,
            dwFillAttribute: 0,
            dwFlags: STARTF_USESTDHANDLES,
            wShowWindow: 0,
            cbReserved2: 0,
            lpReserved2: ptr::null_mut(),
            hStdInput: stdio[0],
            hStdOutput: stdio[1],
            hStdError: stdio[2],
        },
        lpAttributeList: attributes.list(),
    };
    let application = wide(executable.as_os_str())?;
    let mut command_line = Vec::new();
    append_argument(
        &mut command_line,
        &executable.as_os_str().encode_wide().collect::<Vec<_>>(),
    );
    for argument in arguments {
        append_argument(
            &mut command_line,
            &argument.encode_wide().collect::<Vec<_>>(),
        );
    }
    if command_line.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in an argument",
        ));
    }
    command_line.push(0);
    let directory = wide(directory.as_os_str())?;
    let mut information = PROCESS_INFORMATION {
        hProcess: ptr::null_mut(),
        hThread: ptr::null_mut(),
        dwProcessId: 0,
        dwThreadId: 0,
    };
    // No console at all: a hidden one (CREATE_NO_WINDOW) would need a
    // conhost.exe child, which the child-process policy refuses, and the
    // worker would fail to start (STATUS_DLL_INIT_FAILED).
    let flags = EXTENDED_STARTUPINFO_PRESENT
        | CREATE_SUSPENDED
        | DETACHED_PROCESS
        | CREATE_UNICODE_ENVIRONMENT;
    let application: PCWSTR = application.as_ptr();
    // SAFETY: a NUL-terminated application name, a writable NUL-terminated
    // command line, an environment block ending in two NULs and a
    // NUL-terminated directory; `startup` is a whole STARTUPINFOEXW whose
    // attribute list and everything that list points to are alive (see
    // above); `information` receives the new process's handles. Handles are
    // inherited only from the explicit list (or none when it is empty).
    let created = unsafe {
        CreateProcessW(
            application,
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            i32::from(!inherited.is_empty()),
            flags,
            environment.as_ptr().cast(),
            directory.as_ptr(),
            ptr::from_ref(&startup).cast::<STARTUPINFOW>(),
            &mut information,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Child {
        // SAFETY: CreateProcessW returned these handles to us; nothing else owns them.
        process: unsafe { OwnedHandle::from_raw_handle(information.hProcess) },
        // SAFETY: as above.
        thread: unsafe { OwnedHandle::from_raw_handle(information.hThread) },
    })
}
