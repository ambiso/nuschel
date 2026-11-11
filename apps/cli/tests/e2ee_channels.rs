// SPDX-License-Identifier: EUPL-1.2
//! End-to-end-encrypted channels (phase 4c, docs/e2ee.md §6.2) between
//! `nuschl-cli` processes and `nuschld` on loopback: lazy external joins, several
//! devices of one person, activity for readers who are not members, an ACL change
//! that removes a member, and a device removed after 90 days without being seen
//! (the server's clock is moved). Build the crypto worker first:
//! `cargo build -p nuschl-crypto-worker`.
mod common;

use common::*;
use nuschld::{
    acl::{AclEntry, perm},
    conversations::Scope,
    server::Running,
};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio};

const DAY_MS: i64 = 86_400_000;

/// Registered users may make permanent channels (the default lets them make only
/// temporary ones, which keep no history: no stream for MLS).
fn let_users_make_channels(running: &Running) {
    let mut state = running.server.lock();
    state.channels.get_mut(0).unwrap().node.acls.push(AclEntry {
        apply_here: true,
        apply_subs: true,
        user_id: None,
        group: "auth".into(),
        grant: perm::MAKE_CHANNEL,
        deny: 0,
    });
    state.permissions_changed(None);
    state.authz_changed(Scope::All);
    state.finish();
}

/// Denies `user_id` Enter in `channel`: it can no longer read it.
fn deny_enter(running: &Running, channel: u32, user_id: u32) {
    let mut state = running.server.lock();
    state
        .channels
        .get_mut(channel)
        .unwrap()
        .node
        .acls
        .push(AclEntry {
            apply_here: true,
            apply_subs: true,
            user_id: Some(user_id),
            group: String::new(),
            grant: 0,
            deny: perm::ENTER,
        });
    state.permissions_changed(None);
    state.authz_changed(Scope::All);
    state.finish();
}

impl Cli {
    /// The channel's conversation id, once the server named it.
    async fn conversation_of(&mut self, channel: u64) -> String {
        self.wait("the channel's conversation", |event| {
            (event["kind"] == "channel_info" && event["channel"].as_u64() == Some(channel))
                .then(|| event["conversation"].as_str().map(str::to_owned))
                .flatten()
        })
        .await
    }

    /// Opens the channel's end-to-end conversation and waits until this device is
    /// a member of a group with `members` devices, all verified.
    async fn open_channel(&mut self, conversation: &str, members: usize) -> Value {
        self.send(json!({"kind":"e2ee_open_channel", "conversation":conversation}))
            .await;
        self.channel_view(conversation, members).await
    }

    /// The conversation's view (alone, or with a commit) once the group has
    /// `members` devices and this one is in it.
    async fn channel_view(&mut self, conversation: &str, members: usize) -> Value {
        self.wait(&format!("a channel group of {members}"), |event| {
            let view = if event["kind"] == "e2ee_received" {
                &event["received"]["view"]
            } else {
                &event["view"]
            };
            ((event["kind"] == "e2ee_conversation" || event["kind"] == "e2ee_received")
                && event["conversation"] == conversation
                && view["channel"] == true
                && view["pinned"] == true
                && view["active"] == true
                && view["members"]
                    .as_array()
                    .is_some_and(|list| list.len() == members)
                && view["lock"]["state"] == "encrypted")
                .then(|| event.clone())
        })
        .await
    }
}

/// Bob's phone, linked to his laptop and using his account's certificate.
async fn bob_with_two_devices(
    running: &Running,
    state: &Path,
    profile: &Path,
    laptop: &Path,
    phone_profile: &Path,
    phone: &Path,
) -> std::path::PathBuf {
    let mut device = Cli::spawn(running, "bob", profile, Some(laptop), &[]).await;
    device.key_packages_uploaded().await;
    device.quit().await;
    let certificate = state.join("bob.p12");
    let exported = Cli::command(running, "bob", profile)
        .arg("--export-certificate")
        .arg(&certificate)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(exported.success());
    link(running, profile, laptop, phone).await;
    let import: [&std::ffi::OsStr; 2] = ["--import-certificate".as_ref(), certificate.as_os_str()];
    let mut device = Cli::spawn(running, "bob", phone_profile, Some(phone), &import).await;
    device.key_packages_uploaded().await;
    device.quit().await;
    certificate
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_people_and_four_devices_in_an_encrypted_channel() {
    if !worker_present() {
        panic!("build nuschl-crypto-worker first: cargo build -p nuschl-crypto-worker");
    }
    let state = tempfile::tempdir().unwrap();
    let running = server(state.path()).await;
    let_users_make_channels(&running);
    let directories: Vec<tempfile::TempDir> =
        (0..10).map(|_| tempfile::tempdir().unwrap()).collect();
    let path = |index: usize| directories[index].path();
    let (alice_profile, alice_identity) = (path(0), path(1));
    let (bob_profile, laptop_identity) = (path(2), path(3));
    let (phone_profile, phone_identity) = (path(4), path(5));
    let (carol_profile, carol_identity) = (path(6), path(7));
    let (dave_profile, dave_identity) = (path(8), path(9));
    Cli::register(&running, "alice", alice_profile).await;
    Cli::register(&running, "bob", bob_profile).await;
    let carol_id = Cli::register(&running, "carol", carol_profile).await;
    Cli::register(&running, "dave", dave_profile).await;
    bob_with_two_devices(
        &running,
        state.path(),
        bob_profile,
        laptop_identity,
        phone_profile,
        phone_identity,
    )
    .await;

    // Alice makes a channel and opens it first: she creates its group, alone.
    let mut alice = Cli::spawn(&running, "alice", alice_profile, Some(alice_identity), &[]).await;
    alice.key_packages_uploaded().await;
    alice
        .send(json!({"kind":"create_channel", "parent":0, "name":"Secret", "temporary":false}))
        .await;
    let channel = alice
        .wait("the new channel", |event| {
            (event["kind"] == "channel" && event["channel"]["name"] == "Secret")
                .then(|| event["channel"]["id"].as_u64())
                .flatten()
        })
        .await;
    let conversation = alice.conversation_of(channel).await;
    alice.open_channel(&conversation, 1).await;

    alice.sent(&conversation, "before anyone else joined").await;

    // Carol and Bob's laptop join lazily, by external commit, when they open it;
    // Alice sees each join and checks the joiner in key transparency.
    let mut carol = Cli::spawn(&running, "carol", carol_profile, Some(carol_identity), &[]).await;
    carol.key_packages_uploaded().await;
    assert_eq!(carol.conversation_of(channel).await, conversation);
    carol.open_channel(&conversation, 2).await;
    alice.channel_view(&conversation, 2).await;
    // The first online existing member grants retained keys. A new member
    // reads the old text from its signed archive, not past MLS epoch secrets.
    carol
        .send(json!({"kind":"e2ee_history", "conversation":conversation}))
        .await;
    carol
        .wait("history from before joining", |event| {
            (event["kind"] == "history_page"
                && event["messages"].as_array().is_some_and(|messages| {
                    messages
                        .iter()
                        .any(|message| message["content"] == "before anyone else joined")
                }))
            .then_some(())
        })
        .await;
    let mut laptop = Cli::spawn(&running, "bob", bob_profile, Some(laptop_identity), &[]).await;
    laptop.open_channel(&conversation, 3).await;
    alice.channel_view(&conversation, 3).await;
    carol.channel_view(&conversation, 3).await;

    // Dave may read the channel but never opened it: not a member, yet he learns
    // that something was said, and where.
    let mut dave = Cli::spawn(&running, "dave", dave_profile, Some(dave_identity), &[]).await;
    dave.key_packages_uploaded().await;
    dave.conversation_of(channel).await;

    let seq = alice.sent(&conversation, "hello, channel").await;
    let message = carol.received("hello, channel").await;
    assert_eq!(message["received"]["seq"], seq);
    assert_eq!(message["author"]["name"], "alice", "{message}");
    assert_eq!(message["received"]["sender"]["verified"], true, "{message}");
    laptop.received("hello, channel").await;
    dave.wait("the channel's activity", |event| {
        (event["kind"] == "e2ee_activity"
            && event["conversation"] == conversation
            && event["seq"].as_u64() == Some(seq))
        .then_some(())
    })
    .await;
    assert!(
        !dave
            .backlog
            .iter()
            .any(|event| event["kind"] == "e2ee_received"),
        "a reader who is not a member decrypts nothing"
    );

    // Bob's second device joins by itself too (the laptop goes: one connection
    // per account).
    laptop.quit().await;
    let mut phone = Cli::spawn(&running, "bob", phone_profile, Some(phone_identity), &[]).await;
    phone.open_channel(&conversation, 4).await;
    alice.channel_view(&conversation, 4).await;
    phone.sent(&conversation, "the phone is here").await;
    alice.received("the phone is here").await;
    carol.received("the phone is here").await;

    // An ACL change takes Carol's access: the server stops delivering to her at
    // once and proposes her device's removal; a member commits it, and the next
    // epoch's keys never reach her.
    deny_enter(&running, channel as u32, carol_id as u32);
    carol
        .wait("losing the channel", |event| {
            (event["kind"] == "unsubscribed" && event["conversation"] == conversation).then_some(())
        })
        .await;
    let removed = alice
        .wait("Carol removed from the group", |event| {
            let view = if event["kind"] == "e2ee_received" {
                &event["received"]["view"]
            } else {
                &event["view"]
            };
            (event["conversation"] == conversation
                && view["members"].as_array().is_some_and(|members| {
                    members.len() == 3
                        && members
                            .iter()
                            .all(|member| member["device_name"] != "carol (nuschl-cli)")
                }))
            .then(|| event.clone())
        })
        .await;
    let view = if removed["kind"] == "e2ee_received" {
        &removed["received"]["view"]
    } else {
        &removed["view"]
    };
    assert!(
        view["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|member| member["device_name"] != "carol (nuschl-cli)"),
        "{removed}"
    );
    phone.channel_view(&conversation, 3).await;
    let after = alice.sent(&conversation, "carol cannot read this").await;
    phone.received("carol cannot read this").await;
    assert!(after > seq);
    assert!(
        !carol
            .backlog
            .iter()
            .any(|event| event["kind"] == "e2ee_received"
                && event["received"]["text"] == "carol cannot read this"),
        "a removed member reads nothing new"
    );

    dave.quit().await;
    phone.quit().await;
    carol.quit().await;
    alice.quit().await;
}

/// A device not seen for 90 days is removed from its groups (docs/e2ee.md §3.6,
/// the user's answer 8): the server proposes it, a member commits it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_unseen_for_90_days_is_removed() {
    if !worker_present() {
        panic!("build nuschl-crypto-worker first: cargo build -p nuschl-crypto-worker");
    }
    let state = tempfile::tempdir().unwrap();
    let running = server(state.path()).await;
    let directories: Vec<tempfile::TempDir> =
        (0..6).map(|_| tempfile::tempdir().unwrap()).collect();
    let path = |index: usize| directories[index].path();
    let (alice_profile, alice_identity) = (path(0), path(1));
    let (bob_profile, laptop_identity) = (path(2), path(3));
    let (phone_profile, phone_identity) = (path(4), path(5));
    Cli::register(&running, "alice", alice_profile).await;
    let bob_id = Cli::register(&running, "bob", bob_profile).await;
    bob_with_two_devices(
        &running,
        state.path(),
        bob_profile,
        laptop_identity,
        phone_profile,
        phone_identity,
    )
    .await;

    let mut alice = Cli::spawn(&running, "alice", alice_profile, Some(alice_identity), &[]).await;
    alice.key_packages_uploaded().await;
    alice
        .send(json!({"kind":"e2ee_open", "user_id":bob_id}))
        .await;
    let view = alice.established(None).await;
    let conversation = view["conversation"].as_str().unwrap().to_owned();
    assert_eq!(
        view["view"]["members"].as_array().unwrap().len(),
        3,
        "{view}"
    );

    // The phone stays in use; the laptop is gone for three months.
    let mut phone = Cli::spawn(&running, "bob", phone_profile, Some(phone_identity), &[]).await;
    phone.established(Some(&conversation)).await;
    running
        .server
        .lock()
        .mls
        .as_ref()
        .unwrap()
        .advance_clock(89 * DAY_MS);
    running.server.lock().mls_sweep();
    // Not yet: 89 days.
    alice.sent(&conversation, "still three of us").await;
    phone.received("still three of us").await;
    running
        .server
        .lock()
        .mls
        .as_ref()
        .unwrap()
        .advance_clock(2 * DAY_MS);
    running.server.lock().mls_sweep();
    let removed = alice
        .wait("the laptop removed", |event| {
            (event["kind"] == "e2ee_conversation"
                && event["conversation"] == conversation
                && event["view"]["members"]
                    .as_array()
                    .is_some_and(|list| list.len() == 2))
            .then(|| event.clone())
        })
        .await;
    assert!(
        removed["view"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|member| member["device_name"] != "bob (nuschl-cli)"),
        "{removed}"
    );
    alice.sent(&conversation, "two of us now").await;
    phone.received("two of us now").await;
    phone.quit().await;
    alice.quit().await;
}
