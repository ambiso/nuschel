// SPDX-License-Identifier: EUPL-1.2
//! The launch stage's steps, over any [`Api`]: the real one on Windows, the
//! mock in tests (so that every failure midway is exercised under Miri).
#![forbid(unsafe_code)]

use std::io;

use nuschel_ipc::windows_sandbox::{FILE_HANDLE_FLAG, Policy};
use windows_sys::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE},
};

use crate::{
    api::Api,
    os::{self, AttributeList, Capabilities, Command, Job},
    plan::{self, WideCString},
};

/// Read, write, create and delete inside a granted directory ("Modify").
pub(crate) const MODIFY: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;
/// What the container needs on its own executable.
pub(crate) const READ_EXECUTE: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;

/// What the launch stage starts, gathered by the caller.
pub(crate) struct Inputs {
    /// This executable, which becomes the worker.
    pub(crate) executable: WideCString,
    /// The worker's own arguments.
    pub(crate) arguments: Vec<Vec<u16>>,
    /// `--writable` directories.
    pub(crate) writable: Vec<WideCString>,
    /// The offered file, opened read-only by the caller, who keeps it open
    /// for the whole launch and closes it afterwards.
    pub(crate) file: Option<HANDLE>,
    /// This process's standard handles (null or INVALID_HANDLE_VALUE if
    /// absent), owned by the runtime and left open.
    pub(crate) stdio: [HANDLE; 3],
    pub(crate) system_root: Option<Vec<u16>>,
    pub(crate) windir: Option<Vec<u16>>,
}

/// Creates the container and the job, starts the worker in them, waits, and
/// returns its exit code. Every step that fails returns an error and
/// releases what was set up; once the worker exists, a failure terminates it
/// before it has run an instruction.
pub(crate) fn launch<A: Api>(api: &A, policy: &Policy, inputs: Inputs) -> io::Result<u32> {
    let profile = WideCString::from_str(&policy.profile)?;
    let container =
        os::app_container_sid(api, &profile).map_err(context("AppContainer profile"))?;
    let capabilities = policy
        .capabilities
        .iter()
        .map(|name| os::capability_sid(api, name).map_err(context(name)))
        .collect::<io::Result<Vec<_>>>()?;
    // The container must read and run its own image. Where the user may not
    // change the file's permissions (an installation under Program Files)
    // every AppContainer can usually read it already; if not, CreateProcessW
    // fails below and nothing runs unconfined.
    let _ = os::grant(api, &inputs.executable, &container, READ_EXECUTE, false);
    for directory in &inputs.writable {
        os::grant(api, directory, &container, MODIFY, true)
            .map_err(context(String::from_utf16_lossy(directory.units())))?;
    }
    // The container's own folder is its working directory and holds TEMP.
    let folder = os::container_folder(api, &container).map_err(context("AppContainer folder"))?;
    let temp = plan::join(&folder, "Temp");
    api.create_directory_all(&temp)?;

    let mut arguments = inputs.arguments;
    if let Some(file) = inputs.file {
        arguments.push(FILE_HANDLE_FLAG.encode_utf16().collect());
        arguments.push(plan::handle_argument(file).encode_utf16().collect());
    }
    let stdio = inputs.stdio.map(plan::standard_handle);
    let handles = plan::handle_list(inputs.file, stdio);
    for &handle in &handles {
        os::mark_inheritable(api, handle).map_err(context("inheritable handle"))?;
    }
    let mut variables = vec![("TEMP", temp.clone()), ("TMP", temp)];
    variables.extend(inputs.system_root.map(|value| ("SystemRoot", value)));
    variables.extend(inputs.windir.map(|value| ("windir", value)));
    // Without LOCALAPPDATA, CreateProcessW refuses an AppContainer process
    // (ERROR_ENVVAR_NOT_FOUND); it is three levels above the folder.
    variables.extend(plan::ancestor(&folder, 3).map(|local| ("LOCALAPPDATA", local)));
    let environment = plan::environment_block(&variables)?;
    let mut command_line = plan::command_line(inputs.executable.units(), &arguments)?;
    let directory = WideCString::new(folder)?;

    let job = Job::new(api, &plan::job_limits(policy)?, &plan::UI_RESTRICTIONS)
        .map_err(context("job object"))?;
    // Everything the attribute list points to is declared before it, so it
    // outlives the list (AttributeList borrows it).
    let security = Capabilities::new(&container, &capabilities)?;
    let child_policy = plan::CHILD_PROCESS_RESTRICTED;
    let mitigations = plan::MITIGATIONS;
    let packages = plan::ALL_APPLICATION_PACKAGES_OPT_OUT;
    let child = {
        let mut attributes = AttributeList::new(api, 5).map_err(context("attribute list"))?;
        attributes.security_capabilities(&security)?;
        if !handles.is_empty() {
            attributes.handle_list(&handles)?;
        }
        attributes.child_process_policy(&child_policy)?;
        attributes.mitigation_policy(&mitigations)?;
        if policy.less_privileged {
            attributes.all_application_packages_policy(&packages)?;
        }
        let command = Command {
            application: &inputs.executable,
            command_line: &mut command_line,
            environment: &environment,
            directory: &directory,
            stdio,
        };
        os::create_process(api, command, &mut attributes, !handles.is_empty())
            .map_err(context("CreateProcessW"))?
        // The list is deleted here, after CreateProcessW has returned.
    };
    if let Err(error) = job.assign(&child) {
        child.terminate();
        return Err(context("job assignment")(error));
    }
    if let Err(error) = child.resume() {
        child.terminate();
        return Err(context("ResumeThread")(error));
    }
    child.wait()
}

fn context<E: std::fmt::Display>(step: impl std::fmt::Display) -> impl FnOnce(E) -> io::Error {
    move |error| io::Error::other(format!("{step}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{ffi::c_void, ptr};

    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS,
        EXTENDED_STARTUPINFO_PRESENT,
    };

    use super::*;
    use crate::{
        mock::{Mock, container_sid},
        plan::tests::{sid, wide},
    };

    const EXECUTABLE: &str = r"C:\Program Files\Nuschel\nuschel-storage-worker.exe";
    const HISTORY: &str = r"C:\Users\t\AppData\Local\Nuschel\history";

    fn handle(raw: usize) -> HANDLE {
        ptr::without_provenance_mut::<c_void>(raw)
    }

    fn policy() -> Policy {
        Policy {
            profile: "nuschel.storage.0123456789abcdef".into(),
            capabilities: vec!["internetClient".into(), "microphone".into()],
            writable: vec![HISTORY.into()],
            memory: 1 << 30,
            cpu_seconds: Some(5),
            less_privileged: true,
            ..Policy::default()
        }
    }

    fn inputs() -> Inputs {
        Inputs {
            executable: WideCString::from_str(EXECUTABLE).unwrap(),
            arguments: vec![wide("--database"), wide(r"C:\a b\history.sqlite3")],
            writable: vec![WideCString::from_str(HISTORY).unwrap()],
            file: Some(handle(0x400)),
            // Standard error is the same handle as standard output.
            stdio: [handle(0x10), handle(0x14), handle(0x14)],
            system_root: Some(wide(r"C:\Windows")),
            windir: Some(wide(r"C:\Windows")),
        }
    }

    fn folder() -> String {
        let container = container_sid("nuschel.storage.0123456789abcdef");
        let mut text = format!("S-1-{}", container[7]);
        for chunk in container[8..].chunks(4) {
            text.push_str(&format!(
                "-{}",
                u32::from_le_bytes(chunk.try_into().unwrap())
            ));
        }
        format!(r"C:\Users\t\AppData\Local\Packages\{text}\AC")
    }

    #[test]
    fn launches_confined_and_releases_everything() {
        let mock = Mock::new();
        assert_eq!(launch(&mock, &policy(), inputs()).unwrap(), 7);
        let created = mock.created().unwrap();
        assert_eq!(created.application, EXECUTABLE);
        assert_eq!(
            created.command_line,
            format!(r#""{EXECUTABLE}" --database "C:\a b\history.sqlite3" --file-handle 1024"#)
        );
        let folder = folder();
        assert_eq!(created.directory, folder);
        assert_eq!(
            created.environment,
            [
                r"LOCALAPPDATA=C:\Users\t\AppData\Local".to_owned(),
                r"SystemRoot=C:\Windows".into(),
                format!(r"TEMP={folder}\Temp"),
                format!(r"TMP={folder}\Temp"),
                r"windir=C:\Windows".into(),
            ]
        );
        let flags = EXTENDED_STARTUPINFO_PRESENT
            | CREATE_SUSPENDED
            | DETACHED_PROCESS
            | CREATE_UNICODE_ENVIRONMENT;
        assert_eq!(created.flags, flags);
        assert!(created.inherit);
        assert_eq!(created.handles, [0x400, 0x10, 0x14]);
        assert_eq!(created.stdio, [0x10, 0x14, 0x14]);
        assert_eq!(
            created.container,
            container_sid("nuschel.storage.0123456789abcdef")
        );
        let internet = vec![1, 2, 0, 0, 0, 0, 0, 15, 3, 0, 0, 0, 1, 0, 0, 0];
        assert_eq!(created.capabilities, [internet, sid(1024, 10)]);
        assert_eq!(created.child_policy, Some(plan::CHILD_PROCESS_RESTRICTED));
        assert_eq!(created.mitigations, Some(plan::MITIGATIONS));
        assert_eq!(
            created.packages,
            Some(plan::ALL_APPLICATION_PACKAGES_OPT_OUT)
        );
        let limits = mock.limits().unwrap();
        assert_eq!(limits.ProcessMemoryLimit, 1 << 30);
        assert_eq!(limits.BasicLimitInformation.ActiveProcessLimit, 1);
        assert_eq!(
            mock.interface(),
            Some(plan::UI_RESTRICTIONS.UIRestrictionsClass)
        );
        assert_eq!(mock.outcome(), (true, true, false));
        assert_eq!(mock.directories(), [format!(r"{folder}\Temp")]);
        let container = container_sid("nuschel.storage.0123456789abcdef");
        assert!(plan::grants(
            &mock.dacl(HISTORY).unwrap(),
            &container,
            MODIFY,
            true
        ));
        assert!(plan::grants(
            &mock.dacl(EXECUTABLE).unwrap(),
            &container,
            READ_EXECUTE,
            false
        ));
        assert!(!plan::grants(
            &mock.dacl(EXECUTABLE).unwrap(),
            &container,
            MODIFY,
            false
        ));
    }

    #[test]
    fn a_full_app_container_without_handles_inherits_nothing() {
        let mock = Mock::new();
        let policy = Policy {
            less_privileged: false,
            capabilities: Vec::new(),
            writable: Vec::new(),
            ..policy()
        };
        let inputs = Inputs {
            file: None,
            writable: Vec::new(),
            stdio: [ptr::null_mut(); 3],
            ..inputs()
        };
        launch(&mock, &policy, inputs).unwrap();
        let created = mock.created().unwrap();
        assert_eq!(created.packages, None);
        assert!(!created.inherit);
        assert!(created.handles.is_empty());
        assert!(created.capabilities.is_empty());
    }

    #[test]
    fn existing_profiles_and_grants_are_reused() {
        let mock = Mock::new();
        launch(&mock, &policy(), inputs()).unwrap();
        let writes = mock.acl_writes();
        assert_eq!(writes, 2);
        // The second start finds the profile and both grants in place.
        launch(&mock, &policy(), inputs()).unwrap();
        assert_eq!(
            mock.calls()["derive_app_container_sid_from_app_container_name"],
            1
        );
        assert_eq!(mock.acl_writes(), writes);
        // A directory without a DACL is open to everyone already.
        let open = Mock::new();
        open.set_null_dacl(HISTORY);
        launch(&open, &policy(), inputs()).unwrap();
        assert_eq!(open.acl_writes(), 1);
        // An existing entry for someone else is kept beside the new one.
        let shared = Mock::new();
        let other =
            crate::plan::tests::acl(&[crate::plan::tests::allowed_ace(3, MODIFY, &sid(9, 9))]);
        shared.set_dacl(HISTORY, other);
        launch(&shared, &policy(), inputs()).unwrap();
        let dacl = shared.dacl(HISTORY).unwrap();
        assert!(plan::grants(&dacl, &sid(9, 9), MODIFY, true));
        assert!(plan::grants(
            &dacl,
            &container_sid("nuschel.storage.0123456789abcdef"),
            MODIFY,
            true
        ));
    }

    /// Every fallible call, failed in turn: the launch fails (except for the
    /// tolerated grant on the executable), and the mock checks on drop that
    /// everything allocated was released once and every handle closed once.
    #[test]
    fn every_failure_midway_releases_everything() {
        let calls = {
            let mock = Mock::new();
            launch(&mock, &policy(), inputs()).unwrap();
            mock.calls()
        };
        let mut cases = 0;
        for (&name, &count) in &calls {
            for index in 0..count {
                let mock = Mock::failing(name, index);
                let result = launch(&mock, &policy(), inputs());
                let tolerated = index == 0
                    && [
                        "get_named_security_info",
                        "set_entries_in_acl",
                        "set_named_security_info",
                    ]
                    .contains(&name);
                assert_eq!(result.is_ok(), tolerated, "{name} #{index}: {result:?}");
                let (assigned, resumed, terminated) = mock.outcome();
                if ["assign_process_to_job_object", "resume_thread"].contains(&name) {
                    assert!(
                        terminated && !resumed,
                        "{name}: the suspended worker must end"
                    );
                }
                if name == "create_process" {
                    assert!(!assigned && !terminated);
                }
                drop(mock);
                cases += 1;
            }
        }
        assert!(cases >= 25, "only {cases} failure points");
    }

    #[test]
    fn inherited_handles_must_be_files_and_not_standard_handles() {
        let mock = Mock::new();
        let file = handle(0x400);
        mock.add_file(file);
        let standard = [handle(0x10), handle(0x14), handle(0x18)];
        assert_eq!(os::inherited_handle(&mock, "1024", standard).unwrap(), file);
        assert!(os::inherited_handle(&mock, "16", standard).is_err());
        assert!(os::inherited_handle(&mock, "2048", standard).is_err());
        assert!(os::inherited_handle(&mock, "0", standard).is_err());
        assert!(os::inherited_handle(&mock, "x", standard).is_err());
    }
}
