// SPDX-License-Identifier: EUPL-1.2
//! The launch stage's steps, over any [`Api`]: the real one on Windows, the
//! mock in tests (so that every failure midway is exercised under Miri).
#![forbid(unsafe_code)]

use std::io;

use nuschel_ipc::windows_sandbox::{FILE_HANDLE_FLAG, Policy};
use windows_sys::Win32::Foundation::HANDLE;

use crate::{
    api::Api,
    os::{self, AttributeList, Capabilities, Command, Job},
    plan::{self, Grant, WideCString},
};

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
    /// absent), owned by the runtime and left open. Console handles are not
    /// passed on.
    pub(crate) stdio: [HANDLE; 3],
    pub(crate) system_root: Option<Vec<u16>>,
    pub(crate) windir: Option<Vec<u16>>,
}

/// Creates the container and the job, starts the worker in them, waits, and
/// returns its exit code. Every step that fails returns an error and
/// releases what was set up. The worker is created suspended inside its job
/// (never outside it); a failure before it is resumed terminates it before
/// it has run an instruction.
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
    let _ = os::grant(api, &inputs.executable, &container, Grant::Executable);
    for directory in &inputs.writable {
        os::grant(api, directory, &container, Grant::Directory)
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
    // A console handle would let the worker type into the user's console;
    // the supervisor passes pipes and NUL, so only a hand-started launch
    // stage has any.
    let stdio = inputs.stdio.map(|handle| {
        let handle = plan::standard_handle(handle);
        if !handle.is_null() && api.is_console(handle) {
            std::ptr::null_mut()
        } else {
            handle
        }
    });
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
    let jobs = [job.raw()];
    let child_policy = plan::CHILD_PROCESS_RESTRICTED;
    let mitigations = plan::MITIGATIONS;
    let packages = plan::ALL_APPLICATION_PACKAGES_OPT_OUT;
    let child = {
        let mut attributes = AttributeList::new(api, 6).map_err(context("attribute list"))?;
        attributes.security_capabilities(&security)?;
        attributes.job_list(&jobs)?;
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

    use windows_sys::Win32::{
        Security::{CONTAINER_INHERIT_ACE, OBJECT_INHERIT_ACE},
        System::Threading::{
            CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, DETACHED_PROCESS,
            EXTENDED_STARTUPINFO_PRESENT,
        },
    };

    use super::*;
    use crate::{
        mock::{Mock, container_sid},
        plan::{
            MODIFY, READ_EXECUTE, State, grant_state,
            tests::{acl, allowed_ace, sid, wide},
        },
    };

    const EXECUTABLE: &str = r"C:\Program Files\Nuschel\nuschel-storage-worker.exe";
    const HISTORY: &str = r"C:\Users\t\AppData\Local\Nuschel\history";
    const PROFILE: &str = "nuschel.storage.0123456789abcdef";

    fn handle(raw: usize) -> HANDLE {
        ptr::without_provenance_mut::<c_void>(raw)
    }

    fn policy() -> Policy {
        Policy {
            profile: PROFILE.into(),
            capabilities: vec!["internetClient".into(), "microphone".into()],
            writable: vec![HISTORY.into()],
            memory: 1 << 30,
            cpu_seconds: Some(5),
            less_privileged: true,
            ..Policy::default()
        }
    }

    /// A mock that knows the caller's handles used by [`inputs`].
    fn new_mock() -> Mock {
        with_handles(Mock::new())
    }

    fn with_handles(mock: Mock) -> Mock {
        mock.add_file(handle(0x400));
        mock.add_handle(handle(0x10), false);
        mock.add_handle(handle(0x14), false);
        mock
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
        let container = container_sid(PROFILE);
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
        let mock = new_mock();
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
        assert_eq!(created.jobs, 1, "created inside its job");
        assert_eq!(created.container, container_sid(PROFILE));
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
        let container = container_sid(PROFILE);
        let history = mock.dacl(HISTORY).unwrap();
        assert_eq!(
            grant_state(&history, &container, Grant::Directory),
            State::Granted
        );
        let executable = mock.dacl(EXECUTABLE).unwrap();
        assert_eq!(
            grant_state(&executable, &container, Grant::Executable),
            State::Granted
        );
    }

    #[test]
    fn a_full_app_container_without_handles_inherits_nothing() {
        let mock = new_mock();
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
    fn console_handles_are_not_passed_on() {
        let mock = new_mock();
        mock.add_handle(handle(0x20), true);
        let inputs = Inputs {
            stdio: [handle(0x20), handle(0x14), handle(0x20)],
            ..inputs()
        };
        launch(&mock, &policy(), inputs).unwrap();
        let created = mock.created().unwrap();
        assert_eq!(created.stdio, [0, 0x14, 0]);
        assert_eq!(created.handles, [0x400, 0x14]);
    }

    #[test]
    fn existing_profiles_and_grants_are_reused() {
        let mock = new_mock();
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
        let open = new_mock();
        open.set_null_dacl(HISTORY);
        launch(&open, &policy(), inputs()).unwrap();
        assert_eq!(open.acl_writes(), 1);
        // An existing entry for someone else is kept beside the new ones.
        let shared = new_mock();
        shared.set_dacl(HISTORY, acl(&[allowed_ace(3, MODIFY, &sid(9, 9))]));
        launch(&shared, &policy(), inputs()).unwrap();
        let dacl = shared.dacl(HISTORY).unwrap();
        assert_eq!(
            grant_state(&dacl, &sid(9, 9), Grant::Directory),
            State::Excessive
        );
        assert_eq!(
            grant_state(&dacl, &container_sid(PROFILE), Grant::Directory),
            State::Granted
        );
    }

    #[test]
    fn an_older_broader_grant_is_narrowed() {
        let mock = new_mock();
        let container = container_sid(PROFILE);
        let inherit = u8::try_from(OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE).unwrap();
        // What an earlier version wrote: Modify on the directory itself too.
        mock.set_dacl(HISTORY, acl(&[allowed_ace(inherit, MODIFY, &container)]));
        mock.set_dacl(EXECUTABLE, acl(&[allowed_ace(0, MODIFY, &container)]));
        launch(&mock, &policy(), inputs()).unwrap();
        let history = mock.dacl(HISTORY).unwrap();
        assert_eq!(
            grant_state(&history, &container, Grant::Directory),
            State::Granted
        );
        let executable = mock.dacl(EXECUTABLE).unwrap();
        assert_eq!(
            grant_state(&executable, &container, Grant::Executable),
            State::Granted
        );
        assert_eq!(
            grant_state(&executable, &container, Grant::Executable),
            State::Granted
        );
        assert_eq!(
            executable.len(),
            acl(&[allowed_ace(0, READ_EXECUTE, &container)]).len()
        );
    }

    #[test]
    fn junctions_are_refused_not_followed() {
        let mock = new_mock();
        mock.set_reparse_point(HISTORY);
        let error = launch(&mock, &policy(), inputs()).unwrap_err();
        assert!(error.to_string().contains("junction"), "{error}");
        assert_eq!(mock.dacl(HISTORY), None, "nothing written through it");
        assert!(mock.created().is_none());
        // A junction in place of the executable only skips its grant.
        let executable = new_mock();
        executable.set_reparse_point(EXECUTABLE);
        launch(&executable, &policy(), inputs()).unwrap();
        assert_eq!(executable.dacl(EXECUTABLE), None);
    }

    /// Every fallible call, failed in turn, with and without an existing
    /// profile: the launch fails (except for the tolerated grant on the
    /// executable), and the mock checks on drop that everything allocated
    /// was released once and every handle closed once.
    #[test]
    fn every_failure_midway_releases_everything() {
        let mut cases = 0;
        for existing in [false, true] {
            let fresh = || {
                let mock = new_mock();
                if existing {
                    // A first launch leaves the profile and the grants behind.
                    launch(&mock, &policy(), inputs()).unwrap();
                }
                mock
            };
            let calls = {
                let mock = fresh();
                let before = mock.calls();
                launch(&mock, &policy(), inputs()).unwrap();
                let mut calls = mock.calls();
                for (name, count) in before {
                    *calls.get_mut(name).unwrap() -= count;
                }
                calls
            };
            // The calls the executable's grant makes in this scenario.
            let executable = {
                let mock = fresh();
                let before = mock.calls();
                let path = WideCString::from_str(EXECUTABLE).unwrap();
                let container = os::Sid::from_bytes(&container_sid(PROFILE)).unwrap();
                let _ = os::grant(&mock, &path, &container, Grant::Executable);
                let mut calls = mock.calls();
                for (name, count) in before {
                    *calls.get_mut(name).unwrap() -= count;
                }
                calls
            };
            for (&name, &count) in &calls {
                for index in 0..count {
                    let mock = fresh();
                    let offset = mock.calls().get(name).copied().unwrap_or(0);
                    mock.fail_at(name, offset + index);
                    let result = launch(&mock, &policy(), inputs());
                    // The executable's grant comes first among the file
                    // and ACL calls, and may fail without stopping the launch.
                    let tolerated = index < executable.get(name).copied().unwrap_or(0);
                    assert_eq!(result.is_ok(), tolerated, "{name} #{index}: {result:?}");
                    let (assigned, resumed, terminated) = mock.outcome();
                    if name == "resume_thread" {
                        assert!(assigned && terminated && !resumed, "the worker must end");
                    }
                    drop(mock);
                    cases += 1;
                }
            }
        }
        println!("{cases} failure points");
        assert!(cases >= 40, "only {cases} failure points");
    }

    #[test]
    fn inherited_handles_must_be_files_and_not_standard_handles() {
        let mock = new_mock();
        let file = handle(0x400);
        let standard = [handle(0x10), handle(0x14), handle(0x18)];
        assert_eq!(os::inherited_handle(&mock, "1024", standard).unwrap(), file);
        assert!(os::inherited_handle(&mock, "16", standard).is_err());
        assert!(
            os::inherited_handle(&mock, "20", [ptr::null_mut(); 3]).is_err(),
            "a pipe"
        );
        assert!(os::inherited_handle(&mock, "2048", standard).is_err());
        assert!(os::inherited_handle(&mock, "0", standard).is_err());
        assert!(os::inherited_handle(&mock, "x", standard).is_err());
        // The file is taken over through a duplicate; the original stays open.
        let duplicate = os::duplicate(&mock, file).unwrap();
        assert_ne!(duplicate.raw(), file);
        drop(duplicate);
    }
}
