// SPDX-License-Identifier: EUPL-1.2
//! The thin layer over Win32: one method per function the launch stage calls,
//! each implemented for Windows by exactly one call ([`Win32`]) and, in tests,
//! by a mock that checks every pointer under Miri (`crate::mock`).
//!
//! A method is `unsafe` when the function reads or writes through caller
//! pointers, or takes ownership of a handle; its `# Safety` section is the
//! function's contract from Microsoft's documentation (linked; read on
//! 2026-11-11), with what ReactOS's implementation shows where it has one.
//! ReactOS has no AppContainer support at all: no declaration of the four
//! AppContainer functions anywhere in its tree, process attribute lists only
//! as stubs, and a CreateProcessW that ignores `lpAttributeList`. (Since the
//! worker executables import the AppContainer functions, they do not even
//! load where those are missing, so a missing sandbox cannot go unnoticed.) Methods that take only handles, plain values
//! and Rust references are safe: Windows checks every handle value against
//! the process's handle table and fails with ERROR_INVALID_HANDLE, so no
//! value can make them touch memory they should not.
//!
//! Return values are passed through unchanged; callers check them (`BOOL` 0,
//! negative `HRESULT`, nonzero `WIN32_ERROR`) and read [`Api::last_error`]
//! only where the function documents setting it.
#![allow(unsafe_code)]

use std::{ffi::c_void, io};

use windows_sys::{
    Win32::{
        Foundation::{HANDLE, HLOCAL, WAIT_EVENT, WIN32_ERROR},
        Security::{ACL, Authorization::EXPLICIT_ACCESS_W, PSECURITY_DESCRIPTOR, PSID},
        System::{
            JobObjects::{JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION},
            Threading::{
                LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
                STARTUPINFOW,
            },
        },
    },
    core::{BOOL, HRESULT, PCWSTR, PWSTR},
};

pub(crate) trait Api {
    /// [GetLastError](https://learn.microsoft.com/en-us/windows/win32/api/errhandlingapi/nf-errhandlingapi-getlasterror):
    /// the calling thread's last-error code. Meaningful only right after a
    /// function that documents setting it has failed.
    fn last_error(&self) -> u32;

    // ----------------------------------------------- AppContainer profiles

    /// [CreateAppContainerProfile](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-createappcontainerprofile)
    /// (userenv.dll, Windows 8): creates the profile `name` without
    /// capabilities and returns its SID. Returns `S_OK`, or an HRESULT such
    /// as `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)` when the profile exists
    /// (the documented sample then derives the SID). The page does not say
    /// whether `*sid` is written on failure, so callers set it to null first
    /// and take ownership only on success. ReactOS does not implement it.
    ///
    /// # Safety
    /// `name` (at most 64 characters), `display_name` (at most 512) and
    /// `description` (at most 2048) point to NUL-terminated UTF-16 strings,
    /// read during the call only. `sid` is valid and aligned for one `PSID`
    /// write; on success it receives a SID the caller must release with
    /// `FreeSid` exactly once.
    unsafe fn create_app_container_profile(
        &self,
        name: PCWSTR,
        display_name: PCWSTR,
        description: PCWSTR,
        sid: *mut PSID,
    ) -> HRESULT;

    /// [DeriveAppContainerSidFromAppContainerName](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-deriveappcontainersidfromappcontainername)
    /// (Windows 8): the SID of profile `name`. ReactOS does not implement it.
    ///
    /// # Safety
    /// `name` points to a NUL-terminated UTF-16 string read during the call;
    /// `sid` is valid for one `PSID` write and on success receives a SID the
    /// caller must release with `FreeSid` exactly once.
    unsafe fn derive_app_container_sid_from_app_container_name(
        &self,
        name: PCWSTR,
        sid: *mut PSID,
    ) -> HRESULT;

    /// [GetAppContainerFolderPath](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-getappcontainerfolderpath)
    /// (Windows 8): the container's folder, `%LOCALAPPDATA%\Packages\<name>\AC`.
    /// ReactOS does not implement it.
    ///
    /// # Safety
    /// `sid_string` points to the NUL-terminated string form of an
    /// AppContainer SID, read during the call; `path` is valid for one `PWSTR`
    /// write and on success receives a NUL-terminated string the caller must
    /// release with `CoTaskMemFree` exactly once.
    unsafe fn get_app_container_folder_path(&self, sid_string: PCWSTR, path: *mut PWSTR)
    -> HRESULT;

    /// [CoTaskMemFree](https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-cotaskmemfree):
    /// releases memory from `CoTaskMemAlloc` (a null pointer is ignored).
    ///
    /// # Safety
    /// `memory` is null or was allocated by the COM task allocator for this
    /// caller and is not used again.
    unsafe fn co_task_mem_free(&self, memory: *const c_void);

    // --------------------------------------------------------------- SIDs

    /// [DeriveCapabilitySidsFromName](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-derivecapabilitysidsfromname)
    /// (Windows 10): the group and capability SIDs for a capability name.
    /// Returns nonzero on success; on failure `GetLastError` says why.
    /// Microsoft's AppContainer sample uses only the first capability SID.
    /// ReactOS does not implement it.
    ///
    /// # Safety
    /// `name` points to a NUL-terminated UTF-16 string. The four out-pointers
    /// are each valid for one write. On success `*group_sids` and `*sids` are
    /// arrays of `*group_count` and `*sid_count` SIDs; every SID and both
    /// arrays must be released with `LocalFree`, each exactly once.
    unsafe fn derive_capability_sids_from_name(
        &self,
        name: PCWSTR,
        group_sids: *mut *mut PSID,
        group_count: *mut u32,
        sids: *mut *mut PSID,
        sid_count: *mut u32,
    ) -> BOOL;

    /// [ConvertSidToStringSidW](https://learn.microsoft.com/en-us/windows/win32/api/sddl/nf-sddl-convertsidtostringsidw):
    /// formats a SID as `S-1-...`. Nonzero on success, else `GetLastError`.
    /// ReactOS (advapi32 wine/security.c) allocates the string with
    /// `LocalAlloc(LMEM_FIXED)` and leaves `*text` alone on most failures.
    ///
    /// # Safety
    /// `sid` points to a valid SID, read during the call; `text` is valid for
    /// one write and on success receives a NUL-terminated string the caller
    /// must release with `LocalFree` exactly once.
    unsafe fn convert_sid_to_string_sid(&self, sid: PSID, text: *mut PWSTR) -> BOOL;

    /// [IsValidSid](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-isvalidsid):
    /// nonzero if the SID's revision and subauthority count are valid; sets
    /// no last error. Windows faults on a null pointer (and ReactOS's
    /// `RtlValidSid` even calls null valid), so callers check for null first.
    ///
    /// # Safety
    /// `sid` points to readable memory holding at least a SID header.
    unsafe fn is_valid_sid(&self, sid: PSID) -> BOOL;

    /// [GetLengthSid](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-getlengthsid):
    /// a valid SID's length in bytes (ReactOS: `RtlLengthSid`,
    /// `8 + 4 * SubAuthorityCount`); undefined for an invalid SID, so call
    /// [`Api::is_valid_sid`] first.
    ///
    /// # Safety
    /// `sid` points to a valid SID.
    unsafe fn get_length_sid(&self, sid: PSID) -> u32;

    /// [FreeSid](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-freesid):
    /// releases a SID from `AllocateAndInitializeSid` and the functions that
    /// document `FreeSid` (here the AppContainer profile functions). In
    /// ReactOS FreeSid, LocalFree and CoTaskMemFree all free the process heap,
    /// but Windows promises no such thing: each allocation goes back to the
    /// function its documentation names.
    ///
    /// # Safety
    /// `sid` came from such a function, is released only here, and is not
    /// used again.
    unsafe fn free_sid(&self, sid: PSID);

    /// [LocalFree](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-localfree):
    /// releases a local-heap allocation; returns null on success. A null
    /// argument is ignored.
    ///
    /// # Safety
    /// `memory` is null or was allocated with `LocalAlloc` for this caller
    /// (as the returning function documents) and is not used again.
    unsafe fn local_free(&self, memory: HLOCAL) -> HLOCAL;

    // --------------------------------------------------------------- ACLs

    /// [GetNamedSecurityInfoW](https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-getnamedsecurityinfow)
    /// for a file or directory's DACL (`SE_FILE_OBJECT`,
    /// `DACL_SECURITY_INFORMATION`, owner, group and SACL not requested).
    /// Returns `ERROR_SUCCESS` or a Win32 error code (not via
    /// `GetLastError`). ReactOS (ntmarta) sets the requested out-pointers to
    /// null first, allocates a self-relative descriptor with
    /// `LocalAlloc(LMEM_FIXED)`, points `*dacl` inside it, and frees it again
    /// on failure.
    ///
    /// # Safety
    /// `name` points to a NUL-terminated path. `dacl` and `descriptor` are
    /// each valid for one write. On success `*descriptor` is a security
    /// descriptor the caller must release with `LocalFree` exactly once, and
    /// `*dacl` points into it (or is null for a NULL DACL): it is valid only
    /// until the descriptor is released, and is not freed on its own.
    unsafe fn get_named_security_info(
        &self,
        name: PCWSTR,
        dacl: *mut *mut ACL,
        descriptor: *mut PSECURITY_DESCRIPTOR,
    ) -> WIN32_ERROR;

    /// [SetEntriesInAclW](https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-setentriesinaclw)
    /// with one entry: a new ACL that merges `entry` into `old` (GRANT_ACCESS
    /// combines with the trustee's existing rights). Returns `ERROR_SUCCESS`
    /// or a Win32 error code. ReactOS sets `*new` to null first, reads the
    /// trustee SID during the call only (GetLengthSid, then a copy), and
    /// allocates the result with `LocalAlloc`; unlike Windows it drops the old
    /// entries and the inheritance flags, which matters only on ReactOS.
    ///
    /// # Safety
    /// `entry` points to one initialised `EXPLICIT_ACCESS_W` whose trustee,
    /// with `TRUSTEE_IS_SID`, has `ptstrName` pointing to a valid SID; both
    /// are read during the call only. `old` is null or a valid ACL, read
    /// only. `new` is valid for one write and on success receives an ACL the
    /// caller must release with `LocalFree` exactly once.
    unsafe fn set_entries_in_acl(
        &self,
        entry: *const EXPLICIT_ACCESS_W,
        old: *const ACL,
        new: *mut *mut ACL,
    ) -> WIN32_ERROR;

    /// [SetNamedSecurityInfoW](https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-setnamedsecurityinfow)
    /// replacing a file or directory's DACL (`SE_FILE_OBJECT`,
    /// `DACL_SECURITY_INFORMATION`); entries inherited from the parent are
    /// recomputed, and inheritable entries propagate to existing children.
    /// Returns `ERROR_SUCCESS` or a Win32 error code. ReactOS builds a
    /// descriptor on its stack that points at the caller's ACL for the call
    /// only (and does not propagate).
    ///
    /// # Safety
    /// `name` points to a NUL-terminated path and `dacl` to a valid ACL; both
    /// are read during the call only.
    unsafe fn set_named_security_info(&self, name: PCWSTR, dacl: *const ACL) -> WIN32_ERROR;

    // -------------------------------------------------------------- files

    /// Creates `path` and its parents if missing (`std::fs::create_dir_all`).
    fn create_directory_all(&self, path: &[u16]) -> io::Result<()>;

    // --------------------------------------------------------------- jobs

    /// [CreateJobObjectW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createjobobjectw)
    /// with default security and no name: a new job handle the caller owns
    /// and must close, or null on failure (`GetLastError`).
    fn create_job_object(&self) -> HANDLE;

    /// [SetInformationJobObject](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-setinformationjobobject)
    /// with `JobObjectExtendedLimitInformation`: the structure is copied
    /// during the call. Nonzero on success, else `GetLastError`. The length
    /// is the structure's size (ReactOS rejects any other; its kernel probes
    /// and captures the input, keeping no pointer).
    fn set_job_limits(&self, job: HANDLE, limits: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION) -> BOOL;

    /// [SetInformationJobObject](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-setinformationjobobject)
    /// with `JobObjectBasicUIRestrictions` (exactly 4 bytes), copied during
    /// the call.
    fn set_job_ui_restrictions(
        &self,
        job: HANDLE,
        restrictions: &JOBOBJECT_BASIC_UI_RESTRICTIONS,
    ) -> BOOL;

    /// [AssignProcessToJobObject](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject):
    /// nonzero on success, else `GetLastError`. Since Windows 8 a process
    /// already in a job can be assigned to another (nested jobs); the page
    /// says a job with UI limits cannot be nested, yet the VM check found the
    /// worker in this job with all its limits while its launch stage was in
    /// the supervisor's job (docs/windows-sandbox.md). ReactOS supports no
    /// nesting (STATUS_ACCESS_DENIED).
    fn assign_process_to_job_object(&self, job: HANDLE, process: HANDLE) -> BOOL;

    // ---------------------------------------------------- process creation

    /// [InitializeProcThreadAttributeList](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-initializeprocthreadattributelist):
    /// with a null `list`, stores the size needed for `count` attributes in
    /// `*size` and fails with `ERROR_INSUFFICIENT_BUFFER`; with a buffer of
    /// at least that size, initialises it. Nonzero on success. No alignment
    /// is documented; the sample uses HeapAlloc memory (16-byte aligned on
    /// x64), which the caller matches. ReactOS only has a stub (not exported
    /// by default builds).
    ///
    /// # Safety
    /// `size` is valid for reading and writing one `usize`. `list` is null,
    /// or points to `*size` writable bytes that stay allocated and in place
    /// until [`Api::delete_proc_thread_attribute_list`] has been called on
    /// it exactly once.
    unsafe fn initialize_proc_thread_attribute_list(
        &self,
        list: LPPROC_THREAD_ATTRIBUTE_LIST,
        count: u32,
        size: *mut usize,
    ) -> BOOL;

    /// [UpdateProcThreadAttribute](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute):
    /// adds one attribute. "lpValue … must persist until the attribute list
    /// is destroyed using the DeleteProcThreadAttributeList function": the
    /// list stores the pointer, not a copy, and Windows reads the value (and
    /// whatever it points to, such as the SIDs of a SECURITY_CAPABILITIES)
    /// when the process is created. Nonzero on success, else `GetLastError`.
    /// ReactOS only has a stub; the (unbuilt) Wine code in its tree stores
    /// `entry->value = value`.
    ///
    /// # Safety
    /// `list` was initialised by [`Api::initialize_proc_thread_attribute_list`]
    /// and not yet deleted. `value` points to `size` initialised bytes of the
    /// type `attribute` requires, which stay valid, unmoved and unchanged
    /// until the list is deleted (and in particular across every
    /// `CreateProcessW` that uses it).
    unsafe fn update_proc_thread_attribute(
        &self,
        list: LPPROC_THREAD_ATTRIBUTE_LIST,
        attribute: usize,
        value: *const c_void,
        size: usize,
    ) -> BOOL;

    /// [DeleteProcThreadAttributeList](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-deleteprocthreadattributelist).
    ///
    /// # Safety
    /// `list` was initialised and is deleted only once; its buffer may be
    /// freed afterwards, not before.
    unsafe fn delete_proc_thread_attribute_list(&self, list: LPPROC_THREAD_ATTRIBUTE_LIST);

    /// [CreateProcessW](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw)
    /// with default process and thread security. Nonzero on success, else
    /// `GetLastError`; on success `*information` holds new process and
    /// thread handles the caller owns and must close. ReactOS zeroes
    /// `*information` first, copies `sizeof(STARTUPINFOW)` and the
    /// environment block during the call (keeping neither), writes a
    /// temporary NUL into the command line only when `application` is null,
    /// and ignores the attribute list altogether.
    ///
    /// # Safety
    /// - `application` points to a NUL-terminated path.
    /// - `command_line` points to a writable NUL-terminated buffer of at most
    ///   32,768 units: CreateProcessW may modify it during the call.
    /// - `environment`, with `CREATE_UNICODE_ENVIRONMENT`, points to a block
    ///   of NUL-terminated `name=value` UTF-16 strings ended by an extra NUL.
    /// - `directory` points to a NUL-terminated path.
    /// - `startup` points to a `STARTUPINFOEXW` (with
    ///   `EXTENDED_STARTUPINFO_PRESENT` in `flags`) whose `cb` is its size and
    ///   whose `lpAttributeList` is an initialised list whose values are all
    ///   valid (see [`Api::update_proc_thread_attribute`]); the standard
    ///   handles in it are inheritable and in the list's handle list.
    /// - `information` is valid for one `PROCESS_INFORMATION` write.
    #[allow(clippy::too_many_arguments)]
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
    ) -> BOOL;

    // ------------------------------------------------ processes and handles

    /// [ResumeThread](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-resumethread):
    /// the previous suspend count, or `u32::MAX` on failure.
    fn resume_thread(&self, thread: HANDLE) -> u32;

    /// [TerminateProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess).
    fn terminate_process(&self, process: HANDLE, exit_code: u32) -> BOOL;

    /// [WaitForSingleObject](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitforsingleobject):
    /// `WAIT_OBJECT_0` once signalled, `WAIT_FAILED` on failure.
    fn wait_for_single_object(&self, handle: HANDLE, milliseconds: u32) -> WAIT_EVENT;

    /// [GetExitCodeProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getexitcodeprocess):
    /// `STILL_ACTIVE` (259) while running, so wait first. The code is valid
    /// only on success (ReactOS may write it on a failure path).
    fn get_exit_code_process(&self, process: HANDLE, code: &mut u32) -> BOOL;

    /// [CloseHandle](https://learn.microsoft.com/en-us/windows/win32/api/handleapi/nf-handleapi-closehandle).
    ///
    /// # Safety
    /// The caller owns `handle`, closes it only here, and nothing uses the
    /// value afterwards (it may be reused for another object: I/O safety).
    unsafe fn close_handle(&self, handle: HANDLE) -> BOOL;

    /// [SetHandleInformation](https://learn.microsoft.com/en-us/windows/win32/api/handleapi/nf-handleapi-sethandleinformation).
    fn set_handle_information(&self, handle: HANDLE, mask: u32, flags: u32) -> BOOL;

    /// [GetHandleInformation](https://learn.microsoft.com/en-us/windows/win32/api/handleapi/nf-handleapi-gethandleinformation):
    /// writes the flags on success only (ReactOS).
    fn get_handle_information(&self, handle: HANDLE, flags: &mut u32) -> BOOL;

    /// [GetFileType](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfiletype):
    /// `FILE_TYPE_DISK` for a file; `FILE_TYPE_UNKNOWN` both for an unknown
    /// type and on failure, so only `FILE_TYPE_DISK` is trusted here.
    fn get_file_type(&self, handle: HANDLE) -> u32;
}

#[cfg(windows)]
pub(crate) use win32::Win32;

#[cfg(windows)]
mod win32 {
    use std::{ffi::c_void, io, os::windows::ffi::OsStringExt as _, ptr};

    use windows_sys::{
        Win32::{
            Foundation::{
                CloseHandle, GetHandleInformation, GetLastError, HANDLE, HLOCAL, LocalFree,
                SetHandleInformation, WAIT_EVENT, WIN32_ERROR,
            },
            Security::{
                ACL,
                Authorization::{
                    ConvertSidToStringSidW, EXPLICIT_ACCESS_W, GetNamedSecurityInfoW,
                    SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW,
                },
                DACL_SECURITY_INFORMATION, DeriveCapabilitySidsFromName, FreeSid, GetLengthSid,
                IsValidSid,
                Isolation::{
                    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
                    GetAppContainerFolderPath,
                },
                PSECURITY_DESCRIPTOR, PSID,
            },
            Storage::FileSystem::GetFileType,
            System::{
                Com::CoTaskMemFree,
                JobObjects::{
                    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_UI_RESTRICTIONS,
                    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
                    JobObjectExtendedLimitInformation, SetInformationJobObject,
                },
                Threading::{
                    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
                    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
                    PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW,
                    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
                },
            },
        },
        core::{BOOL, HRESULT, PCWSTR, PWSTR},
    };

    use super::Api;

    /// The real thing. Each method is one call; for the `unsafe` ones the
    /// caller upholds the contract in the trait's `# Safety` section, which
    /// is the function's documented contract, and the other arguments are
    /// constants that function accepts.
    pub(crate) struct Win32;

    impl Api for Win32 {
        fn last_error(&self) -> u32 {
            // SAFETY: no arguments; reads the calling thread's error value.
            unsafe { GetLastError() }
        }

        unsafe fn create_app_container_profile(
            &self,
            name: PCWSTR,
            display_name: PCWSTR,
            description: PCWSTR,
            sid: *mut PSID,
        ) -> HRESULT {
            // SAFETY: the caller upholds the trait contract for the strings
            // and `sid`; no capabilities (null array, count 0).
            unsafe {
                CreateAppContainerProfile(name, display_name, description, ptr::null(), 0, sid)
            }
        }

        unsafe fn derive_app_container_sid_from_app_container_name(
            &self,
            name: PCWSTR,
            sid: *mut PSID,
        ) -> HRESULT {
            // SAFETY: the caller upholds the trait contract.
            unsafe { DeriveAppContainerSidFromAppContainerName(name, sid) }
        }

        unsafe fn get_app_container_folder_path(
            &self,
            sid_string: PCWSTR,
            path: *mut PWSTR,
        ) -> HRESULT {
            // SAFETY: the caller upholds the trait contract.
            unsafe { GetAppContainerFolderPath(sid_string, path) }
        }

        unsafe fn co_task_mem_free(&self, memory: *const c_void) {
            // SAFETY: the caller upholds the trait contract.
            unsafe { CoTaskMemFree(memory) }
        }

        unsafe fn derive_capability_sids_from_name(
            &self,
            name: PCWSTR,
            group_sids: *mut *mut PSID,
            group_count: *mut u32,
            sids: *mut *mut PSID,
            sid_count: *mut u32,
        ) -> BOOL {
            // SAFETY: the caller upholds the trait contract.
            unsafe { DeriveCapabilitySidsFromName(name, group_sids, group_count, sids, sid_count) }
        }

        unsafe fn convert_sid_to_string_sid(&self, sid: PSID, text: *mut PWSTR) -> BOOL {
            // SAFETY: the caller upholds the trait contract.
            unsafe { ConvertSidToStringSidW(sid, text) }
        }

        unsafe fn is_valid_sid(&self, sid: PSID) -> BOOL {
            // SAFETY: the caller upholds the trait contract.
            unsafe { IsValidSid(sid) }
        }

        unsafe fn get_length_sid(&self, sid: PSID) -> u32 {
            // SAFETY: the caller upholds the trait contract.
            unsafe { GetLengthSid(sid) }
        }

        unsafe fn free_sid(&self, sid: PSID) {
            // SAFETY: the caller upholds the trait contract. FreeSid's
            // return value (null on success) carries no error information.
            unsafe { FreeSid(sid) };
        }

        unsafe fn local_free(&self, memory: HLOCAL) -> HLOCAL {
            // SAFETY: the caller upholds the trait contract.
            unsafe { LocalFree(memory) }
        }

        unsafe fn get_named_security_info(
            &self,
            name: PCWSTR,
            dacl: *mut *mut ACL,
            descriptor: *mut PSECURITY_DESCRIPTOR,
        ) -> WIN32_ERROR {
            // SAFETY: the caller upholds the trait contract; owner, group
            // and SACL are not requested, so their out-pointers may be null.
            unsafe {
                GetNamedSecurityInfoW(
                    name,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    dacl,
                    ptr::null_mut(),
                    descriptor,
                )
            }
        }

        unsafe fn set_entries_in_acl(
            &self,
            entry: *const EXPLICIT_ACCESS_W,
            old: *const ACL,
            new: *mut *mut ACL,
        ) -> WIN32_ERROR {
            // SAFETY: the caller upholds the trait contract; one entry.
            unsafe { SetEntriesInAclW(1, entry, old, new) }
        }

        unsafe fn set_named_security_info(&self, name: PCWSTR, dacl: *const ACL) -> WIN32_ERROR {
            // SAFETY: the caller upholds the trait contract; only the DACL
            // is set, so the owner, group and SACL arguments are null.
            unsafe {
                SetNamedSecurityInfoW(
                    name,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    dacl,
                    ptr::null(),
                )
            }
        }

        fn create_directory_all(&self, path: &[u16]) -> io::Result<()> {
            std::fs::create_dir_all(std::ffi::OsString::from_wide(path))
        }

        fn create_job_object(&self) -> HANDLE {
            // SAFETY: both pointer arguments may be null (default security
            // descriptor, unnamed job).
            unsafe { CreateJobObjectW(ptr::null(), ptr::null()) }
        }

        fn set_job_limits(
            &self,
            job: HANDLE,
            limits: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        ) -> BOOL {
            let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .unwrap_or(u32::MAX);
            // SAFETY: the class matches the structure, `limits` is a live
            // reference to `size` initialised bytes, read during the call
            // only (ReactOS: NtSetInformationJobObject probes and copies it).
            unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    ptr::from_ref(limits).cast::<c_void>(),
                    size,
                )
            }
        }

        fn set_job_ui_restrictions(
            &self,
            job: HANDLE,
            restrictions: &JOBOBJECT_BASIC_UI_RESTRICTIONS,
        ) -> BOOL {
            let size =
                u32::try_from(size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>()).unwrap_or(u32::MAX);
            // SAFETY: as in `set_job_limits`, for the UI restrictions class.
            unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectBasicUIRestrictions,
                    ptr::from_ref(restrictions).cast::<c_void>(),
                    size,
                )
            }
        }

        fn assign_process_to_job_object(&self, job: HANDLE, process: HANDLE) -> BOOL {
            // SAFETY: handles only; Windows validates them.
            unsafe { AssignProcessToJobObject(job, process) }
        }

        unsafe fn initialize_proc_thread_attribute_list(
            &self,
            list: LPPROC_THREAD_ATTRIBUTE_LIST,
            count: u32,
            size: *mut usize,
        ) -> BOOL {
            // SAFETY: the caller upholds the trait contract; flags are
            // reserved and must be 0.
            unsafe { InitializeProcThreadAttributeList(list, count, 0, size) }
        }

        unsafe fn update_proc_thread_attribute(
            &self,
            list: LPPROC_THREAD_ATTRIBUTE_LIST,
            attribute: usize,
            value: *const c_void,
            size: usize,
        ) -> BOOL {
            // SAFETY: the caller upholds the trait contract; flags must be
            // 0, and no previous value or return size is requested (both
            // reserved, null).
            unsafe {
                UpdateProcThreadAttribute(
                    list,
                    0,
                    attribute,
                    value,
                    size,
                    ptr::null_mut(),
                    ptr::null(),
                )
            }
        }

        unsafe fn delete_proc_thread_attribute_list(&self, list: LPPROC_THREAD_ATTRIBUTE_LIST) {
            // SAFETY: the caller upholds the trait contract.
            unsafe { DeleteProcThreadAttributeList(list) }
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
            // SAFETY: the caller upholds the trait contract; null process
            // and thread attributes mean default security, non-inheritable
            // handles.
            unsafe {
                CreateProcessW(
                    application,
                    command_line,
                    ptr::null(),
                    ptr::null(),
                    inherit_handles,
                    flags,
                    environment,
                    directory,
                    startup,
                    information,
                )
            }
        }

        fn resume_thread(&self, thread: HANDLE) -> u32 {
            // SAFETY: a handle only.
            unsafe { ResumeThread(thread) }
        }

        fn terminate_process(&self, process: HANDLE, exit_code: u32) -> BOOL {
            // SAFETY: a handle and a value only.
            unsafe { TerminateProcess(process, exit_code) }
        }

        fn wait_for_single_object(&self, handle: HANDLE, milliseconds: u32) -> WAIT_EVENT {
            // SAFETY: a handle and a value only.
            unsafe { WaitForSingleObject(handle, milliseconds) }
        }

        fn get_exit_code_process(&self, process: HANDLE, code: &mut u32) -> BOOL {
            // SAFETY: `code` is a live, exclusive reference: valid and
            // aligned for the one u32 written.
            unsafe { GetExitCodeProcess(process, code) }
        }

        unsafe fn close_handle(&self, handle: HANDLE) -> BOOL {
            // SAFETY: the caller upholds the trait contract (owns the handle).
            unsafe { CloseHandle(handle) }
        }

        fn set_handle_information(&self, handle: HANDLE, mask: u32, flags: u32) -> BOOL {
            // SAFETY: a handle and values only.
            unsafe { SetHandleInformation(handle, mask, flags) }
        }

        fn get_handle_information(&self, handle: HANDLE, flags: &mut u32) -> BOOL {
            // SAFETY: `flags` is a live, exclusive reference for one u32.
            unsafe { GetHandleInformation(handle, flags) }
        }

        fn get_file_type(&self, handle: HANDLE) -> u32 {
            // SAFETY: a handle only.
            unsafe { GetFileType(handle) }
        }
    }
}
