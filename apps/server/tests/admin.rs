// SPDX-License-Identifier: EUPL-1.2
//! `nuschld admin` on a stopped server's state (the in-memory backend; the
//! operator lock against a running server is tested with the real storage
//! worker in apps/storage-worker/tests/postgres.rs).
use clap::Parser;
use nuschl_ipc::server::{MessageKind, Retention, StoredMessage};
use nuschld::{
    admin::{self, Admin},
    config::{Config, Settings},
    server::start,
    state::Registered,
    storage::{Backend, MemoryStore},
};
use std::sync::{Arc, Mutex};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    admin: Admin,
}

fn settings(directory: &std::path::Path) -> Settings {
    Config::from_toml(&format!(
        "bind = [\"127.0.0.1\"]\nport = 0\nstate_directory = {directory:?}\n\
         [password_hashing]\nmemory_kib = 64\npasses = 1\n"
    ))
    .unwrap()
    .settings()
    .unwrap()
}

async fn run(
    settings: &Settings,
    store: &Arc<Mutex<MemoryStore>>,
    line: &[&str],
) -> Result<(), String> {
    let mut arguments = vec!["admin"];
    arguments.extend_from_slice(line);
    let cli = Cli::try_parse_from(arguments).map_err(|error| error.to_string())?;
    admin::run(settings.clone(), Backend::Memory(store.clone()), cli.admin)
        .await
        .map_err(|error| error.to_string())
}

/// A server that ran once: Alice and Bob registered, a channel with messages.
async fn prepared() -> (
    tempfile::TempDir,
    Settings,
    Arc<Mutex<MemoryStore>>,
    [u8; 16],
) {
    let directory = tempfile::tempdir().unwrap();
    let settings = settings(directory.path());
    let store = MemoryStore::shared();
    let running = start(settings.clone(), Backend::Memory(store.clone()))
        .await
        .unwrap();
    let conversation = {
        let mut state = running.server.lock();
        for (id, name) in [(1, "Alice"), (2, "Bob")] {
            state.registry.insert(Registered {
                id,
                account: [id as u8; 16],
                name: name.into(),
                password_hash: None,
                certificate_hash: Some(format!("{id:040}")),
                last_channel: None,
                last_active: None,
            });
            state.persist_registration(id);
        }
        state.registry.next_id = 3;
        let mut channel = nuschld::state::Channel::new(1, Some(0), "Lobby".into());
        channel.node.acls.push(nuschld::acl::AclEntry {
            apply_here: true,
            apply_subs: true,
            user_id: Some(2),
            group: String::new(),
            grant: nuschld::acl::perm::WRITE,
            deny: 0,
        });
        let conversation = channel.conversation_id;
        state.conversations.add_channel(conversation, 1);
        state.channels.0.insert(1, channel);
        state.channels.get_mut(0).unwrap().children.push(1);
        state.persist_channel(1);
        conversation
    };
    running.shutdown().await;
    // Two old messages and a recent one, as the server would have stored them.
    let now = nuschld::chat::now_ms();
    let mut memory = store.lock().unwrap();
    for (seq, author, age_days, content) in [
        (1, 1_u8, 40, "old from Alice"),
        (2, 2, 35, "old from Bob"),
        (3, 1, 0, "recent from Alice"),
    ] {
        memory.history.messages.insert(
            (hex::encode(conversation), seq),
            StoredMessage {
                conversation: hex::encode(conversation),
                seq,
                kind: MessageKind::Message,
                author_account: Some(hex::encode([author; 16])),
                author_user_id: Some(u32::from(author)),
                author_name: if author == 1 { "Alice" } else { "Bob" }.into(),
                format: 1,
                content: content.into(),
                sent_ms: now - age_days * 86_400_000,
                ..StoredMessage::default()
            },
        );
    }
    drop(memory);
    (directory, settings, store, conversation)
}

#[tokio::test]
async fn users_are_listed_renamed_given_passwords_and_removed() {
    let (directory, settings, store, _) = prepared().await;
    let password = directory.path().join("password");
    std::fs::write(&password, "correct horse\n").unwrap();
    for user in ["alice", "#0"] {
        run(
            &settings,
            &store,
            &[
                "users",
                "set-password",
                user,
                "--password-file",
                password.to_str().unwrap(),
            ],
        )
        .await
        .unwrap();
    }
    run(&settings, &store, &["users", "list"]).await.unwrap();
    run(&settings, &store, &["users", "show", "alice"])
        .await
        .unwrap();
    run(&settings, &store, &["users", "rename", "alice", "Alicia"])
        .await
        .unwrap();
    let taken = run(&settings, &store, &["users", "rename", "#1", "bob"]).await;
    assert!(taken.unwrap_err().contains("already called"));
    let invalid = run(&settings, &store, &["users", "rename", "#1", "bad>name"]).await;
    assert!(invalid.unwrap_err().contains("not a valid user name"));
    assert!(
        run(&settings, &store, &["users", "rename", "#0", "Root"])
            .await
            .is_err(),
        "SuperUser keeps its name"
    );
    // Removing needs --yes; it cascades the ACL entry naming Bob.
    let unconfirmed = run(&settings, &store, &["users", "remove", "bob"]).await;
    assert!(unconfirmed.unwrap_err().contains("--yes"));
    run(&settings, &store, &["users", "remove", "bob", "--yes"])
        .await
        .unwrap();
    assert!(
        run(&settings, &store, &["users", "remove", "#0", "--yes"])
            .await
            .is_err()
    );
    let opened = admin::open(&settings, Backend::Memory(store.clone()))
        .await
        .unwrap();
    let state = &opened.state;
    assert_eq!(state.registry.users[&1].name, "Alicia");
    for id in [0, 1] {
        let hash = state.registry.users[&id].password_hash.clone().unwrap();
        assert_eq!(
            nuschld::password::verify("correct horse".into(), hash).await,
            Some(true)
        );
    }
    assert!(!state.registry.users.contains_key(&2));
    assert!(state.channels.get(1).unwrap().node.acls.is_empty());
    opened.close().await;
}

#[tokio::test]
async fn shortening_a_channels_retention_needs_yes() {
    let (_directory, settings, store, conversation) = prepared().await;
    run(&settings, &store, &["channels", "list"]).await.unwrap();
    let refused = run(&settings, &store, &["channels", "retention", "Lobby", "30"]).await;
    let message = refused.unwrap_err();
    assert!(message.contains("removes 2 stored messages"), "{message}");
    assert!(message.contains("--yes"));
    run(
        &settings,
        &store,
        &["channels", "retention", "Lobby", "30", "--yes"],
    )
    .await
    .unwrap();
    // Keeping more needs no confirmation; nonsense is refused.
    run(
        &settings,
        &store,
        &["channels", "retention", "1", "forever"],
    )
    .await
    .unwrap();
    assert!(
        run(&settings, &store, &["channels", "retention", "1", "0"])
            .await
            .is_err()
    );
    assert!(
        run(
            &settings,
            &store,
            &["channels", "retention", "Nowhere", "7"]
        )
        .await
        .is_err()
    );
    run(
        &settings,
        &store,
        &["channels", "retention", "Lobby", "7", "--yes"],
    )
    .await
    .unwrap();
    let opened = admin::open(&settings, Backend::Memory(store.clone()))
        .await
        .unwrap();
    assert_eq!(opened.state.retention(&conversation), Retention::Days(7));
    opened.close().await;
    // The export leaves out what the retention no longer keeps.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lobby.json");
    run(
        &settings,
        &store,
        &[
            "history",
            "export",
            "Lobby",
            "--output",
            path.to_str().unwrap(),
        ],
    )
    .await
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let exported: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(exported["name"], "Lobby");
    assert_eq!(exported["retention"]["days"], 7);
    let messages = exported["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["content"], "");
    assert_eq!(messages[0]["delete_reason"], 2);
    assert_eq!(messages[2]["content"], "recent from Alice");
    assert_eq!(messages[2]["author"]["name"], "Alice");
    assert!(messages[2].get("idempotency_key").is_none());
}

#[tokio::test]
async fn bans_and_erasure() {
    let (_directory, settings, store, conversation) = prepared().await;
    run(
        &settings,
        &store,
        &[
            "bans",
            "add",
            "--address",
            "192.0.2.0/24",
            "--reason",
            "spam",
        ],
    )
    .await
    .unwrap();
    run(
        &settings,
        &store,
        &[
            "bans",
            "add",
            "--certificate",
            &"ab".repeat(20),
            "--duration",
            "3600",
        ],
    )
    .await
    .unwrap();
    assert!(
        run(&settings, &store, &["bans", "add", "--reason", "nothing"])
            .await
            .is_err()
    );
    assert!(
        run(
            &settings,
            &store,
            &["bans", "add", "--address", "not an address"]
        )
        .await
        .is_err()
    );
    run(&settings, &store, &["bans", "list"]).await.unwrap();
    {
        let opened = admin::open(&settings, Backend::Memory(store.clone()))
            .await
            .unwrap();
        let bans = &opened.state.bans;
        assert_eq!(bans.len(), 2);
        assert!(bans[0].matches_address("192.0.2.99".parse().unwrap()));
        assert!(!bans[0].matches_address("192.0.3.1".parse().unwrap()));
        assert_eq!(bans[1].duration, 3600);
        opened.close().await;
    }
    run(&settings, &store, &["bans", "remove", "0"])
        .await
        .unwrap();
    assert!(
        run(&settings, &store, &["bans", "remove", "5"])
            .await
            .is_err()
    );
    // Erasing needs --yes.
    let refused = run(&settings, &store, &["history", "erase-user", "alice"]).await;
    assert!(refused.unwrap_err().contains("--yes"));
    run(
        &settings,
        &store,
        &["history", "erase-user", "alice", "--yes"],
    )
    .await
    .unwrap();
    let memory = store.lock().unwrap();
    let rows = &memory.history.messages;
    assert_eq!(rows[&(hex::encode(conversation), 1)].content, "");
    assert_eq!(rows[&(hex::encode(conversation), 1)].delete_reason, Some(3));
    assert_eq!(
        rows[&(hex::encode(conversation), 2)].content,
        "old from Bob"
    );
    assert_eq!(rows[&(hex::encode(conversation), 3)].content, "");
}
