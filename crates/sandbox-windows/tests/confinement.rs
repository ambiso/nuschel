// SPDX-License-Identifier: EUPL-1.2
//! Windows only: every worker's sandbox, checked from the inside.
//!
//! `confinement.exe [--only KIND] [--nested] [--connect HOST:PORT] [--workers DIR [--rendezvous ws://HOST:PORT/v1
//! --relay tcp://HOST:PORT [--code-file PATH] [--receive CODE --expect TEXT]]]`
//!
//! This binary starts itself through `nuschl_sandbox`'s real sandbox
//! commands, once per worker kind, as if it were that worker. Inside, as a
//! probe, it tries what a compromised worker would: read a file in the user's
//! Documents, list the user's profile, write outside its directory, connect
//! to `--connect` (default 10.0.2.100:80, the VM harness), start a process,
//! open another process to query or end it, commit more memory than its job
//! allows, read the user's environment and autostart keys in the registry or
//! write a key there, and read the clipboard (the test puts text there first). With `--workers`, the real workers then do their jobs in the
//! same sandboxes: content decodes an image, math renders a formula, storage writes and reads
//! history, crypto creates an identity, media opens the default devices and
//! transfer offers a file (reaching `--rendezvous` if one is given; with
//! `--code-file` it writes the code there and waits until someone has
//! received the file) and, with `--receive`, downloads one through the
//! rendezvous and relay.
//!
//! Prints one line per check and exits non-zero if any expectation fails.
#![allow(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    println!("the confinement check runs on Windows only (tests/README.md)");
}

#[cfg(windows)]
fn main() {
    nuschl_sandbox_windows::launch_if_requested();
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let probe = ["--probe", "--database", "--data", "--live", "--file-handle"];
    // The math worker takes no arguments: a run without any is a probe only
    // inside an AppContainer.
    if arguments
        .first()
        .is_some_and(|first| probe.contains(&first.as_str()))
        || (arguments.is_empty() && windows::is_app_container())
    {
        windows::probe();
        return;
    }
    std::process::exit(windows::drive(&arguments));
}

#[cfg(windows)]
mod windows {
    use std::{
        io::{BufRead as _, Write as _},
        path::{Path, PathBuf},
        time::Duration,
    };

    use nuschl_ipc::{
        ContentRequest, ContentResponse, Envelope, LiveMediaEvent, LiveMediaRequest,
        StorageRequest, StorageResponse, TransferEvent, TransferRequest, WormholeServers,
        crypto::{CryptoRequest, CryptoResponse, RecoveryPolicy},
        host,
        math::{MathRequest, MathResponse},
        write_frame,
    };
    use nuschl_sandbox::{Containment, Role, command, contain};
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
        process::{Child, Command},
        time::timeout,
    };

    /// A 4x4 red PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAYAAACp8Z5+AAAAEklEQVR4nGP4z8DwHxkzkC4AADxAH+HggXe0AAAAAElFTkSuQmCC";

    // ---------------------------------------------------------------- probe

    /// Inside a sandbox: one JSON request on stdin, one JSON report on stdout.
    pub fn probe() {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).ok();
        let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let text = |key: &str| request[key].as_str().unwrap_or_default().to_owned();
        let outcome = |result: std::io::Result<()>| match result {
            Ok(()) => "allowed".to_owned(),
            Err(error) => format!("denied ({error})"),
        };
        let mut report = json!({ "app_container": is_app_container(), "job": job() });
        report["read_documents"] = outcome(std::fs::read(text("secret")).map(drop)).into();
        report["list_profile"] = outcome(std::fs::read_dir(text("profile")).map(drop)).into();
        report["write_profile"] = outcome(std::fs::write(text("outside"), b"escape")).into();
        report["write_public"] = outcome(std::fs::write(text("public"), b"escape")).into();
        report["write_temp"] = outcome(std::fs::write(text("temp"), b"escape")).into();
        if let Some(inside) = request["inside"].as_str() {
            let file = Path::new(inside).join("probe.txt");
            report["write_own_directory"] = outcome(std::fs::write(&file, b"ok")).into();
        }
        let address = text("connect");
        // In a less privileged AppContainer Winsock itself fails to start,
        // and std panics; that is a denial too.
        let connect = std::panic::catch_unwind(|| {
            address
                .parse()
                .map_err(std::io::Error::other)
                .and_then(|address| {
                    std::net::TcpStream::connect_timeout(&address, Duration::from_secs(4))
                })
        })
        .unwrap_or_else(|_| Err(std::io::Error::other("Winsock unavailable")));
        report["connect"] = outcome(connect.map(drop)).into();
        let spawned = std::process::Command::new(r"C:\Windows\System32\cmd.exe")
            .args(["/c", "exit 0"])
            .status();
        report["start_process"] = outcome(spawned.map(drop)).into();
        report["enumerate_processes"] = enumerate_processes().into();
        let driver = request["driver"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .unwrap_or(0);
        // PROCESS_QUERY_LIMITED_INFORMATION and PROCESS_TERMINATE.
        report["query_other_process"] = outcome(open_process(driver, 0x1000)).into();
        report["terminate_other_process"] = outcome(open_process(driver, 0x0001)).into();
        let mut memory: Vec<u8> = Vec::new();
        let bytes = request["allocate"]
            .as_u64()
            .and_then(|bytes| usize::try_from(bytes).ok())
            .unwrap_or(0);
        report["commit_over_limit"] = outcome(
            memory
                .try_reserve_exact(bytes)
                .map_err(std::io::Error::other),
        )
        .into();
        drop(memory);
        report["read_user_environment"] = outcome(read_key(r"Environment")).into();
        report["read_autostart"] =
            outcome(read_key(r"Software\Microsoft\Windows\CurrentVersion\Run")).into();
        report["write_registry"] = outcome(create_key(r"Software\NuschlSandboxProbe")).into();
        report["read_clipboard"] = outcome(clipboard_text()).into();
        // The transfer probe was handed a file by handle; it must read it.
        let arguments: Vec<String> = std::env::args().collect();
        if let Some(position) = arguments.iter().position(|a| a == "--file-handle") {
            let read = nuschl_sandbox_windows::inherited_file(arguments[position + 1].as_ref())
                .and_then(|mut file| {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut file, &mut text).map(|_| text)
                });
            report["read_offered_file"] = match read {
                Ok(text) if text == "offered" => "allowed".into(),
                Ok(text) => format!("wrong content {text:?}").into(),
                Err(error) => format!("denied ({error})").into(),
            };
        }
        let mut output = std::io::stdout().lock();
        writeln!(output, "{report}").ok();
        output.flush().ok();
    }

    /// Opens `HKEY_CURRENT_USER\\<path>` for reading.
    fn read_key(path: &str) -> std::io::Result<()> {
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW,
        };
        let path: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name and an out-pointer to a local.
        let error =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &raw mut key) };
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error.cast_signed()));
        }
        // SAFETY: the key opened above, closed once.
        unsafe { RegCloseKey(key) };
        Ok(())
    }

    /// Creates (and leaves) `HKEY_CURRENT_USER\\<path>`.
    fn create_key(path: &str) -> std::io::Result<()> {
        use windows_sys::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, RegCloseKey,
            RegCreateKeyExW,
        };
        let path: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name, no class or security attributes, and
        // out-pointers to locals.
        let error = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &raw mut key,
                std::ptr::null_mut(),
            )
        };
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error.cast_signed()));
        }
        // SAFETY: the key created above, closed once.
        unsafe { RegCloseKey(key) };
        Ok(())
    }

    /// Whether this process can get text from the clipboard.
    fn clipboard_text() -> std::io::Result<()> {
        use windows_sys::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, OpenClipboard,
        };
        const CF_UNICODETEXT: u32 = 13;
        // SAFETY: no window; fails if another process holds the clipboard.
        if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the clipboard is open; the handle is only compared to null.
        let data = unsafe { GetClipboardData(CF_UNICODETEXT) };
        let error = std::io::Error::last_os_error();
        // SAFETY: opened above.
        unsafe { CloseClipboard() };
        if data.is_null() { Err(error) } else { Ok(()) }
    }

    /// The limits of the job this process is in (the innermost one, when
    /// jobs nest), as the process itself sees them.
    fn job() -> Value {
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation,
            QueryInformationJobObject,
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let mut interface = JOBOBJECT_BASIC_UI_RESTRICTIONS::default();
        let limits_size = u32::try_from(size_of_val(&limits)).unwrap_or(0);
        let interface_size = u32::try_from(size_of_val(&interface)).unwrap_or(0);
        // SAFETY: a null job handle means the caller's own job; `limits` is a
        // local of the class's structure and size; no length is returned.
        let limits_read = unsafe {
            QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                (&raw mut limits).cast(),
                limits_size,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: as above, for the UI restrictions.
        let interface_read = unsafe {
            QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectBasicUIRestrictions,
                (&raw mut interface).cast(),
                interface_size,
                std::ptr::null_mut(),
            )
        };
        json!({
            "read": limits_read != 0 && interface_read != 0,
            "active_processes": limits.BasicLimitInformation.ActiveProcessLimit,
            "memory": limits.ProcessMemoryLimit,
            // User-mode CPU seconds, 0 without a limit.
            "cpu": if limits.BasicLimitInformation.LimitFlags & 2 == 0 {
                0
            } else {
                limits.BasicLimitInformation.PerProcessUserTimeLimit / 10_000_000
            },
            "interface": interface.UIRestrictionsClass,
        })
    }

    pub fn is_app_container() -> bool {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            Security::{GetTokenInformation, TOKEN_QUERY, TokenIsAppContainer},
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: no arguments; returns a pseudo handle that needs no closing.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: the current-process pseudo handle and an out-pointer to a local.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
            return false;
        }
        let (mut value, mut length) = (0_u32, 0_u32);
        // SAFETY: TokenIsAppContainer fills one DWORD; out-pointers to locals.
        let queried = unsafe {
            GetTokenInformation(
                token,
                TokenIsAppContainer,
                (&raw mut value).cast(),
                4,
                &raw mut length,
            )
        };
        // SAFETY: the token handle opened above, closed once.
        unsafe { CloseHandle(token) };
        queried != 0 && value != 0
    }

    fn enumerate_processes() -> String {
        use windows_sys::Win32::System::ProcessStatus::K32EnumProcesses;
        let mut ids = vec![0_u32; 4096];
        let mut needed = 0;
        let bytes = u32::try_from(size_of_val(ids.as_slice())).unwrap_or(0);
        // SAFETY: `ids` has room for the byte count passed; `needed` is a
        // local for one write.
        let listed = unsafe { K32EnumProcesses(ids.as_mut_ptr(), bytes, &raw mut needed) };
        if listed == 0 {
            format!("denied ({})", std::io::Error::last_os_error())
        } else {
            format!("{} process ids", needed / 4)
        }
    }

    fn open_process(pid: u32, access: u32) -> std::io::Result<()> {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::OpenProcess};
        // SAFETY: no pointers; a failure returns null.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the handle just opened, closed once.
        unsafe { CloseHandle(handle) };
        Ok(())
    }

    // --------------------------------------------------------------- driver

    struct Checks {
        failures: u32,
    }

    impl Checks {
        fn expect(&mut self, kind: &str, check: &str, expected: &str, actual: &str) {
            let pass = actual.starts_with(expected);
            if !pass {
                self.failures += 1;
            }
            let mark = if pass { "ok  " } else { "FAIL" };
            println!("{mark} {kind:<9} {check:<24} want {expected:<8} got {actual}");
        }

        fn report(&mut self, kind: &str, check: &str, result: Result<String, String>) {
            match result {
                Ok(detail) => println!("ok   {kind:<9} {check:<24} {detail}"),
                Err(detail) => {
                    self.failures += 1;
                    println!("FAIL {kind:<9} {check:<24} {detail}");
                }
            }
        }
    }

    fn option(arguments: &[String], name: &str) -> Option<String> {
        let position = arguments.iter().position(|a| a == name)?;
        arguments.get(position + 1).cloned()
    }

    /// Puts this process into a new job (no limits); its children start in it.
    fn enter_job() -> Result<String, String> {
        use windows_sys::Win32::System::{
            JobObjects::{AssignProcessToJobObject, CreateJobObjectW},
            Threading::GetCurrentProcess,
        };
        // SAFETY: default security and no name (both may be null).
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // SAFETY: no arguments; a pseudo handle that needs no closing.
        let process = unsafe { GetCurrentProcess() };
        // SAFETY: handles only. The job handle stays open (and the process in
        // the job) until this test exits.
        if unsafe { AssignProcessToJobObject(job, process) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok("every launch stage and worker starts in it".into())
    }

    pub fn drive(arguments: &[String]) -> i32 {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut checks = Checks { failures: 0 };
        // With --nested every launch stage starts inside a job, so each
        // worker starts in it too and its own job (with UI limits) must nest.
        if arguments.iter().any(|argument| argument == "--nested") {
            checks.report("driver", "runs in a job", enter_job());
        }
        // Only a probe that cannot read text the test itself can read means anything.
        if let Err(error) = clipboard_text() {
            println!("     the clipboard holds no text for this test ({error}); set some first");
        }
        runtime.block_on(run(arguments, &mut checks));
        println!("{} failed check(s)", checks.failures);
        i32::from(checks.failures > 0)
    }

    async fn run(arguments: &[String], checks: &mut Checks) {
        let me = std::env::current_exe().expect("own path");
        let profile = PathBuf::from(std::env::var_os("USERPROFILE").expect("USERPROFILE"));
        let documents = profile.join("Documents");
        std::fs::create_dir_all(&documents).ok();
        let secret = documents.join("nuschl-sandbox-secret.txt");
        std::fs::write(&secret, "secret").expect("secret file");
        let work = std::env::temp_dir().join(format!("nuschl-confinement-{}", std::process::id()));
        let storage = work.join("history");
        let crypto = work.join("crypto");
        for directory in [&storage, &crypto] {
            std::fs::create_dir_all(directory).expect("worker directory");
        }
        let offered = work.join("offered.txt");
        std::fs::write(&offered, "offered").expect("offered file");
        let connect = option(arguments, "--connect").unwrap_or_else(|| "10.0.2.100:80".into());
        let request = |inside: Option<&Path>, allocate: u64| {
            json!({
                "secret": secret,
                "profile": profile,
                "outside": profile.join("nuschl-sandbox-escape.txt"),
                "public": r"C:\Users\Public\nuschl-sandbox-escape.txt",
                "temp": std::env::temp_dir().join("nuschl-sandbox-escape.txt"),
                "inside": inside,
                "connect": connect,
                "driver": std::process::id(),
                "allocate": allocate,
            })
        };
        let mib = 1 << 20;
        let probes = [
            (
                "content",
                command(&me, Role::Content { cpu_seconds: 5 }, &["--probe".into()]),
                request(None, 640 * mib),
            ),
            (
                "math",
                command(&me, Role::Math, &[]),
                request(None, 640 * mib),
            ),
            (
                "storage",
                command(
                    &me,
                    Role::Storage {
                        directory: &storage,
                    },
                    &[],
                ),
                request(Some(&storage), 1200 * mib),
            ),
            (
                "crypto",
                command(&me, Role::Crypto { directory: &crypto }, &[]),
                request(Some(&crypto), 1200 * mib),
            ),
            (
                "media",
                command(&me, Role::Media, &[]),
                request(None, 1200 * mib),
            ),
            (
                "transfer",
                command(
                    &me,
                    Role::Transfer {
                        file: Some(&offered),
                    },
                    &[],
                ),
                request(None, 1200 * mib),
            ),
        ];
        let only = option(arguments, "--only");
        for (kind, command, request) in probes {
            if only.as_deref().is_some_and(|only| only != kind) {
                continue;
            }
            let report = match command {
                Ok(command) => run_probe(command, &request).await,
                Err(error) => Err(error.to_string()),
            };
            let report = match report {
                Ok(report) => report,
                Err(error) => {
                    checks.report(kind, "probe ran", Err(error));
                    continue;
                }
            };
            let field = |name: &str| report[name].as_str().unwrap_or("missing").to_owned();
            let in_container = report["app_container"].as_bool() == Some(true);
            checks.report(
                kind,
                "in an AppContainer",
                if in_container {
                    Ok("yes".into())
                } else {
                    Err("no".into())
                },
            );
            // Its own job, as seen from inside: one process, the memory
            // limit, all eight UI restrictions.
            let job = &report["job"];
            let memory: u64 = if kind == "content" || kind == "math" {
                512 << 20
            } else {
                1 << 30
            };
            // Content gets its request's budget (5 s here), math 120 s for
            // its whole life, the others no CPU limit.
            let cpu: i64 = match kind {
                "content" => 5,
                "math" => 120,
                _ => 0,
            };
            let effective = job["read"] == true
                && job["active_processes"] == 1
                && job["memory"] == memory
                && job["cpu"] == cpu
                && job["interface"] == 0xff;
            checks.report(
                kind,
                "in its job",
                if effective {
                    Ok(job.to_string())
                } else {
                    Err(job.to_string())
                },
            );
            for check in [
                "read_documents",
                "list_profile",
                "write_profile",
                "write_public",
                "write_temp",
                "start_process",
                "query_other_process",
                "terminate_other_process",
                "commit_over_limit",
                "read_autostart",
                "write_registry",
                "read_clipboard",
            ] {
                checks.expect(kind, check, "denied", &field(check));
            }
            // Every full AppContainer may read HKCU\Environment; the less
            // privileged ones may not.
            if kind == "media" || kind == "transfer" {
                let environment = field("read_user_environment");
                println!(
                    "     {kind:<9} {:<24} {environment} (full AppContainer)",
                    "read_user_environment"
                );
            } else {
                checks.expect(
                    kind,
                    "read_user_environment",
                    "denied",
                    &field("read_user_environment"),
                );
            }
            let network = if kind == "transfer" {
                "allowed"
            } else {
                "denied"
            };
            checks.expect(kind, "connect", network, &field("connect"));
            if kind == "storage" || kind == "crypto" {
                checks.expect(
                    kind,
                    "write_own_directory",
                    "allowed",
                    &field("write_own_directory"),
                );
            }
            if kind == "transfer" {
                checks.expect(
                    kind,
                    "read_offered_file",
                    "allowed",
                    &field("read_offered_file"),
                );
            }
            println!(
                "     {kind:<9} {:<24} {}",
                "enumerate_processes",
                field("enumerate_processes")
            );
        }
        // A policy the launch stage cannot apply never runs the worker.
        let refused = Command::new(&me)
            .args([
                "--nuschl-sandbox",
                "--profile",
                "not a valid name",
                "--",
                "--probe",
            ])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        checks.report(
            "launcher",
            "refuses a bad policy",
            match refused.map(|status| status.code()) {
                Ok(Some(nuschl_sandbox_windows::LAUNCH_FAILED)) => Ok("exit 125".into()),
                other => Err(format!("{other:?}")),
            },
        );
        if let Some(directory) = option(arguments, "--workers") {
            workers(Path::new(&directory), &work, &offered, arguments, checks).await;
        }
        std::fs::remove_file(&secret).ok();
    }

    async fn run_probe(mut command: Command, request: &Value) -> Result<Value, String> {
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("spawn: {error}"))?;
        let _containment = contain(&child).map_err(|error| error.to_string())?;
        let mut input = child.stdin.take().ok_or("no stdin")?;
        input
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        let mut output = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        let mut line = String::new();
        let read = timeout(Duration::from_secs(30), output.read_line(&mut line)).await;
        let status = timeout(Duration::from_secs(5), child.wait()).await;
        match read {
            Ok(Ok(count)) if count > 0 => {
                serde_json::from_str(&line).map_err(|error| format!("{error}: {line}"))
            }
            _ => Err(format!("no report; launch stage exited {status:?}")),
        }
    }

    // ------------------------------------------------------ the real workers

    fn spawn(mut command: Command) -> Result<(Child, Containment), String> {
        let child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("spawn: {error}"))?;
        let containment = contain(&child).map_err(|error| error.to_string())?;
        Ok((child, containment))
    }

    async fn send<T: serde::Serialize>(
        child: &mut Child,
        id: u64,
        payload: T,
    ) -> Result<(), String> {
        let mut frame = Vec::new();
        write_frame(&mut frame, &Envelope::new(id, payload)).map_err(|e| e.to_string())?;
        let input = child.stdin.as_mut().ok_or("no stdin")?;
        input.write_all(&frame).await.map_err(|e| e.to_string())
    }

    async fn receive<T: serde::de::DeserializeOwned>(child: &mut Child) -> Result<T, String> {
        receive_within(child, 20).await
    }

    async fn receive_within<T: serde::de::DeserializeOwned>(
        child: &mut Child,
        seconds: u64,
    ) -> Result<T, String> {
        let output = child.stdout.as_mut().ok_or("no stdout")?;
        let read = async {
            let size = usize::try_from(output.read_u32().await?).map_err(std::io::Error::other)?;
            let mut bytes = vec![0; size];
            output.read_exact(&mut bytes).await?;
            std::io::Result::Ok(bytes)
        };
        let bytes = timeout(Duration::from_secs(seconds), read)
            .await
            .map_err(|_| format!("no answer within {seconds} s"))?
            .map_err(|error| format!("worker closed its output: {error}"))?;
        let envelope: Envelope<T> = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        Ok(envelope.payload)
    }

    async fn workers(
        directory: &Path,
        work: &Path,
        offered: &Path,
        arguments: &[String],
        checks: &mut Checks,
    ) {
        let worker = |name: &str| directory.join(format!("nuschl-{name}-worker.exe"));
        let rendezvous = option(arguments, "--rendezvous");
        let servers = WormholeServers {
            rendezvous: rendezvous
                .clone()
                .unwrap_or_else(|| "ws://10.0.2.100:80/v1".into()),
            relay: option(arguments, "--relay").unwrap_or_else(|| "tcp://10.0.2.100:4001".into()),
            direct: false,
        };

        let only = option(arguments, "--only");
        let wanted = |kind: &str| only.as_deref().is_none_or(|only| only == kind);

        if wanted("math") {
            let rendered = async {
                let command =
                    command(&worker("math"), Role::Math, &[]).map_err(|e| e.to_string())?;
                let (mut child, _containment) = spawn(command)?;
                let request = MathRequest {
                    tex: r"\frac{a}{b} + \sqrt{x^2}".into(),
                    display: false,
                    size: 16.0,
                };
                send(&mut child, 1, request).await?;
                match receive::<MathResponse>(&mut child).await? {
                    MathResponse::Rendered(image) => {
                        Ok(format!("{}×{} pixels", image.width, image.height))
                    }
                    other => Err(format!("{other:?}")),
                }
            };
            checks.report("math", "renders a formula", rendered.await);
        }
        if only.is_some() {
            return;
        }

        let image = format!("data:image/png;base64,{PNG}");
        let decoded =
            host::exchange(&worker("content"), &[ContentRequest::View { uri: image }]).await;
        checks.report(
            "content",
            "decodes an image",
            match decoded.map(|mut results| results.pop()) {
                Ok(Some(Ok(ContentResponse::Rendered(document)))) => {
                    Ok(format!("{} block(s)", document.blocks.len()))
                }
                other => Err(format!("{other:?}")),
            },
        );

        let history = work.join("history");
        let stored = async {
            let role = Role::Storage {
                directory: &history,
            };
            let command = command(&worker("storage"), role, &[]).map_err(|e| e.to_string())?;
            let (mut child, _containment) = spawn(command)?;
            let message = nuschl_core::ChatMessage {
                server: "vm".into(),
                channel: "Root".into(),
                author: "tester".into(),
                observed_at_ms: 1,
                body: "kept in a sandbox".into(),
            };
            send(&mut child, 1, StorageRequest::Append { message }).await?;
            let appended: StorageResponse = receive(&mut child).await?;
            send(
                &mut child,
                2,
                StorageRequest::HistoryPage {
                    server: "vm".into(),
                    channel: "Root".into(),
                    before: None,
                    limit: 10,
                },
            )
            .await?;
            let page: StorageResponse = receive(&mut child).await?;
            match (appended, page) {
                (StorageResponse::Appended(_), StorageResponse::HistoryPage(page))
                    if page
                        .messages
                        .iter()
                        .any(|m| m.message.body == "kept in a sandbox") =>
                {
                    Ok(format!(
                        "{} message(s); database {} bytes",
                        page.messages.len(),
                        std::fs::metadata(history.join("history.sqlite3"))
                            .map(|m| m.len())
                            .unwrap_or(0)
                    ))
                }
                other => Err(format!("{other:?}")),
            }
        };
        checks.report("storage", "writes history", stored.await);

        let keys = work.join("crypto");
        let identity = async {
            let role = Role::Crypto { directory: &keys };
            let command = command(&worker("crypto"), role, &[]).map_err(|e| e.to_string())?;
            let (mut child, _containment) = spawn(command)?;
            send(
                &mut child,
                1,
                CryptoRequest::CreateIdentity {
                    device_name: "vm".into(),
                    recovery: RecoveryPolicy::Never,
                },
            )
            .await?;
            match receive::<CryptoResponse>(&mut child).await? {
                CryptoResponse::Identity(view) => Ok(format!(
                    "{:?}; {} file(s) in its directory",
                    view,
                    std::fs::read_dir(&keys).map(|d| d.count()).unwrap_or(0)
                )
                .chars()
                .take(160)
                .collect()),
                other => Err(format!("{other:?}")),
            }
        };
        checks.report("crypto", "creates an identity", identity.await);

        let media = async {
            let command = command(&worker("media"), Role::Media, &[]).map_err(|e| e.to_string())?;
            let (mut child, _containment) = spawn(command)?;
            send(
                &mut child,
                0,
                LiveMediaRequest::SetState {
                    epoch: 1,
                    muted: false,
                    deafened: false,
                },
            )
            .await?;
            let mut devices = String::new();
            loop {
                match receive::<LiveMediaEvent>(&mut child).await? {
                    LiveMediaEvent::Devices {
                        inputs,
                        outputs,
                        input,
                        output,
                        ..
                    } => {
                        devices = format!(
                            "{} input(s), {} output(s), using {input:?} / {output:?}",
                            inputs.len(),
                            outputs.len()
                        )
                    }
                    LiveMediaEvent::Status {
                        ready: true,
                        message,
                    } => {
                        break Ok(format!("{message}; {devices}"));
                    }
                    LiveMediaEvent::Status {
                        ready: false,
                        message,
                    } => break Err(message),
                    _ => {}
                }
            }
        };
        checks.report("media", "opens the default devices", media.await);

        let transfer = async {
            let role = Role::Transfer {
                file: Some(offered),
            };
            let command = command(&worker("transfer"), role, &[]).map_err(|e| e.to_string())?;
            let (mut child, _containment) = spawn(command)?;
            send(
                &mut child,
                1,
                TransferRequest::Offer {
                    name: "offered.txt".into(),
                    servers: servers.clone(),
                },
            )
            .await?;
            match receive::<TransferEvent>(&mut child).await? {
                TransferEvent::Code { code } => match option(arguments, "--code-file") {
                    None => Ok(format!("code {code} from the rendezvous")),
                    // Someone (the test host) picks the file up with this code.
                    Some(path) => {
                        std::fs::write(path, &code).map_err(|e| e.to_string())?;
                        loop {
                            match receive_within::<TransferEvent>(&mut child, 120).await? {
                                TransferEvent::Done { peer } => {
                                    break Ok(format!("code {code}; peer {peer} received it"));
                                }
                                TransferEvent::Failed { reason, .. } => break Err(reason),
                                _ => {}
                            }
                        }
                    }
                },
                TransferEvent::Failed { reason, .. } if rendezvous.is_none() => Ok(format!(
                    "no rendezvous given; the worker reported: {reason}"
                )),
                other => Err(format!("{other:?}")),
            }
        };
        checks.report("transfer", "offers a file", transfer.await);

        let Some(code) = option(arguments, "--receive") else {
            return;
        };
        let expected = option(arguments, "--expect").unwrap_or_default();
        let download = async {
            use base64::Engine as _;
            let role = Role::Transfer { file: None };
            let command = command(&worker("transfer"), role, &[]).map_err(|e| e.to_string())?;
            let (mut child, _containment) = spawn(command)?;
            send(
                &mut child,
                1,
                TransferRequest::Receive {
                    code,
                    max_size: 1 << 20,
                    servers,
                },
            )
            .await?;
            let mut received = Vec::new();
            loop {
                match receive::<TransferEvent>(&mut child).await? {
                    TransferEvent::Data { bytes } => received.extend(
                        base64::engine::general_purpose::STANDARD
                            .decode(bytes)
                            .map_err(|e| e.to_string())?,
                    ),
                    TransferEvent::Done { .. } => break,
                    TransferEvent::Failed { reason, .. } => return Err(reason),
                    _ => {}
                }
            }
            if received == expected.as_bytes() {
                Ok(format!(
                    "{} bytes through the rendezvous and relay",
                    received.len()
                ))
            } else {
                Err(format!("received {:?}", String::from_utf8_lossy(&received)))
            }
        };
        checks.report("transfer", "receives a file", download.await);
    }
}
