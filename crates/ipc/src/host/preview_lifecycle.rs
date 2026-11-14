// SPDX-License-Identifier: EUPL-1.2
//! Preview-specific namespace lifetime. Wrapper exit alone is not role exit.
//!
//! Linux bubblewrap reports its namespace init before releasing setup, closes
//! the selected status FD in the sandbox child, and keeps a monitor outside.
//! A retained proc stat file names that original init even if its numeric PID
//! is later reused. The kernel completes namespace teardown before init is a
//! zombie. Missing identity or an ambiguous observation never releases credit.

#[cfg(target_os = "linux")]
use std::{fs::File, io, time::Duration};
use tokio::process::Child;
#[cfg(target_os = "linux")]
use tokio::{io::AsyncReadExt as _, process::ChildStderr};

use super::Error;

#[cfg(target_os = "linux")]
const STATUS_BYTES: usize = 2048;
#[cfg(target_os = "linux")]
const PROC_BYTES: usize = 16 * 1024;

/// Must remain owned alongside the process permit, including failed readiness.
/// Dropping this value is not a retirement operation.
pub struct Retirement {
    #[cfg(target_os = "linux")]
    wrapper: Option<u32>,
    #[cfg(target_os = "linux")]
    stderr: Option<ChildStderr>,
    #[cfg(target_os = "linux")]
    init: Option<File>,
    attempted: bool,
}

impl Retirement {
    pub fn new(child: &mut Child) -> Self {
        #[cfg(not(target_os = "linux"))]
        let _ = child;
        Self {
            #[cfg(target_os = "linux")]
            wrapper: child.id(),
            #[cfg(target_os = "linux")]
            stderr: child.stderr.take(),
            #[cfg(target_os = "linux")]
            init: None,
            attempted: false,
        }
    }

    /// Call exactly once, before writing any request bytes. On error or caller
    /// cancellation, still call `retire`; its unknown-state wait is intentional.
    pub async fn ready(&mut self) -> Result<(), Error> {
        if self.attempted {
            return Err(Error::InvalidResponse);
        }
        self.attempted = true;
        #[cfg(target_os = "linux")]
        {
            let wrapper = self.wrapper.ok_or(Error::InvalidResponse)?;
            let stderr = self.stderr.as_mut().ok_or(Error::InvalidResponse)?;
            let bytes = tokio::time::timeout(Duration::from_secs(5), status_line(stderr))
                .await
                .map_err(|_| Error::Timeout)??;
            let receipt: Receipt = serde_json::from_slice(&bytes)?;
            let init = observe_init(wrapper, receipt)?;
            self.init = Some(init);
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(nuschl_sandbox::Error::MissingSandboxHelper(
                "preview role retirement on this platform",
            )
            .into())
        }
    }

    /// Reap the wrapper and observe original namespace-init retirement. The
    /// owner must await this even after a logical timeout or readiness failure.
    /// No observation is weaker than an indefinitely occupied process slot.
    #[cfg(target_os = "linux")]
    pub async fn retire(mut self, child: &mut Child) {
        nuschl_sandbox::reap(child).await;
        loop {
            if let Some(init) = self.init.as_mut()
                && init_retired(init)
            {
                // Only trusted bwrap retains this FD; the sandbox child closed
                // it before exec. Discard bounded diagnostics after retirement.
                if let Some(stderr) = self.stderr.take() {
                    let mut bounded = stderr.take(STATUS_BYTES as u64);
                    let mut scratch = [0; 256];
                    let drain =
                        async { while bounded.read(&mut scratch).await.is_ok_and(|n| n != 0) {} };
                    let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn retire(self, child: &mut Child) {
        nuschl_sandbox::reap(child).await;
        std::future::pending::<()>().await;
    }
}

#[cfg(target_os = "linux")]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    #[serde(rename = "child-pid")]
    child: u32,
    #[serde(rename = "pid-namespace")]
    pid_namespace: u64,
    #[serde(rename = "mnt-namespace")]
    _mnt: Option<u64>,
    #[serde(rename = "cgroup-namespace")]
    _cgroup: Option<u64>,
    #[serde(rename = "ipc-namespace")]
    _ipc: Option<u64>,
    #[serde(rename = "net-namespace")]
    _net: Option<u64>,
    #[serde(rename = "uts-namespace")]
    _uts: Option<u64>,
}

#[cfg(target_os = "linux")]
async fn status_line(stderr: &mut ChildStderr) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::with_capacity(STATUS_BYTES);
    for _ in 0..STATUS_BYTES {
        let byte = stderr.read_u8().await?;
        bytes.push(byte);
        if byte == b'\n' {
            return Ok(bytes);
        }
    }
    Err(Error::InvalidResponse)
}

#[cfg(target_os = "linux")]
fn observe_init(wrapper: u32, receipt: Receipt) -> Result<File, Error> {
    use std::os::unix::fs::MetadataExt as _;
    if receipt.child == 0 || receipt.child > i32::MAX as u32 || receipt.pid_namespace == 0 {
        return Err(Error::InvalidResponse);
    }
    let base = format!("/proc/{}", receipt.child);
    let mut stat = File::open(format!("{base}/stat"))?;
    let first = read_stat(&mut stat)?;
    if first.pid != receipt.child || first.parent != wrapper || first.started == 0 {
        return Err(Error::InvalidResponse);
    }
    let namespace = std::fs::metadata(format!("{base}/ns/pid"))?.ino();
    if namespace != receipt.pid_namespace
        || namespace == std::fs::metadata("/proc/self/ns/pid")?.ino()
    {
        return Err(Error::InvalidResponse);
    }
    let status = bounded_proc(&mut File::open(format!("{base}/status"))?)?;
    let status = std::str::from_utf8(&status).map_err(|_| Error::InvalidResponse)?;
    let ids = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .ok_or(Error::InvalidResponse)?;
    if ids.split_ascii_whitespace().last() != Some("1") {
        return Err(Error::InvalidResponse);
    }
    // Numeric path lookups above must not have raced the original task's exit.
    let last = read_stat(&mut stat)?;
    if first.pid != last.pid || first.started != last.started || last.parent != wrapper {
        return Err(Error::InvalidResponse);
    }
    Ok(stat)
}

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
struct Stat {
    pid: u32,
    parent: u32,
    started: u64,
    state: u8,
}

#[cfg(target_os = "linux")]
fn parse_stat(bytes: &[u8]) -> Result<Stat, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidResponse)?;
    let (pid, rest) = text.split_once(" (").ok_or(Error::InvalidResponse)?;
    let (_, rest) = rest.rsplit_once(") ").ok_or(Error::InvalidResponse)?;
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next().ok_or(Error::InvalidResponse)?.as_bytes();
    if state.len() != 1 {
        return Err(Error::InvalidResponse);
    }
    let parent = fields.next().ok_or(Error::InvalidResponse)?;
    let started = fields.nth(17).ok_or(Error::InvalidResponse)?;
    Ok(Stat {
        pid: pid.parse().map_err(|_| Error::InvalidResponse)?,
        parent: parent.parse().map_err(|_| Error::InvalidResponse)?,
        started: started.parse().map_err(|_| Error::InvalidResponse)?,
        state: state[0],
    })
}

#[cfg(target_os = "linux")]
fn bounded_proc(file: &mut File) -> Result<Vec<u8>, Error> {
    use io::{Read as _, Seek as _, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take((PROC_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > PROC_BYTES {
        return Err(Error::InvalidResponse);
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn read_stat(file: &mut File) -> Result<Stat, Error> {
    parse_stat(&bounded_proc(file)?)
}

#[cfg(target_os = "linux")]
fn init_retired(file: &mut File) -> bool {
    match read_stat(file) {
        Ok(stat) => stat.state == b'Z',
        Err(Error::Io(error)) => error.raw_os_error() == Some(3), // ESRCH, original inode only.
        Err(_) => false,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn stat_identity_is_parsed_without_process_name_ambiguity() {
        let mut text = "123 (name ) with spaces) S 99".to_owned();
        for _ in 0..17 {
            text.push_str(" 0");
        }
        text.push_str(" 456 0 0\n");
        assert_eq!(
            parse_stat(text.as_bytes()).unwrap(),
            Stat {
                pid: 123,
                parent: 99,
                started: 456,
                state: b'S',
            }
        );
        for bad in [
            b"".as_slice(),
            b"123 (x) ZZ 99",
            b"123 x S 99",
            b"-1 (x) Z 99",
        ] {
            assert!(parse_stat(bad).is_err());
        }
    }

    #[test]
    fn trusted_receipt_requires_typed_namespace_identity() {
        for bad in [
            r#"{}"#,
            r#"{"child-pid":12}"#,
            r#"{"child-pid":12,"pid-namespace":"1"}"#,
            r#"{"child-pid":12,"pid-namespace":1,"extra":true}"#,
        ] {
            assert!(serde_json::from_str::<Receipt>(bad).is_err());
        }
        let receipt: Receipt =
            serde_json::from_str(r#"{"child-pid":12,"pid-namespace":123}"#).unwrap();
        assert_eq!(receipt.child, 12);
        assert_eq!(receipt.pid_namespace, 123);
    }
}
