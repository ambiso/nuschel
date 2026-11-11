// SPDX-License-Identifier: EUPL-1.2
//! A stand-in for Windows, for tests under Miri. It behaves the way the
//! documentation (and ReactOS, where it implements the function) says:
//! - it allocates what a function returns with the allocator that function
//!   documents, and panics if anything is released twice, with the wrong
//!   function, or not at all (checked when the mock is dropped);
//! - it opens handles that must be closed exactly once;
//! - it reads every pointer it is given as far as Windows would (strings to
//!   their NUL, a SID's whole length, an ACL's `AclSize`), writes every
//!   out-parameter, and writes into buffers Windows writes (the attribute
//!   list, the command line), so Miri reports a short, dangling, misaligned
//!   or read-only buffer;
//! - like Windows, the attribute list keeps the value pointers it is given
//!   and dereferences them only in CreateProcessW, so a value dropped or
//!   moved before then is a use-after-free under Miri;
//! - it can fail the n-th call of any fallible function.
#![allow(unsafe_code)]

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::c_void,
    io, ptr,
};

use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, HLOCAL,
            WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0, WIN32_ERROR,
        },
        Security::{
            ACL,
            Authorization::{EXPLICIT_ACCESS_W, TRUSTEE_IS_SID},
            PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
        },
        Storage::FileSystem::FILE_TYPE_DISK,
        System::{
            JobObjects::{JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION},
            Threading::{
                EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
                PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
                PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_CREATION_FLAGS,
                PROCESS_INFORMATION, STARTUPINFOEXW, STARTUPINFOW,
            },
        },
    },
    core::{BOOL, HRESULT, PCWSTR, PWSTR},
};

use crate::{
    api::Api,
    os::Free,
    plan::{acl_size, hresult_from_win32},
    raw,
};

const E_ACCESSDENIED: HRESULT = hresult_from_win32(ERROR_ACCESS_DENIED);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Job,
    Process,
    Thread,
}

/// What CreateProcessW was given, decoded.
#[derive(Debug, Default)]
pub(crate) struct Created {
    pub(crate) application: String,
    pub(crate) command_line: String,
    pub(crate) environment: Vec<String>,
    pub(crate) directory: String,
    pub(crate) flags: u32,
    pub(crate) inherit: bool,
    pub(crate) stdio: [usize; 3],
    pub(crate) container: Vec<u8>,
    pub(crate) capabilities: Vec<Vec<u8>>,
    pub(crate) handles: Vec<usize>,
    pub(crate) child_policy: Option<u32>,
    pub(crate) mitigations: Option<u64>,
    pub(crate) packages: Option<u32>,
}

/// A live attribute list: its attribute count, its buffer size, and the
/// (attribute, value, size) entries, value pointers kept as Windows keeps them.
type List = (usize, usize, Vec<(usize, *const c_void, usize)>);

#[derive(Default)]
struct State {
    allocations: HashMap<usize, (Free, *mut [u64])>,
    handles: HashMap<usize, Kind>,
    next_handle: usize,
    fail: Option<(&'static str, usize)>,
    counts: HashMap<&'static str, usize>,
    last_error: u32,
    profiles: HashSet<String>,
    dacls: HashMap<String, Vec<u8>>,
    null_dacls: HashSet<String>,
    acl_writes: usize,
    directories: Vec<String>,
    /// Live attribute lists: capacity and the (attribute, value, size)
    /// entries, pointers kept as Windows keeps them.
    lists: HashMap<usize, List>,
    inheritable: HashSet<usize>,
    files: HashSet<usize>,
    created: Option<Created>,
    limits: Option<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>,
    interface: Option<u32>,
    assigned: bool,
    resumed: bool,
    terminated: bool,
    exit_code: u32,
}

pub(crate) struct Mock {
    state: RefCell<State>,
}

impl Mock {
    pub(crate) fn new() -> Self {
        let state = State {
            next_handle: 0x1000,
            exit_code: 7,
            ..State::default()
        };
        Self {
            state: RefCell::new(state),
        }
    }

    /// Fails the `index`-th call (from 0) of the function `name`.
    pub(crate) fn failing(name: &'static str, index: usize) -> Self {
        let mock = Self::new();
        mock.state.borrow_mut().fail = Some((name, index));
        mock
    }

    pub(crate) fn calls(&self) -> HashMap<&'static str, usize> {
        self.state.borrow().counts.clone()
    }

    pub(crate) fn set_dacl(&self, path: &str, acl: Vec<u8>) {
        self.state.borrow_mut().dacls.insert(path.into(), acl);
    }

    pub(crate) fn set_null_dacl(&self, path: &str) {
        self.state.borrow_mut().null_dacls.insert(path.into());
    }

    pub(crate) fn dacl(&self, path: &str) -> Option<Vec<u8>> {
        self.state.borrow().dacls.get(path).cloned()
    }

    pub(crate) fn acl_writes(&self) -> usize {
        self.state.borrow().acl_writes
    }

    pub(crate) fn add_file(&self, handle: HANDLE) {
        self.state.borrow_mut().files.insert(handle.addr());
    }

    pub(crate) fn created(&self) -> Option<Created> {
        self.state.borrow_mut().created.take()
    }

    pub(crate) fn limits(&self) -> Option<JOBOBJECT_EXTENDED_LIMIT_INFORMATION> {
        self.state.borrow().limits
    }

    pub(crate) fn interface(&self) -> Option<u32> {
        self.state.borrow().interface
    }

    pub(crate) fn directories(&self) -> Vec<String> {
        self.state.borrow().directories.clone()
    }

    /// (assigned to the job, resumed, terminated)
    pub(crate) fn outcome(&self) -> (bool, bool, bool) {
        let state = self.state.borrow();
        (state.assigned, state.resumed, state.terminated)
    }

    /// Counts a call and says whether it is the one to fail (setting the
    /// last error as a failing Win32 function does).
    fn fails(&self, name: &'static str) -> bool {
        let mut state = self.state.borrow_mut();
        let count = state.counts.entry(name).or_insert(0);
        let index = *count;
        *count += 1;
        let fail = state.fail == Some((name, index));
        if fail {
            state.last_error = ERROR_ACCESS_DENIED;
        }
        fail
    }

    fn allocate(&self, free: Free, bytes: &[u8]) -> *mut c_void {
        let mut words = vec![0_u64; bytes.len().div_ceil(8).max(1)].into_boxed_slice();
        for (word, chunk) in words.iter_mut().zip(bytes.chunks(8)) {
            let mut eight = [0; 8];
            eight[..chunk.len()].copy_from_slice(chunk);
            *word = u64::from_ne_bytes(eight);
        }
        let allocation = Box::into_raw(words);
        let pointer = allocation.cast::<c_void>();
        self.state
            .borrow_mut()
            .allocations
            .insert(pointer.addr(), (free, allocation));
        pointer
    }

    fn allocate_wide(&self, free: Free, text: &str) -> *mut u16 {
        let bytes: Vec<u8> = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_ne_bytes)
            .collect();
        self.allocate(free, &bytes).cast::<u16>()
    }

    /// An array of pointers, written as pointers so they keep provenance.
    fn allocate_pointers(&self, free: Free, pointers: &[PSID]) -> *mut PSID {
        let array = self
            .allocate(free, &vec![0; size_of_val(pointers)])
            .cast::<PSID>();
        for (index, &pointer) in pointers.iter().enumerate() {
            // Inside the allocation (index < len), so no overflow.
            let slot = array.wrapping_add(index);
            // SAFETY: the allocation holds `pointers.len()` pointer-sized,
            // 8-aligned words, and `slot` is the `index`-th of them.
            unsafe { slot.write(pointer) };
        }
        array
    }

    fn release(&self, free: Free, pointer: *mut c_void) {
        if pointer.is_null() {
            return;
        }
        let (allocator, allocation) = self
            .state
            .borrow_mut()
            .allocations
            .remove(&pointer.addr())
            .unwrap_or_else(|| {
                panic!("{free:?} release of {pointer:p}: not allocated (double free?)")
            });
        assert_eq!(allocator, free, "released with the wrong function");
        // SAFETY: `allocation` came from Box::into_raw in `allocate` and has
        // just been removed from the table, so this is its only release.
        drop(unsafe { Box::from_raw(allocation) });
    }

    fn open(&self, kind: Kind) -> HANDLE {
        let mut state = self.state.borrow_mut();
        state.next_handle += 4;
        let raw = state.next_handle;
        state.handles.insert(raw, kind);
        ptr::without_provenance_mut(raw)
    }

    fn is_open(&self, handle: HANDLE, kind: Kind) -> bool {
        self.state.borrow().handles.get(&handle.addr()) == Some(&kind)
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let state = self.state.get_mut();
        assert!(
            state.allocations.is_empty(),
            "leaked: {:?}",
            state
                .allocations
                .values()
                .map(|(free, _)| free)
                .collect::<Vec<_>>()
        );
        assert!(
            state.handles.is_empty(),
            "handles left open: {:?}",
            state.handles
        );
        assert!(state.lists.is_empty(), "attribute lists not deleted");
    }
}

/// # Safety
/// `text` points to a NUL-terminated UTF-16 string (the API's contract).
unsafe fn read_wide(text: PCWSTR) -> String {
    assert!(!text.is_null() && text.is_aligned());
    // SAFETY: NUL-terminated and aligned (caller, checked).
    String::from_utf16(&unsafe { raw::copy_wide_until_nul(text) }).unwrap()
}

/// # Safety
/// `sid` points to a valid SID (the API's contract).
unsafe fn read_sid(sid: PSID) -> Vec<u8> {
    assert!(!sid.is_null());
    // SAFETY: a valid SID has an 8-byte header (caller).
    let header = unsafe { raw::copy_bytes(sid, 8) };
    let length = 8 + 4 * usize::from(header[1]);
    // SAFETY: a valid SID is `8 + 4 * count` bytes (caller).
    unsafe { raw::copy_bytes(sid, length) }
}

/// # Safety
/// `acl` points to a valid ACL (the API's contract).
unsafe fn read_acl(acl: *const ACL) -> Vec<u8> {
    assert!(!acl.is_null() && acl.is_aligned());
    // SAFETY: a valid ACL has an 8-byte header (caller).
    let header = unsafe { raw::copy_bytes(acl.cast(), 8) };
    let size = acl_size(header.first_chunk::<8>().unwrap()).expect("an ACL");
    // SAFETY: a valid ACL is `AclSize` bytes (caller).
    unsafe { raw::copy_bytes(acl.cast(), size) }
}

fn sid_text(sid: &[u8]) -> String {
    let mut text = format!("S-1-{}", sid[7]);
    for chunk in sid[8..].chunks(4) {
        text.push_str(&format!(
            "-{}",
            u32::from_le_bytes(chunk.try_into().unwrap())
        ));
    }
    text
}

/// A deterministic AppContainer SID for a profile name.
pub(crate) fn container_sid(name: &str) -> Vec<u8> {
    let hash = name.bytes().fold(5381_u32, |hash, byte| {
        hash.wrapping_mul(33) ^ u32::from(byte)
    });
    crate::plan::tests::sid(hash, 7)
}

impl Api for Mock {
    fn last_error(&self) -> u32 {
        self.state.borrow().last_error
    }

    unsafe fn create_app_container_profile(
        &self,
        name: PCWSTR,
        display_name: PCWSTR,
        description: PCWSTR,
        sid: *mut PSID,
    ) -> HRESULT {
        // SAFETY: the three strings are NUL-terminated (contract).
        let name = unsafe { read_wide(name) };
        // SAFETY: as above.
        unsafe { read_wide(display_name) };
        // SAFETY: as above.
        unsafe { read_wide(description) };
        if self.fails("create_app_container_profile") {
            return E_ACCESSDENIED;
        }
        if !self.state.borrow_mut().profiles.insert(name.clone()) {
            return hresult_from_win32(183);
        }
        let allocated = self.allocate(Free::Sid, &container_sid(&name));
        // SAFETY: `sid` is valid for one write (contract).
        unsafe { sid.write(allocated) };
        0
    }

    unsafe fn derive_app_container_sid_from_app_container_name(
        &self,
        name: PCWSTR,
        sid: *mut PSID,
    ) -> HRESULT {
        // SAFETY: NUL-terminated (contract).
        let name = unsafe { read_wide(name) };
        if self.fails("derive_app_container_sid_from_app_container_name") {
            return E_ACCESSDENIED;
        }
        let allocated = self.allocate(Free::Sid, &container_sid(&name));
        // SAFETY: valid for one write (contract).
        unsafe { sid.write(allocated) };
        0
    }

    unsafe fn get_app_container_folder_path(
        &self,
        sid_string: PCWSTR,
        path: *mut PWSTR,
    ) -> HRESULT {
        // SAFETY: NUL-terminated (contract).
        let sid = unsafe { read_wide(sid_string) };
        if self.fails("get_app_container_folder_path") {
            return E_ACCESSDENIED;
        }
        let folder = format!(r"C:\Users\t\AppData\Local\Packages\{sid}\AC");
        let allocated = self.allocate_wide(Free::CoTask, &folder);
        // SAFETY: valid for one write (contract).
        unsafe { path.write(allocated) };
        0
    }

    unsafe fn co_task_mem_free(&self, memory: *const c_void) {
        self.release(Free::CoTask, memory.cast_mut());
    }

    unsafe fn derive_capability_sids_from_name(
        &self,
        name: PCWSTR,
        group_sids: *mut *mut PSID,
        group_count: *mut u32,
        sids: *mut *mut PSID,
        sid_count: *mut u32,
    ) -> BOOL {
        // SAFETY: NUL-terminated (contract).
        let name = unsafe { read_wide(name) };
        if self.fails("derive_capability_sids_from_name") {
            return 0;
        }
        let length = u32::try_from(name.len()).unwrap();
        let group = self.allocate(Free::Local, &crate::plan::tests::sid(32, length));
        let capability = self.allocate(Free::Local, &crate::plan::tests::sid(1024, length));
        let groups = self.allocate_pointers(Free::Local, &[group]);
        let capabilities = self.allocate_pointers(Free::Local, &[capability]);
        // SAFETY: each out-pointer is valid for one write (contract).
        unsafe { group_sids.write(groups) };
        // SAFETY: as above.
        unsafe { group_count.write(1) };
        // SAFETY: as above.
        unsafe { sids.write(capabilities) };
        // SAFETY: as above.
        unsafe { sid_count.write(1) };
        1
    }

    unsafe fn convert_sid_to_string_sid(&self, sid: PSID, text: *mut PWSTR) -> BOOL {
        // SAFETY: a valid SID (contract).
        let sid = unsafe { read_sid(sid) };
        if self.fails("convert_sid_to_string_sid") {
            return 0;
        }
        let allocated = self.allocate_wide(Free::Local, &sid_text(&sid));
        // SAFETY: valid for one write (contract).
        unsafe { text.write(allocated) };
        1
    }

    unsafe fn is_valid_sid(&self, sid: PSID) -> BOOL {
        // Windows faults on null (ReactOS would even say "valid").
        assert!(!sid.is_null(), "IsValidSid(NULL)");
        // SAFETY: readable SID header (contract).
        let header = unsafe { raw::copy_bytes(sid, 8) };
        if self.fails("is_valid_sid") {
            return 0;
        }
        BOOL::from(header[0] == 1 && header[1] <= 15)
    }

    unsafe fn get_length_sid(&self, sid: PSID) -> u32 {
        // SAFETY: a valid SID (contract).
        u32::try_from(unsafe { read_sid(sid) }.len()).unwrap()
    }

    unsafe fn free_sid(&self, sid: PSID) {
        self.release(Free::Sid, sid);
    }

    unsafe fn local_free(&self, memory: HLOCAL) -> HLOCAL {
        self.release(Free::Local, memory);
        ptr::null_mut()
    }

    unsafe fn get_named_security_info(
        &self,
        name: PCWSTR,
        dacl: *mut *mut ACL,
        descriptor: *mut PSECURITY_DESCRIPTOR,
    ) -> WIN32_ERROR {
        // SAFETY: NUL-terminated (contract).
        let path = unsafe { read_wide(name) };
        if self.fails("get_named_security_info") {
            return ERROR_ACCESS_DENIED;
        }
        let state = self.state.borrow();
        let null = state.null_dacls.contains(&path);
        let acl = state.dacls.get(&path).cloned().unwrap_or_else(|| {
            let owner = crate::plan::tests::sid(500, 1);
            crate::plan::tests::acl(&[crate::plan::tests::allowed_ace(3, 0x001f_01ff, &owner)])
        });
        drop(state);
        // A self-relative descriptor: a 24-byte header, then the DACL.
        let mut bytes = vec![1_u8, 0, 4, 0x80];
        bytes.resize(24, 0);
        bytes.extend(&acl);
        let allocated = self.allocate(Free::Local, &bytes);
        let inside = if null {
            ptr::null_mut()
        } else {
            allocated.cast::<u8>().wrapping_add(24).cast::<ACL>()
        };
        // SAFETY: both out-pointers are valid for one write (contract).
        unsafe { descriptor.write(allocated) };
        // SAFETY: as above.
        unsafe { dacl.write(inside) };
        ERROR_SUCCESS
    }

    unsafe fn set_entries_in_acl(
        &self,
        entry: *const EXPLICIT_ACCESS_W,
        old: *const ACL,
        new: *mut *mut ACL,
    ) -> WIN32_ERROR {
        assert!(entry.is_aligned());
        // SAFETY: one initialised entry (contract).
        let entry = unsafe { entry.read() };
        assert_eq!(entry.Trustee.TrusteeForm, TRUSTEE_IS_SID);
        assert!(entry.Trustee.pMultipleTrustee.is_null());
        // SAFETY: with TRUSTEE_IS_SID the name is a valid SID (contract).
        let sid = unsafe { read_sid(entry.Trustee.ptstrName.cast()) };
        // SAFETY: a valid ACL (contract).
        let old = unsafe { read_acl(old) };
        if self.fails("set_entries_in_acl") {
            return ERROR_ACCESS_DENIED;
        }
        let flags = u8::try_from(entry.grfInheritance).unwrap();
        let ace = crate::plan::tests::allowed_ace(flags, entry.grfAccessPermissions, &sid);
        let count = u16::from_le_bytes([old[4], old[5]]);
        let mut aces = vec![old[8..].to_vec()];
        aces.push(ace);
        let mut acl = crate::plan::tests::acl(&aces);
        acl[4..6].copy_from_slice(&(count + 1).to_le_bytes());
        let allocated = self.allocate(Free::Local, &acl).cast::<ACL>();
        // SAFETY: valid for one write (contract).
        unsafe { new.write(allocated) };
        ERROR_SUCCESS
    }

    unsafe fn set_named_security_info(&self, name: PCWSTR, dacl: *const ACL) -> WIN32_ERROR {
        // SAFETY: NUL-terminated (contract).
        let path = unsafe { read_wide(name) };
        // SAFETY: a valid ACL (contract).
        let acl = unsafe { read_acl(dacl) };
        if self.fails("set_named_security_info") {
            return ERROR_ACCESS_DENIED;
        }
        let mut state = self.state.borrow_mut();
        state.acl_writes += 1;
        state.dacls.insert(path, acl);
        ERROR_SUCCESS
    }

    fn create_directory_all(&self, path: &[u16]) -> io::Result<()> {
        if self.fails("create_directory_all") {
            return Err(io::Error::from_raw_os_error(5));
        }
        let path = String::from_utf16(path).unwrap();
        self.state.borrow_mut().directories.push(path);
        Ok(())
    }

    fn create_job_object(&self) -> HANDLE {
        if self.fails("create_job_object") {
            return ptr::null_mut();
        }
        self.open(Kind::Job)
    }

    fn set_job_limits(&self, job: HANDLE, limits: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> BOOL {
        assert!(self.is_open(job, Kind::Job));
        if self.fails("set_job_limits") {
            return 0;
        }
        self.state.borrow_mut().limits = Some(*limits);
        1
    }

    fn set_job_ui_restrictions(
        &self,
        job: HANDLE,
        restrictions: &JOBOBJECT_BASIC_UI_RESTRICTIONS,
    ) -> BOOL {
        assert!(self.is_open(job, Kind::Job));
        if self.fails("set_job_ui_restrictions") {
            return 0;
        }
        self.state.borrow_mut().interface = Some(restrictions.UIRestrictionsClass);
        1
    }

    fn assign_process_to_job_object(&self, job: HANDLE, process: HANDLE) -> BOOL {
        assert!(self.is_open(job, Kind::Job) && self.is_open(process, Kind::Process));
        if self.fails("assign_process_to_job_object") {
            return 0;
        }
        self.state.borrow_mut().assigned = true;
        1
    }

    unsafe fn initialize_proc_thread_attribute_list(
        &self,
        list: LPPROC_THREAD_ATTRIBUTE_LIST,
        count: u32,
        size: *mut usize,
    ) -> BOOL {
        let count = usize::try_from(count).unwrap();
        let needed = 48 + 24 * count;
        if self.fails("initialize_proc_thread_attribute_list") {
            return 0;
        }
        if list.is_null() {
            // SAFETY: valid for a write (contract).
            unsafe { size.write(needed) };
            self.state.borrow_mut().last_error = ERROR_INSUFFICIENT_BUFFER;
            return 0;
        }
        // SAFETY: valid for reading and writing (contract).
        let capacity = unsafe { size.read() };
        assert!(capacity >= needed, "attribute list buffer too small");
        assert_eq!(list.addr() % 8, 0, "attribute list misaligned");
        // Windows fills in the list header: the whole buffer must be writable.
        // SAFETY: `capacity` writable bytes (contract).
        unsafe { list.cast::<u8>().write_bytes(0xa5, capacity) };
        // SAFETY: valid for a write (contract).
        unsafe { size.write(needed) };
        self.state
            .borrow_mut()
            .lists
            .insert(list.addr(), (count, capacity, Vec::new()));
        1
    }

    unsafe fn update_proc_thread_attribute(
        &self,
        list: LPPROC_THREAD_ATTRIBUTE_LIST,
        attribute: usize,
        value: *const c_void,
        size: usize,
    ) -> BOOL {
        if self.fails("update_proc_thread_attribute") {
            return 0;
        }
        let mut state = self.state.borrow_mut();
        let (count, capacity, entries) = state.lists.get_mut(&list.addr()).expect("a live list");
        assert!(
            entries.len() < *count,
            "more attributes than the list was made for"
        );
        // Kept by pointer, as Windows does (Wine's dead code in ReactOS:
        // `entry->value = value`).
        entries.push((attribute, value, size));
        let offset = 48 + 24 * (entries.len() - 1);
        assert!(offset + 8 <= *capacity);
        drop(state);
        // `offset + 8 <= capacity`, checked above.
        let slot = list.cast::<u8>().wrapping_add(offset).cast::<usize>();
        // SAFETY: inside the list's `capacity` writable bytes (contract).
        unsafe { slot.write_unaligned(attribute) };
        1
    }

    unsafe fn delete_proc_thread_attribute_list(&self, list: LPPROC_THREAD_ATTRIBUTE_LIST) {
        let (_, capacity, _) = self
            .state
            .borrow_mut()
            .lists
            .remove(&list.addr())
            .expect("deleting a list that is not live (twice?)");
        // SAFETY: the list's buffer is still allocated (contract).
        unsafe { list.cast::<u8>().write_bytes(0x5a, capacity) };
    }

    unsafe fn create_process(
        &self,
        application: PCWSTR,
        command_line: PWSTR,
        inherit_handles: BOOL,
        flags: PROCESS_CREATION_FLAGS,
        environment: *const c_void,
        directory: PCWSTR,
        startup: *const STARTUPINFOW,
        information: *mut PROCESS_INFORMATION,
    ) -> BOOL {
        let mut created = Created {
            flags,
            inherit: inherit_handles != 0,
            ..Created::default()
        };
        // SAFETY: NUL-terminated strings (contract).
        created.application = unsafe { read_wide(application) };
        // SAFETY: as above.
        created.directory = unsafe { read_wide(directory) };
        // SAFETY: as above.
        created.command_line = unsafe { read_wide(command_line) };
        // Writable (contract): like ReactOS, put a NUL at the first unit and
        // restore it.
        // SAFETY: the buffer holds at least its NUL, so the first unit exists.
        let first = unsafe { command_line.read() };
        // SAFETY: writable (contract).
        unsafe { command_line.write(0) };
        // SAFETY: as above.
        unsafe { command_line.write(first) };
        // An environment block: strings until an empty one (contract).
        let mut next = environment.cast::<u16>();
        loop {
            // SAFETY: as above; `next` is the start of the next string.
            let variable = unsafe { raw::copy_wide_until_nul(next) };
            if variable.is_empty() {
                break;
            }
            next = next.wrapping_add(variable.len() + 1);
            created
                .environment
                .push(String::from_utf16(&variable).unwrap());
        }
        assert_ne!(flags & EXTENDED_STARTUPINFO_PRESENT, 0);
        assert!(startup.is_aligned());
        // SAFETY: with EXTENDED_STARTUPINFO_PRESENT, a whole STARTUPINFOEXW
        // (contract); reading all of it checks the caller passed all of it.
        let extended = unsafe { startup.cast::<STARTUPINFOEXW>().read() };
        assert_eq!(
            usize::try_from(extended.StartupInfo.cb).unwrap(),
            size_of::<STARTUPINFOEXW>()
        );
        let info = extended.StartupInfo;
        created.stdio = [info.hStdInput, info.hStdOutput, info.hStdError].map(<*mut c_void>::addr);
        let entries = {
            let state = self.state.borrow();
            let (_, _, entries) = state
                .lists
                .get(&extended.lpAttributeList.addr())
                .expect("CreateProcessW with a list that is not live");
            entries.clone()
        };
        for (attribute, value, size) in entries {
            let attribute = u32::try_from(attribute).unwrap();
            match attribute {
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES => {
                    assert_eq!(size, size_of::<SECURITY_CAPABILITIES>());
                    // SAFETY: the kept pointer must still be valid (contract):
                    // under Miri this is where a dropped value would show.
                    let security = unsafe { value.cast::<SECURITY_CAPABILITIES>().read() };
                    // SAFETY: a valid SID (contract).
                    created.container = unsafe { read_sid(security.AppContainerSid) };
                    let count = usize::try_from(security.CapabilityCount).unwrap();
                    // SAFETY: `count` entries (contract).
                    let entries: Vec<SID_AND_ATTRIBUTES> =
                        unsafe { raw::copy_array(security.Capabilities, count) };
                    for entry in entries {
                        // SAFETY: each a valid SID (contract).
                        created.capabilities.push(unsafe { read_sid(entry.Sid) });
                    }
                }
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST => {
                    assert_eq!(size % size_of::<HANDLE>(), 0);
                    // SAFETY: `size` bytes of handles (contract).
                    let handles: Vec<HANDLE> = unsafe {
                        raw::copy_array(value.cast::<HANDLE>(), size / size_of::<HANDLE>())
                    };
                    created.handles = handles.iter().map(|handle| handle.addr()).collect();
                }
                PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY => {
                    assert_eq!(size, 4);
                    // SAFETY: a DWORD (contract).
                    created.child_policy = Some(unsafe { value.cast::<u32>().read() });
                }
                PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY => {
                    assert_eq!(size, 8);
                    // SAFETY: a DWORD64 (contract).
                    created.mitigations = Some(unsafe { value.cast::<u64>().read() });
                }
                PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY => {
                    assert_eq!(size, 4);
                    // SAFETY: a DWORD (contract).
                    created.packages = Some(unsafe { value.cast::<u32>().read() });
                }
                other => panic!("unexpected attribute {other:#x}"),
            }
        }
        {
            let state = self.state.borrow();
            let unique: HashSet<_> = created.handles.iter().collect();
            assert_eq!(unique.len(), created.handles.len(), "a handle listed twice");
            for handle in &created.handles {
                assert!(
                    state.inheritable.contains(handle),
                    "listed handle not inheritable"
                );
            }
            for handle in created.stdio.iter().filter(|&&handle| handle != 0) {
                assert!(
                    created.handles.contains(handle),
                    "standard handle not in the list"
                );
            }
        }
        if self.fails("create_process") {
            return 0;
        }
        let process = self.open(Kind::Process);
        let thread = self.open(Kind::Thread);
        self.state.borrow_mut().created = Some(created);
        // SAFETY: valid for one write (contract).
        unsafe {
            information.write(PROCESS_INFORMATION {
                hProcess: process,
                hThread: thread,
                dwProcessId: 1,
                dwThreadId: 2,
            });
        }
        1
    }

    fn resume_thread(&self, thread: HANDLE) -> u32 {
        assert!(self.is_open(thread, Kind::Thread));
        if self.fails("resume_thread") {
            return u32::MAX;
        }
        self.state.borrow_mut().resumed = true;
        1
    }

    fn terminate_process(&self, process: HANDLE, _: u32) -> BOOL {
        assert!(self.is_open(process, Kind::Process));
        self.state.borrow_mut().terminated = true;
        1
    }

    fn wait_for_single_object(&self, handle: HANDLE, _: u32) -> WAIT_EVENT {
        assert!(self.is_open(handle, Kind::Process));
        if self.fails("wait_for_single_object") {
            return WAIT_FAILED;
        }
        WAIT_OBJECT_0
    }

    fn get_exit_code_process(&self, process: HANDLE, code: &mut u32) -> BOOL {
        assert!(self.is_open(process, Kind::Process));
        if self.fails("get_exit_code_process") {
            return 0;
        }
        *code = self.state.borrow().exit_code;
        1
    }

    unsafe fn close_handle(&self, handle: HANDLE) -> BOOL {
        let closed = self.state.borrow_mut().handles.remove(&handle.addr());
        assert!(
            closed.is_some(),
            "closing {handle:p}, which is not open (twice?)"
        );
        1
    }

    fn set_handle_information(&self, handle: HANDLE, mask: u32, flags: u32) -> BOOL {
        if self.fails("set_handle_information") {
            return 0;
        }
        if mask & flags & 1 != 0 {
            self.state.borrow_mut().inheritable.insert(handle.addr());
        }
        1
    }

    fn get_handle_information(&self, handle: HANDLE, flags: &mut u32) -> BOOL {
        if !self.state.borrow().files.contains(&handle.addr()) {
            self.state.borrow_mut().last_error = 6;
            return 0;
        }
        *flags = 1;
        1
    }

    fn get_file_type(&self, handle: HANDLE) -> u32 {
        if self.state.borrow().files.contains(&handle.addr()) {
            FILE_TYPE_DISK
        } else {
            // FILE_TYPE_PIPE, as the standard handles are.
            3
        }
    }
}
