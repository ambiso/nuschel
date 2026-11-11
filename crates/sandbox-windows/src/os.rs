// SPDX-License-Identifier: EUPL-1.2
//! Safe operations over [`Api`]: every `unsafe` call to Windows in this crate
//! is in this module, one call per block, and everything Windows allocates
//! or opens for us is owned by a guard that releases it exactly once, with
//! the function its documentation names, on every path.
//!
//! Lifetimes carry the rules Windows imposes: an [`AttributeList`] borrows
//! every value it points to (and [`Capabilities`] the SIDs it points to), so
//! none can be moved or dropped before the list is deleted.
#![allow(unsafe_code)]

use std::{ffi::c_void, io, marker::PhantomData, ptr};

use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, HANDLE_FLAG_INHERIT,
            INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        },
        Security::{
            ACL,
            Authorization::{
                ACCESS_MODE, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
                TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
            },
            NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, READ_CONTROL, WRITE_DAC,
        },
        System::{
            JobObjects::{JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION},
            SystemServices::SE_GROUP_ENABLED,
            Threading::{
                CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS,
                EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST,
                PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
                PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION,
                STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
            },
        },
    },
    core::{BOOL, PWSTR},
};

use crate::{
    api::Api,
    plan::{
        Grant, State, WideCString, acl_size, check_sid, grant_state, hresult_error,
        hresult_from_win32, invalid, parse_handle, win32_error,
    },
    raw,
};

/// The calling thread's last error as an I/O error. Read it right after the
/// failing call, before any guard is dropped (a release call may change it).
fn last_error(api: &impl Api) -> io::Error {
    win32_error(api.last_error())
}

/// The function that releases an [`OsMemory`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Free {
    Local,
    CoTask,
    Sid,
}

/// Memory a Win32 function allocated for us, released once when dropped.
pub(crate) struct OsMemory<'a, A: Api> {
    pointer: *mut c_void,
    free: Free,
    api: &'a A,
}

impl<'a, A: Api> OsMemory<'a, A> {
    /// # Safety
    /// `pointer` is null or was allocated for the caller by a function whose
    /// documentation says to release it with `free`, and nothing else
    /// releases it.
    pub(crate) unsafe fn new(api: &'a A, pointer: *mut c_void, free: Free) -> Self {
        Self { pointer, free, api }
    }

    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.pointer
    }

    pub(crate) fn is_null(&self) -> bool {
        self.pointer.is_null()
    }
}

impl<A: Api> Drop for OsMemory<'_, A> {
    fn drop(&mut self) {
        if self.pointer.is_null() {
            return;
        }
        match self.free {
            // SAFETY: by `new`'s contract the pointer came from LocalAlloc for
            // us and only this guard releases it; `drop` runs once. The return
            // value (the pointer again on failure) leaves nothing to do.
            Free::Local => unsafe {
                self.api.local_free(self.pointer);
            },
            // SAFETY: as above, for the COM task allocator.
            Free::CoTask => unsafe { self.api.co_task_mem_free(self.pointer) },
            // SAFETY: as above, for SIDs documented to be freed with FreeSid.
            Free::Sid => unsafe { self.api.free_sid(self.pointer) },
        }
    }
}

/// A handle this process owns, closed once when dropped.
pub(crate) struct Handle<'a, A: Api> {
    raw: HANDLE,
    api: &'a A,
}

impl<'a, A: Api> Handle<'a, A> {
    /// `None` for null (how the functions used here report failure).
    ///
    /// # Safety
    /// A non-null `raw` is a handle returned to the caller to close, which
    /// nothing else closes or uses after this guard is dropped.
    pub(crate) unsafe fn new(api: &'a A, raw: HANDLE) -> Option<Self> {
        (!raw.is_null()).then_some(Self { raw, api })
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.raw
    }

    /// Gives up ownership without closing; the caller takes it over.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn into_raw(self) -> HANDLE {
        let raw = self.raw;
        std::mem::forget(self);
        raw
    }
}

impl<A: Api> Drop for Handle<'_, A> {
    fn drop(&mut self) {
        // SAFETY: owned by this guard (`new`'s contract) and closed only
        // here, once; the value is not used afterwards.
        unsafe { self.api.close_handle(self.raw) };
    }
}

/// A SID in memory this crate owns, 4-byte aligned as SIDs require.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sid {
    words: Vec<u32>,
    length: usize,
}

impl Sid {
    pub(crate) fn from_bytes(bytes: &[u8]) -> io::Result<Self> {
        check_sid(bytes)?;
        let words = bytes
            .chunks(4)
            .map(|chunk| {
                let mut word = [0; 4];
                word[..chunk.len()].copy_from_slice(chunk);
                u32::from_ne_bytes(word)
            })
            .collect();
        Ok(Self {
            words,
            length: bytes.len(),
        })
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut bytes: Vec<u8> = self
            .words
            .iter()
            .flat_map(|word| word.to_ne_bytes())
            .collect();
        bytes.truncate(self.length);
        bytes
    }

    /// For functions that read a SID: points to `self.length` initialised,
    /// 4-aligned bytes, valid while `self` is borrowed and not changed.
    /// Nothing here passes it to a function that writes through it.
    pub(crate) fn as_psid(&self) -> PSID {
        self.words.as_ptr().cast_mut().cast::<c_void>()
    }

    /// A copy of a SID Windows returned.
    ///
    /// # Safety
    /// `sid` is null or points to a SID Windows returned, readable and
    /// unchanged for the whole call (its owner outlives the call).
    pub(crate) unsafe fn copy_from<A: Api>(api: &A, sid: PSID) -> io::Result<Self> {
        if sid.is_null() {
            return Err(invalid("no SID"));
        }
        // SAFETY: non-null and readable (caller); IsValidSid reads only the
        // header (ReactOS: RtlValidSid) and needs no last-error handling.
        if unsafe { api.is_valid_sid(sid) } == 0 {
            return Err(invalid("not a valid SID"));
        }
        // SAFETY: a valid SID (checked just above); GetLengthSid only reads it.
        let length = usize::try_from(unsafe { api.get_length_sid(sid) })
            .map_err(|_| invalid("SID length out of range"))?;
        // SAFETY: a valid SID is exactly GetLengthSid bytes long
        // (`8 + 4 * SubAuthorityCount`), all readable and unchanged during
        // the copy (caller); the copy is checked again by `from_bytes`.
        let bytes = unsafe { raw::copy_bytes(sid.cast_const(), length) };
        Self::from_bytes(&bytes)
    }
}

/// The SID of AppContainer profile `name`, creating the profile if needed.
pub(crate) fn app_container_sid<A: Api>(api: &A, name: &WideCString) -> io::Result<Sid> {
    let display = WideCString::from_str("Nuschel worker")?;
    let mut sid: PSID = ptr::null_mut();
    // SAFETY: `name` and `display` are NUL-terminated (WideCString) and
    // borrowed across the call; `sid` is a local PSID, aligned and valid for
    // the one write.
    let mut result = unsafe {
        api.create_app_container_profile(
            name.as_ptr(),
            display.as_ptr(),
            display.as_ptr(),
            &raw mut sid,
        )
    };
    if result == hresult_from_win32(183) {
        // ERROR_ALREADY_EXISTS: nothing was allocated; ask for its SID.
        sid = ptr::null_mut();
        // SAFETY: as above.
        result = unsafe {
            api.derive_app_container_sid_from_app_container_name(name.as_ptr(), &raw mut sid)
        };
    }
    if result < 0 {
        return Err(hresult_error(result));
    }
    // SAFETY: on success both functions return a SID to be released with
    // FreeSid (their documentation); this guard is its only owner.
    let owned = unsafe { OsMemory::new(api, sid, Free::Sid) };
    // SAFETY: `owned` holds the SID Windows returned and outlives the copy.
    unsafe { Sid::copy_from(api, owned.as_ptr()) }
}

/// A capability's SID: the well-known ones Windows' firewall checks for
/// network access (built here, no call needed), others derived from the
/// name (device capabilities such as `microphone`).
pub(crate) fn capability_sid<A: Api>(api: &A, name: &str) -> io::Result<Sid> {
    let well_known =
        |rid: u8| Sid::from_bytes(&[1, 2, 0, 0, 0, 0, 0, 15, 3, 0, 0, 0, rid, 0, 0, 0]);
    match name {
        // S-1-15-3-1, -2, -3 (winnt.h SECURITY_CAPABILITY_*).
        "internetClient" => well_known(1),
        "internetClientServer" => well_known(2),
        "privateNetworkClientServer" => well_known(3),
        _ => derived_capability_sid(api, name),
    }
}

fn derived_capability_sid<A: Api>(api: &A, name: &str) -> io::Result<Sid> {
    let name = WideCString::from_str(name)?;
    let (mut groups, mut group_count) = (ptr::null_mut(), 0_u32);
    let (mut sids, mut sid_count) = (ptr::null_mut(), 0_u32);
    // SAFETY: `name` is NUL-terminated and borrowed across the call; the
    // four out-pointers are distinct locals, each aligned and valid for one
    // write.
    let derived = unsafe {
        api.derive_capability_sids_from_name(
            name.as_ptr(),
            &raw mut groups,
            &raw mut group_count,
            &raw mut sids,
            &raw mut sid_count,
        )
    };
    if derived == 0 {
        return Err(last_error(api));
    }
    // SAFETY: on success both arrays and every SID in them were allocated
    // with LocalAlloc for us, with the counts written beside them (the
    // function's documentation); these guards are their only owners.
    let _groups = unsafe { LocalSids::new(api, groups, group_count) }?;
    // SAFETY: as above.
    let sids = unsafe { LocalSids::new(api, sids, sid_count) }?;
    let first = sids.first().ok_or_else(|| invalid("no capability SID"))?;
    // SAFETY: `first` is owned by `sids`, which outlives the copy.
    unsafe { Sid::copy_from(api, first) }
}

/// An array of SIDs that Windows allocated, every SID and the array itself
/// with LocalAlloc.
struct LocalSids<'a, A: Api> {
    sids: Vec<OsMemory<'a, A>>,
    _array: OsMemory<'a, A>,
}

impl<'a, A: Api> LocalSids<'a, A> {
    /// # Safety
    /// `array` is null, or was allocated with LocalAlloc for the caller and
    /// holds `count` initialised pointers to SIDs each allocated with
    /// LocalAlloc for the caller; nothing else releases any of them.
    unsafe fn new(api: &'a A, array: *mut PSID, count: u32) -> io::Result<Self> {
        // SAFETY: the array is LocalAlloc'd for us (caller); this guard is
        // its only owner, so it is released once even if a step below fails.
        let array_guard = unsafe { OsMemory::new(api, array.cast::<c_void>(), Free::Local) };
        let pointers = if array.is_null() {
            Vec::new()
        } else {
            let count = usize::try_from(count).map_err(|_| invalid("SID count out of range"))?;
            // SAFETY: the array holds `count` initialised PSIDs (caller), is
            // aligned for them (LocalAlloc returns memory aligned for any
            // type, MEMORY_ALLOCATION_ALIGNMENT) and nothing writes it.
            unsafe { raw::copy_array(array.cast_const(), count) }
        };
        let sids = pointers
            .into_iter()
            // SAFETY: each element is a LocalAlloc'd SID owned by us
            // (caller) and given to exactly one guard here.
            .map(|sid| unsafe { OsMemory::new(api, sid, Free::Local) })
            .collect();
        Ok(Self {
            sids,
            _array: array_guard,
        })
    }

    fn first(&self) -> Option<PSID> {
        self.sids.first().map(OsMemory::as_ptr)
    }
}

/// `%LOCALAPPDATA%\Packages\<profile>\AC`, the container's own folder.
pub(crate) fn container_folder<A: Api>(api: &A, sid: &Sid) -> io::Result<Vec<u16>> {
    let mut text: PWSTR = ptr::null_mut();
    // SAFETY: `sid.as_psid()` points to the valid SID `sid` owns, borrowed
    // across the call and only read; `text` is a local valid for one write.
    if unsafe { api.convert_sid_to_string_sid(sid.as_psid(), &raw mut text) } == 0 {
        return Err(last_error(api));
    }
    // SAFETY: on success a NUL-terminated string LocalAlloc'd for us (the
    // function's documentation); this guard is its only owner.
    let text = unsafe { OsMemory::new(api, text.cast::<c_void>(), Free::Local) };
    if text.is_null() {
        return Err(invalid("no SID string"));
    }
    let mut path: PWSTR = ptr::null_mut();
    // SAFETY: `text` holds the NUL-terminated SID string and outlives the
    // call; `path` is a local valid for one write.
    let result = unsafe {
        api.get_app_container_folder_path(text.as_ptr().cast_const().cast::<u16>(), &raw mut path)
    };
    if result < 0 {
        return Err(hresult_error(result));
    }
    // SAFETY: on success a NUL-terminated string from CoTaskMemAlloc for us
    // (the function's documentation); this guard is its only owner.
    let path = unsafe { OsMemory::new(api, path.cast::<c_void>(), Free::CoTask) };
    if path.is_null() {
        return Err(invalid("no AppContainer folder"));
    }
    // SAFETY: a NUL-terminated UTF-16 string (the function's documentation)
    // in task-allocator memory, which is aligned for any type; `path` owns it
    // until after the copy and nothing writes it.
    Ok(unsafe { raw::copy_wide_until_nul(path.as_ptr().cast_const().cast::<u16>()) })
}

/// An ACL in memory this crate owns, 4-byte aligned as ACLs require.
pub(crate) struct AclBuffer {
    words: Vec<u32>,
    length: usize,
}

impl AclBuffer {
    /// A copy of an ACL Windows returned.
    ///
    /// # Safety
    /// `acl` is non-null and points to a valid ACL that stays allocated and
    /// unchanged for the whole call (its owner outlives the call).
    unsafe fn copy_from(acl: *const ACL) -> io::Result<Self> {
        // SAFETY: a valid ACL begins with its 8-byte header (caller).
        let header = unsafe { raw::copy_bytes(acl.cast::<c_void>(), 8) };
        let length = header
            .first_chunk::<8>()
            .and_then(acl_size)
            .ok_or_else(|| invalid("malformed ACL"))?;
        // SAFETY: a valid ACL is `AclSize` bytes long, header, entries and
        // unused space included (winnt.h ACL; ReactOS copies exactly that
        // much into a descriptor), all readable (caller).
        let bytes = unsafe { raw::copy_bytes(acl.cast::<c_void>(), length) };
        Ok(Self::from_bytes(&bytes))
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let words = bytes
            .chunks(4)
            .map(|chunk| {
                let mut word = [0; 4];
                word[..chunk.len()].copy_from_slice(chunk);
                u32::from_ne_bytes(word)
            })
            .collect();
        Self {
            words,
            length: bytes.len(),
        }
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut bytes: Vec<u8> = self
            .words
            .iter()
            .flat_map(|word| word.to_ne_bytes())
            .collect();
        bytes.truncate(self.length);
        bytes
    }

    /// For functions that read an ACL: `self.length` initialised, 4-aligned
    /// bytes, valid while `self` is borrowed.
    fn as_ptr(&self) -> *const ACL {
        self.words.as_ptr().cast::<ACL>()
    }
}

/// Opens `path` itself, never what a junction or symbolic link points to,
/// for reading and changing its DACL, and checks it is what `grant` expects
/// (a directory or a file) and not a reparse point. All later steps use this
/// handle, so the path is looked up once: a worker that replaced its
/// directory with a junction makes the launch fail instead of redirecting
/// the grant.
fn open_itself<'a, A: Api>(
    api: &'a A,
    path: &WideCString,
    grant: Grant,
) -> io::Result<Handle<'a, A>> {
    // SAFETY: `path` is NUL-terminated and borrowed across the call; the
    // rest are values.
    let raw = unsafe {
        api.create_file(
            path.as_ptr(),
            READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(last_error(api));
    }
    // SAFETY: CreateFileW returned a new handle for us to close; this guard
    // is its only owner.
    let file = unsafe { Handle::new(api, raw) }.ok_or_else(|| invalid("no file handle"))?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if api.get_file_information_by_handle(file.raw(), &mut information) == 0 {
        return Err(last_error(api));
    }
    let attributes = information.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid("a junction or link, which is not followed"));
    }
    let directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if directory != (grant == Grant::Directory) {
        return Err(invalid("not the kind of file expected"));
    }
    Ok(file)
}

/// The DACL of an open file, copied; `None` for a NULL DACL.
fn read_dacl<A: Api>(api: &A, file: &Handle<'_, A>) -> io::Result<Option<AclBuffer>> {
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `file` is open with READ_CONTROL (`open_itself`); `dacl` and
    // `descriptor` are distinct locals, each valid for one write.
    let error = unsafe { api.get_security_info(file.raw(), &raw mut dacl, &raw mut descriptor) };
    if error != ERROR_SUCCESS {
        return Err(win32_error(error));
    }
    // SAFETY: on success the descriptor is LocalAlloc'd for us (the
    // function's documentation); this guard is its only owner. `dacl`
    // points into it and is valid as long as this guard lives.
    let descriptor = unsafe { OsMemory::new(api, descriptor, Free::Local) };
    if dacl.is_null() {
        return Ok(None);
    }
    // SAFETY: a valid ACL inside the descriptor, which `descriptor` keeps
    // alive until after the copy; nothing writes it meanwhile.
    let copy = unsafe { AclBuffer::copy_from(dacl) };
    drop(descriptor);
    copy.map(Some)
}

/// An EXPLICIT_ACCESS_W for `sid`. The SID is referenced, not copied: the
/// entry must not be used after the borrow of `sid` it was made from ends.
fn entry(sid: &Sid, mode: ACCESS_MODE, access: u32, flags: u32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: access,
        grfAccessMode: mode,
        grfInheritance: flags,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
            // With TRUSTEE_IS_SID this is the SID, cast (TRUSTEE_W docs).
            ptstrName: sid.as_psid().cast::<u16>(),
        },
    }
}

/// `old` with `entries` merged in, as SetEntriesInAclW makes it.
fn set_entries<A: Api>(
    api: &A,
    entries: &[EXPLICIT_ACCESS_W],
    old: &AclBuffer,
) -> io::Result<AclBuffer> {
    let count = u32::try_from(entries.len()).map_err(|_| invalid("too many entries"))?;
    let mut new: *mut ACL = ptr::null_mut();
    // SAFETY: `entries` holds `count` initialised entries, borrowed across
    // the call, whose trustee SIDs the caller keeps borrowed for at least as
    // long (`entry`); `old` is a valid, 4-aligned ACL this crate owns; both
    // are only read. `new` is a local valid for one write.
    let error =
        unsafe { api.set_entries_in_acl(count, entries.as_ptr(), old.as_ptr(), &raw mut new) };
    if error != ERROR_SUCCESS {
        return Err(win32_error(error));
    }
    // SAFETY: on success the ACL is LocalAlloc'd for us (the function's
    // documentation); this guard is its only owner.
    let new = unsafe { OsMemory::new(api, new.cast::<c_void>(), Free::Local) };
    if new.is_null() {
        return Err(invalid("no ACL"));
    }
    // SAFETY: the valid ACL just built, kept alive by `new` until after the copy.
    unsafe { AclBuffer::copy_from(new.as_ptr().cast_const().cast::<ACL>()) }
}

/// Gives `sid` what `grant` says on `path`, keeping every other entry. The
/// path is opened once, without following links (`open_itself`). Nothing
/// is written if the DACL already gives exactly that; an explicit entry for
/// `sid` that gives more (on the object, or to its contents) is revoked
/// first. The result is read back and checked. Returns whether the DACL
/// changed.
pub(crate) fn grant<A: Api>(
    api: &A,
    path: &WideCString,
    sid: &Sid,
    grant: Grant,
) -> io::Result<bool> {
    let file = open_itself(api, path, grant)?;
    let Some(current) = read_dacl(api, &file)? else {
        // A NULL DACL grants everyone everything.
        return Ok(false);
    };
    let bytes = sid.bytes();
    let base = match grant_state(&current.bytes(), &bytes, grant) {
        State::Granted => return Ok(false),
        State::Missing => current,
        State::Excessive => {
            let revoke = entry(sid, REVOKE_ACCESS, 0, NO_INHERITANCE);
            set_entries(api, &[revoke], &current)?
        }
    };
    let entries: Vec<EXPLICIT_ACCESS_W> = grant
        .entries()
        .iter()
        .map(|wanted| entry(sid, GRANT_ACCESS, wanted.access, wanted.flags))
        .collect();
    let updated = set_entries(api, &entries, &base)?;
    // SAFETY: `file` is open with WRITE_DAC (`open_itself`); `updated` is a
    // valid, 4-aligned ACL this crate owns, only read.
    let error = unsafe { api.set_security_info(file.raw(), updated.as_ptr()) };
    if error != ERROR_SUCCESS {
        return Err(win32_error(error));
    }
    let written = read_dacl(api, &file)?.ok_or_else(|| invalid("the DACL went away"))?;
    if grant_state(&written.bytes(), &bytes, grant) != State::Granted {
        return Err(invalid("the DACL does not hold the grant after writing it"));
    }
    Ok(true)
}

/// Marks a handle this process holds inheritable, as a handle list requires.
pub(crate) fn mark_inheritable<A: Api>(api: &A, handle: HANDLE) -> io::Result<()> {
    if api.set_handle_information(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
        return Err(last_error(api));
    }
    Ok(())
}

/// The handle value the launch stage passed in `--file-handle`, if it names
/// an open file that is not one of the `standard` handles.
pub(crate) fn inherited_handle<A: Api>(
    api: &A,
    value: &str,
    standard: [HANDLE; 3],
) -> io::Result<HANDLE> {
    let handle = parse_handle(value).ok_or_else(|| invalid("--file-handle needs a handle"))?;
    if standard.contains(&handle) {
        return Err(invalid("--file-handle names a standard handle"));
    }
    let mut flags = 0;
    if api.get_handle_information(handle, &mut flags) == 0 {
        return Err(last_error(api));
    }
    if api.get_file_type(handle) != FILE_TYPE_DISK {
        return Err(invalid("--file-handle is not a file"));
    }
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if api.get_file_information_by_handle(handle, &mut information) == 0 {
        return Err(last_error(api));
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(invalid("--file-handle is a directory"));
    }
    Ok(handle)
}

/// A new, non-inheritable handle to the object `handle` names, owned by the
/// returned guard. `handle` itself is left open and untouched.
pub(crate) fn duplicate<A: Api>(api: &A, handle: HANDLE) -> io::Result<Handle<'_, A>> {
    let raw = api.duplicate_handle(handle);
    if raw.is_null() {
        return Err(last_error(api));
    }
    // SAFETY: DuplicateHandle just returned this new handle to us; nothing
    // else knows its value, so this guard is its only owner.
    unsafe { Handle::new(api, raw) }.ok_or_else(|| invalid("no handle"))
}

/// The worker's job; closing it ends the worker (kill on close).
pub(crate) struct Job<'a, A: Api>(Handle<'a, A>);

impl<'a, A: Api> Job<'a, A> {
    pub(crate) fn new(
        api: &'a A,
        limits: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        interface: &JOBOBJECT_BASIC_UI_RESTRICTIONS,
    ) -> io::Result<Self> {
        let raw = api.create_job_object();
        if raw.is_null() {
            return Err(last_error(api));
        }
        // SAFETY: CreateJobObjectW returned a new handle for us to close
        // (its documentation); this guard is its only owner.
        let job = unsafe { Handle::new(api, raw) }.ok_or_else(|| invalid("no job"))?;
        if api.set_job_limits(job.raw(), limits) == 0 {
            return Err(last_error(api));
        }
        if api.set_job_ui_restrictions(job.raw(), interface) == 0 {
            return Err(last_error(api));
        }
        Ok(Self(job))
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.0.raw()
    }
}

/// A `SECURITY_CAPABILITIES` value and the array of capability SIDs it
/// points to, borrowing the SIDs so they outlive it.
pub(crate) struct Capabilities<'s> {
    value: SECURITY_CAPABILITIES,
    /// Pointed to by `value.Capabilities`; never resized, so its buffer
    /// stays where it is when the struct moves.
    _enabled: Vec<SID_AND_ATTRIBUTES>,
    _sids: PhantomData<&'s Sid>,
}

impl<'s> Capabilities<'s> {
    pub(crate) fn new(container: &'s Sid, capabilities: &'s [Sid]) -> io::Result<Self> {
        let mut enabled: Vec<SID_AND_ATTRIBUTES> = capabilities
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.as_psid(),
                Attributes: SE_GROUP_ENABLED.cast_unsigned(),
            })
            .collect();
        let count = u32::try_from(enabled.len()).map_err(|_| invalid("too many capabilities"))?;
        let value = SECURITY_CAPABILITIES {
            AppContainerSid: container.as_psid(),
            Capabilities: if enabled.is_empty() {
                ptr::null_mut()
            } else {
                enabled.as_mut_ptr()
            },
            CapabilityCount: count,
            Reserved: 0,
        };
        Ok(Self {
            value,
            _enabled: enabled,
            _sids: PhantomData,
        })
    }
}

/// Storage for a process attribute list, aligned like the heap memory
/// Windows' own examples use (16 bytes on x64).
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Block([u8; 16]);

/// An initialised process attribute list. Windows keeps pointers to the
/// values, not copies, so each is borrowed for `'v`, which outlives the
/// list: none can move or be dropped before `Drop` deletes the list.
pub(crate) struct AttributeList<'v, A: Api> {
    /// Never resized after initialisation, so the list does not move.
    buffer: Vec<Block>,
    api: &'v A,
    _values: PhantomData<&'v ()>,
}

impl<'v, A: Api> AttributeList<'v, A> {
    pub(crate) fn new(api: &'v A, count: u32) -> io::Result<Self> {
        let mut size = 0_usize;
        // SAFETY: with a null list the function only stores the size it
        // needs in `size`, a local valid for reading and writing.
        let queried = unsafe {
            api.initialize_proc_thread_attribute_list(ptr::null_mut(), count, &raw mut size)
        };
        // The size query fails by design, with ERROR_INSUFFICIENT_BUFFER.
        if queried != 0 || size == 0 {
            return Err(invalid("attribute list size query"));
        }
        let error = api.last_error();
        if error != ERROR_INSUFFICIENT_BUFFER {
            return Err(win32_error(error));
        }
        let mut buffer = vec![Block([0; 16]); size.div_ceil(size_of::<Block>())];
        let mut capacity = buffer
            .len()
            .checked_mul(size_of::<Block>())
            .ok_or_else(|| invalid("attribute list size"))?;
        // SAFETY: `buffer` is `capacity` (at least `size`) zeroed, writable,
        // 16-aligned bytes that this value owns; `as_mut_ptr` creates no
        // reference, and the Vec is never resized, so the list stays at this
        // address until `Drop` deletes it and only then frees the buffer.
        // `capacity` is a local valid for reading and writing.
        let initialized = unsafe {
            api.initialize_proc_thread_attribute_list(
                buffer.as_mut_ptr().cast::<c_void>(),
                count,
                &raw mut capacity,
            )
        };
        if initialized == 0 {
            // Not initialised: no `Self`, so nothing deletes it.
            return Err(last_error(api));
        }
        Ok(Self {
            buffer,
            api,
            _values: PhantomData,
        })
    }

    pub(crate) fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast::<c_void>()
    }

    /// # Safety
    /// `value` points to `size` initialised bytes of the type `attribute`
    /// takes, valid, unmoved and unchanged for `'v`.
    unsafe fn update(
        &mut self,
        attribute: u32,
        value: *const c_void,
        size: usize,
    ) -> io::Result<()> {
        let attribute = usize::try_from(attribute).map_err(|_| invalid("attribute"))?;
        let list = self.as_ptr();
        // SAFETY: `list` was initialised in `new` and is deleted only in
        // `Drop`; `value` meets UpdateProcThreadAttribute's contract (caller),
        // in particular it outlives the list, which keeps the pointer.
        if unsafe {
            self.api
                .update_proc_thread_attribute(list, attribute, value, size)
        } == 0
        {
            return Err(last_error(self.api));
        }
        Ok(())
    }

    /// Creates the process in this AppContainer with these capabilities.
    pub(crate) fn security_capabilities(&mut self, value: &'v Capabilities<'_>) -> io::Result<()> {
        // SAFETY: a whole SECURITY_CAPABILITIES (the attribute's type and
        // size), borrowed for 'v; the SIDs and the capability array it points
        // to are borrowed by `Capabilities` for at least as long.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
                ptr::from_ref(&value.value).cast::<c_void>(),
                size_of::<SECURITY_CAPABILITIES>(),
            )
        }
    }

    /// The only handles the process inherits (each inheritable, none twice).
    pub(crate) fn handle_list(&mut self, handles: &'v [HANDLE]) -> io::Result<()> {
        // SAFETY: an array of handles, its size in bytes, borrowed for 'v.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                handles.as_ptr().cast::<c_void>(),
                size_of_val(handles),
            )
        }
    }

    /// The jobs the process is created in, in order (Windows 10 and later):
    /// it never exists, not even suspended, outside them.
    pub(crate) fn job_list(&mut self, jobs: &'v [HANDLE]) -> io::Result<()> {
        // SAFETY: an array of job handles, its size in bytes, borrowed for 'v.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_JOB_LIST,
                jobs.as_ptr().cast::<c_void>(),
                size_of_val(jobs),
            )
        }
    }

    pub(crate) fn child_process_policy(&mut self, value: &'v u32) -> io::Result<()> {
        // SAFETY: a DWORD (the attribute's type), borrowed for 'v.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
                ptr::from_ref(value).cast::<c_void>(),
                size_of::<u32>(),
            )
        }
    }

    pub(crate) fn mitigation_policy(&mut self, value: &'v u64) -> io::Result<()> {
        // SAFETY: a DWORD64 (one of the attribute's documented sizes),
        // borrowed for 'v.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
                ptr::from_ref(value).cast::<c_void>(),
                size_of::<u64>(),
            )
        }
    }

    pub(crate) fn all_application_packages_policy(&mut self, value: &'v u32) -> io::Result<()> {
        // SAFETY: a DWORD (the attribute's type in Microsoft's AppContainer
        // sample), borrowed for 'v.
        unsafe {
            self.update(
                PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
                ptr::from_ref(value).cast::<c_void>(),
                size_of::<u32>(),
            )
        }
    }
}

impl<A: Api> Drop for AttributeList<'_, A> {
    fn drop(&mut self) {
        let list = self.as_ptr();
        // SAFETY: initialised in `new` (a value exists only then) and deleted
        // only here, once, before the fields (the buffer) are dropped.
        unsafe { self.api.delete_proc_thread_attribute_list(list) };
    }
}

/// A worker process created suspended.
pub(crate) struct Child<'a, A: Api> {
    process: Handle<'a, A>,
    thread: Handle<'a, A>,
}

impl<A: Api> Child<'_, A> {
    pub(crate) fn resume(&self) -> io::Result<()> {
        if self.thread.api.resume_thread(self.thread.raw()) == u32::MAX {
            return Err(last_error(self.thread.api));
        }
        Ok(())
    }

    /// Ends a child that must not run (it is still suspended). It was
    /// created in its job, so the job's kill-on-close stops it too.
    pub(crate) fn terminate(&self) {
        self.process.api.terminate_process(self.process.raw(), 1);
    }

    /// Waits for the child to exit and returns its exit code.
    pub(crate) fn wait(&self) -> io::Result<u32> {
        let api = self.process.api;
        if api.wait_for_single_object(self.process.raw(), INFINITE) != WAIT_OBJECT_0 {
            return Err(last_error(api));
        }
        let mut code = 0;
        if api.get_exit_code_process(self.process.raw(), &mut code) == 0 {
            return Err(last_error(api));
        }
        Ok(code)
    }
}

/// What [`create_process`] runs.
pub(crate) struct Command<'c> {
    pub(crate) application: &'c WideCString,
    /// NUL-terminated (plan::command_line); CreateProcessW may write to it.
    pub(crate) command_line: &'c mut [u16],
    /// Ends in two NULs (plan::environment_block).
    pub(crate) environment: &'c [u16],
    pub(crate) directory: &'c WideCString,
    pub(crate) stdio: [HANDLE; 3],
}

/// Creates `command`, suspended, without a console, with the attributes in
/// `attributes`; handles are inherited only through its handle list (and
/// only if `inherit`).
pub(crate) fn create_process<'a, A: Api>(
    api: &'a A,
    command: Command<'_>,
    attributes: &mut AttributeList<'_, A>,
    inherit: bool,
) -> io::Result<Child<'a, A>> {
    if command.command_line.last() != Some(&0) || !command.environment.ends_with(&[0, 0]) {
        return Err(invalid("unterminated command line or environment"));
    }
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: u32::try_from(size_of::<STARTUPINFOEXW>()).map_err(|_| invalid("size"))?,
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
            hStdInput: command.stdio[0],
            hStdOutput: command.stdio[1],
            hStdError: command.stdio[2],
        },
        lpAttributeList: attributes.as_ptr(),
    };
    // No console at all: a hidden one (CREATE_NO_WINDOW) needs a conhost.exe
    // child, which the child-process policy refuses. STARTF_USESTDHANDLES
    // stays set even when nothing is inherited (all three handles null):
    // with DETACHED_PROCESS it keeps CreateProcessW from handing the child
    // copies of this process's standard handles (ReactOS proc.c does that
    // for a console child without it).
    let flags = EXTENDED_STARTUPINFO_PRESENT
        | CREATE_SUSPENDED
        | DETACHED_PROCESS
        | CREATE_UNICODE_ENVIRONMENT;
    let mut information = PROCESS_INFORMATION {
        hProcess: ptr::null_mut(),
        hThread: ptr::null_mut(),
        dwProcessId: 0,
        dwThreadId: 0,
    };
    // SAFETY:
    // - `application` and `directory` are NUL-terminated (WideCString),
    //   borrowed across the call.
    // - `command_line` is an exclusively borrowed, writable buffer ending in
    //   NUL (checked above), at most 32,768 units (plan::command_line); no
    //   other reference to it exists during the call.
    // - `environment` ends in two NULs (checked above) and holds
    //   `name=value` strings (plan::environment_block), UTF-16 as
    //   CREATE_UNICODE_ENVIRONMENT says.
    // - `startup` is a whole STARTUPINFOEXW whose `cb` is its size, passed
    //   as a pointer to the whole struct (not just its first field), with
    //   EXTENDED_STARTUPINFO_PRESENT; its attribute list is initialised and
    //   alive, and every value in it is borrowed for the list's lifetime
    //   (AttributeList). The standard handles in it are in that handle list
    //   and inheritable (the caller marked them).
    // - `information` is a local, valid for one write.
    let created = unsafe {
        api.create_process(
            command.application.as_ptr(),
            command.command_line.as_mut_ptr(),
            BOOL::from(inherit),
            flags,
            command.environment.as_ptr().cast::<c_void>(),
            command.directory.as_ptr(),
            ptr::from_ref(&startup).cast::<STARTUPINFOW>(),
            &raw mut information,
        )
    };
    if created == 0 {
        return Err(last_error(api));
    }
    // SAFETY: on success both handles were returned for us to close
    // (PROCESS_INFORMATION docs); each guard is the only owner of one.
    let process = unsafe { Handle::new(api, information.hProcess) };
    // SAFETY: as above.
    let thread = unsafe { Handle::new(api, information.hThread) };
    match (process, thread) {
        (Some(process), Some(thread)) => Ok(Child { process, thread }),
        // Whichever handle exists is closed by its guard here.
        _ => Err(invalid("CreateProcessW returned no handles")),
    }
}
