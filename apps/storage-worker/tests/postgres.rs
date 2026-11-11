// SPDX-License-Identifier: EUPL-1.2
//! The storage worker as `nuschld` uses it (`--server`, its IPC on stdin and
//! stdout), against a throwaway PostgreSQL and against SQLite, with the same
//! script: migrations, history pages, idempotency, edits, deletions,
//! retention, live-only heads, erasure and purging.
//!
//! PostgreSQL comes from `initdb` and `postgres` in `NUSCHL_POSTGRES_BIN` or on
//! `PATH`; it runs from a temporary directory and listens only on a unix
//! socket there. Without those binaries the PostgreSQL test is skipped with a
//! message; `just test-postgres` runs it with PostgreSQL from nixpkgs.
use nuschl_ipc::{
    Envelope, read_frame,
    server::{
        Anchor, MessageKind, PageQuery, Retention, ServerStorageRequest as Request,
        ServerStorageResponse as Response, StoredBan, StoredMessage, StoredRetention, StoredUser,
    },
    write_frame,
};
use std::{
    io::{BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

const WORKER: &str = env!("CARGO_BIN_EXE_nuschl-storage-worker");

/// One storage worker process, spoken to as `nuschld` does.
struct Worker {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next: u64,
}

impl Worker {
    fn start(url_file: &Path) -> Self {
        Self::start_with(url_file, &[])
    }

    fn start_with(url_file: &Path, extra: &[&str]) -> Self {
        let mut child = Command::new(WORKER)
            .args(["--server", "--database-url-file"])
            .arg(url_file)
            .args(extra)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the storage worker starts");
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            next: 0,
        }
    }

    fn request(&mut self, request: Request) -> Response {
        let answer = self.raw(request);
        if let Response::Rejected(error) = &answer {
            panic!("the worker rejected request {}: {error}", self.next);
        }
        answer
    }

    fn raw(&mut self, request: Request) -> Response {
        self.next += 1;
        write_frame(&mut self.input, &Envelope::new(self.next, request)).unwrap();
        self.input.flush().unwrap();
        let answer: Envelope<Response> = read_frame(&mut self.output)
            .unwrap()
            .expect("the worker answers");
        assert_eq!(answer.request_id, self.next);
        answer.payload
    }

    fn count(&mut self, request: Request) -> u64 {
        match self.request(request) {
            Response::Count(count) => count,
            other => panic!("expected a count, got {other:?}"),
        }
    }

    fn page(&mut self, conversation: &str, anchor: Anchor) -> Vec<StoredMessage> {
        match self.request(Request::HistoryPage {
            query: PageQuery {
                conversation: conversation.into(),
                anchor,
                limit: 100,
                ranges: vec![(1, u64::MAX)],
                max_bytes: 256 * 1024,
            },
        }) {
            Response::Page(page) => page.rows,
            other => panic!("expected a page, got {other:?}"),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const ALICE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BOB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const CHANNEL: &str = "cccccccccccccccccccccccccccccccc";
const LIVE: &str = "dddddddddddddddddddddddddddddddd";

fn message(
    seq: u64,
    author: &str,
    content: &str,
    key: Option<&str>,
    sent_ms: i64,
) -> StoredMessage {
    StoredMessage {
        conversation: CHANNEL.into(),
        seq,
        author_account: Some(author.into()),
        author_user_id: Some(1),
        author_name: "Alice".into(),
        format: 1,
        content: content.into(),
        sent_ms,
        idempotency_key: key.map(str::to_owned),
        body_hash: key.map(|_| format!("hash of {content}")),
        ..StoredMessage::default()
    }
}

fn event(seq: u64, kind: MessageKind, target: u64, author: &str) -> StoredMessage {
    StoredMessage {
        kind,
        target: Some(target),
        content: String::new(),
        ..message(seq, author, "", None, 10_000 + seq as i64)
    }
}

/// The script, for any database.
fn exercise(worker: &mut Worker) {
    // Migrations ran when the worker connected; the state loads.
    assert!(matches!(
        worker.request(Request::Load),
        Response::Snapshot(_)
    ));
    worker.request(Request::SaveBans {
        bans: vec![StoredBan {
            address: Some("::ffff:192.0.2.1".into()),
            private_address: true,
            mask: 128,
            name: "Private".into(),
            hash: "ab".repeat(20),
            reason: "privacy regression".into(),
            start: 1,
            duration: 0,
        }],
    });
    private_ban_survived(worker);
    let user = StoredUser {
        id: 1,
        account: Some(ALICE.into()),
        name: "Alice".into(),
        password_hash: None,
        certificate_hash: Some("ab".repeat(20)),
        last_channel: None,
        last_active: None,
    };
    assert!(matches!(
        worker.request(Request::SaveUser { user }),
        Response::Done
    ));
    assert!(matches!(
        worker.request(Request::LoadUsers { after: None }),
        Response::Users(users) if users.iter().any(|user| user.name == "Alice")
    ));

    // Sends: seq 1–3 old (sent at 1000..), 4 recent; keys are scoped.
    for seq in 1..=4 {
        let sent = if seq == 4 {
            9_000_000
        } else {
            1_000 + seq as i64
        };
        let key = format!("{seq:032}");
        let row = message(seq, ALICE, &format!("m{seq}"), Some(&key), sent);
        assert!(matches!(
            worker.request(Request::HistoryAppend { message: row }),
            Response::Appended {
                duplicate: false,
                ..
            }
        ));
    }
    let retry = message(5, ALICE, "m2", Some(&format!("{:032}", 2)), 9_000_001);
    assert!(matches!(
        worker.request(Request::HistoryAppend { message: retry }),
        Response::Appended {
            seq: 2,
            duplicate: true
        }
    ));
    let conflict = message(6, ALICE, "other", Some(&format!("{:032}", 2)), 9_000_002);
    assert!(matches!(
        worker.request(Request::HistoryAppend { message: conflict }),
        Response::Conflict
    ));
    let rows = worker.page(CHANNEL, Anchor::Latest);
    assert_eq!(
        rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        (rows[4].kind, rows[5].kind),
        (MessageKind::Void, MessageKind::Void)
    );
    assert!(rows.iter().all(|row| row.idempotency_key.is_none()));

    // Edits by the author only, replacing the content.
    let edit = |seq, target, author: &str, content: &str| Request::HistoryEdit {
        target,
        author_account: author.into(),
        format: 1,
        content: content.into(),
        edit_limit: 5,
        event: event(seq, MessageKind::Edit, target, author),
    };
    assert!(matches!(
        worker.request(edit(7, 4, BOB, "x")),
        Response::Missing
    ));
    assert!(matches!(
        worker.request(edit(8, 4, ALICE, "m4 edited")),
        Response::Done
    ));
    // A deletion by the author leaves a tombstone.
    assert!(matches!(
        worker.request(Request::HistoryDelete {
            target: 3,
            author_account: Some(ALICE.into()),
            reason: 0,
            event: event(9, MessageKind::Delete, 3, ALICE),
        }),
        Response::Done
    ));
    let rows = worker.page(CHANNEL, Anchor::After(0));
    assert_eq!(rows[3].content, "m4 edited");
    assert_eq!(rows[3].edits, 1);
    assert_eq!(
        (rows[2].content.as_str(), rows[2].delete_reason),
        ("", Some(0))
    );

    // Retention: per conversation, in batches; expired rows stay tombstones.
    assert!(matches!(
        worker.request(Request::HistorySetRetention {
            conversation: CHANNEL.into(),
            retention: Some(Retention::Days(7)),
        }),
        Response::Done
    ));
    assert!(matches!(
        worker.request(Request::HistoryRetentions { after: None }),
        Response::Retentions(rows) if rows == [StoredRetention {
            conversation: CHANNEL.into(),
            retention: Retention::Days(7),
        }]
    ));
    let before = 5_000_000;
    let expirable = |worker: &mut Worker| {
        worker.count(Request::HistoryCountExpirable {
            conversation: CHANNEL.into(),
            before_ms: before,
        })
    };
    assert_eq!(expirable(worker), 2, "m1 and m2; m3 is already a tombstone");
    let expire = |worker: &mut Worker| {
        worker.count(Request::HistoryExpire {
            conversation: CHANNEL.into(),
            before_ms: before,
            deleted_ms: 20_000,
            limit: 1,
        })
    };
    assert_eq!((expire(worker), expire(worker), expire(worker)), (1, 1, 0));
    assert_eq!(expirable(worker), 0);
    let rows = worker.page(CHANNEL, Anchor::After(0));
    assert_eq!(
        rows[..4]
            .iter()
            .map(|row| (row.content.as_str(), row.delete_reason))
            .collect::<Vec<_>>(),
        [
            ("", Some(2)),
            ("", Some(2)),
            ("", Some(0)),
            ("m4 edited", None)
        ]
    );
    // An expired message cannot be edited back.
    assert!(matches!(
        worker.request(edit(10, 1, ALICE, "back")),
        Response::Missing
    ));

    // A live-only conversation keeps only its head, which only rises.
    for seq in [3, 7, 5] {
        assert!(matches!(
            worker.request(Request::HistoryAdvance {
                conversation: LIVE.into(),
                seq,
            }),
            Response::Done
        ));
    }
    assert!(matches!(
        worker.request(Request::HistoryHeads { after: None }),
        Response::Heads(heads) if heads == [(CHANNEL.to_owned(), 10), (LIVE.to_owned(), 7)]
    ));

    // The operator's erase, then purging the conversation.
    assert_eq!(
        worker.count(Request::HistoryErase {
            account: ALICE.into(),
            deleted_ms: 30_000,
        }),
        1,
        "only m4 was left"
    );
    assert!(
        worker
            .page(CHANNEL, Anchor::Latest)
            .iter()
            .all(|row| row.content.is_empty())
    );
    assert!(matches!(
        worker.request(Request::HistoryPurge {
            conversation: CHANNEL.into(),
        }),
        Response::Done
    ));
    assert!(matches!(
        worker.request(Request::HistoryRetentions { after: None }),
        Response::Retentions(rows) if rows.is_empty()
    ));
    assert!(matches!(
        worker.request(Request::HistoryHeads { after: None }),
        Response::Heads(heads) if heads == [(LIVE.to_owned(), 7)]
    ));
}

fn private_ban_survived(worker: &mut Worker) {
    let Response::Snapshot(snapshot) = worker.request(Request::Load) else {
        panic!("expected server state")
    };
    assert_eq!(snapshot.bans.len(), 1);
    assert!(snapshot.bans[0].private_address);
    assert_eq!(
        snapshot.bans[0].address.as_deref(),
        Some("::ffff:192.0.2.1")
    );
}

/// One holder of the operator lock at a time; the others never open the
/// database and answer everything with `Held`.
fn operator_lock(url_file: &Path) {
    let mut first = Worker::start_with(url_file, &["--operator-lock"]);
    assert!(matches!(
        first.request(Request::OperatorLock),
        Response::Done
    ));
    let mut second = Worker::start_with(url_file, &["--operator-lock"]);
    assert!(matches!(second.raw(Request::OperatorLock), Response::Held));
    assert!(matches!(second.raw(Request::Load), Response::Held));
    let check = Command::new(WORKER)
        .args([
            "--server",
            "--check-database",
            "--operator-lock",
            "--database-url-file",
        ])
        .arg(url_file)
        .env_clear()
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!check.success(), "the preflight refuses a held database");
    // A worker without the flag (the server's others) is not kept out.
    let mut unlocked = Worker::start(url_file);
    assert!(matches!(
        unlocked.request(Request::Load),
        Response::Snapshot(_)
    ));
    assert!(matches!(
        unlocked.raw(Request::OperatorLock),
        Response::Rejected(_)
    ));
    drop((first, second));
    let mut third = Worker::start_with(url_file, &["--operator-lock"]);
    assert!(matches!(
        third.request(Request::OperatorLock),
        Response::Done
    ));
}

fn url_file(directory: &Path, url: &str) -> PathBuf {
    let path = directory.join("database-url");
    std::fs::write(&path, url).unwrap();
    path
}

#[test]
fn the_server_schema_through_the_worker_on_sqlite() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("server.sqlite3");
    let url = format!("sqlite://{}", database.display());
    let url_file = url_file(directory.path(), &url);
    let mut worker = Worker::start(&url_file);
    exercise(&mut worker);
    drop(worker);
    private_ban_survived(&mut Worker::start(&url_file));
    operator_lock(&url_file);
}

/// A PostgreSQL server in a temporary directory, on a unix socket only.
struct Postgres {
    server: Child,
    pg_ctl: Option<PathBuf>,
    data: PathBuf,
    directory: tempfile::TempDir,
}

impl Postgres {
    fn binary(name: &str) -> Option<PathBuf> {
        let configured = std::env::var_os("NUSCHL_POSTGRES_BIN").map(PathBuf::from);
        let path = std::env::var_os("PATH").unwrap_or_default();
        configured
            .into_iter()
            .chain(std::env::split_paths(&path))
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    }

    fn start() -> Option<Self> {
        let initdb = Self::binary("initdb")?;
        let postgres = Self::binary("postgres")?;
        eprintln!("testing on PostgreSQL from {}", postgres.display());
        // A unix socket path has at most about 100 bytes.
        let mut directory = tempfile::Builder::new()
            .prefix("nuschl-pg")
            .tempdir()
            .ok()?;
        if directory.path().as_os_str().len() > 80 {
            directory = tempfile::Builder::new()
                .prefix("nuschl-pg")
                .tempdir_in("/tmp")
                .ok()?;
        }
        let data = directory.path().join("data");
        let status = Command::new(&initdb)
            .arg("-D")
            .arg(&data)
            .args([
                "-U",
                "nuschl",
                "-A",
                "trust",
                "-E",
                "UTF8",
                "--no-locale",
                "--no-sync",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .ok()?;
        assert!(status.success(), "initdb failed");
        let server = Command::new(&postgres)
            .arg("-D")
            .arg(&data)
            .arg("-k")
            .arg(directory.path())
            // No TCP at all, no fsync (a throwaway database).
            .args(["-h", "", "-F", "-c", "max_connections=20"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        Some(Self {
            server,
            pg_ctl: Self::binary("pg_ctl"),
            data,
            directory,
        })
    }

    fn url(&self) -> String {
        format!(
            "postgresql://nuschl@localhost/postgres?host={}",
            self.directory.path().display()
        )
    }

    /// Wait until the worker can connect (and has applied the migrations).
    fn wait_ready(&self, url_file: &Path) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let ready = Command::new(WORKER)
                .args(["--server", "--check-database", "--database-url-file"])
                .arg(url_file)
                .env_clear()
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if ready {
                return;
            }
            assert!(Instant::now() < deadline, "PostgreSQL did not come up");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        if let Some(pg_ctl) = &self.pg_ctl {
            let _ = Command::new(pg_ctl)
                .arg("-D")
                .arg(&self.data)
                .args(["-m", "immediate", "-w", "stop"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

#[test]
fn the_server_schema_through_the_worker_on_postgresql() {
    let Some(postgres) = Postgres::start() else {
        eprintln!(
            "SKIPPED: PostgreSQL (initdb, postgres) is not on PATH or in NUSCHL_POSTGRES_BIN; \
             run `just test-postgres` to test the PostgreSQL schema"
        );
        return;
    };
    let url_file = url_file(postgres.directory.path(), &postgres.url());
    postgres.wait_ready(&url_file);
    let mut worker = Worker::start(&url_file);
    exercise(&mut worker);
    // A second worker on the same database: the migrations are idempotent and
    // the data is shared (nuschld runs four).
    drop(worker);
    let mut again = Worker::start(&url_file);
    private_ban_survived(&mut again);
    assert!(matches!(
        again.request(Request::HistoryHeads { after: None }),
        Response::Heads(heads) if heads == [(LIVE.to_owned(), 7)]
    ));
    drop(again);
    operator_lock(&url_file);
}
