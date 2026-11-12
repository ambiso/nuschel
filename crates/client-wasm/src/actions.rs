// SPDX-License-Identifier: EUPL-1.2
//! Bounded scalar provenance for actions from an older browser UI publication.
//! This is neither a permission grant nor a second server/entity model.
use nuschl_client::{
    model::TextTarget,
    server::{ServerState, Status},
};
use serde::{Deserialize, Serialize};

pub const MAX_SERVERS: usize = 128;
const STALE: &str = "This view changed before the action could run. Your draft was kept; use the updated view and try again.";

#[derive(Default)]
pub struct Catalog {
    entries: Vec<u64>,
    next: u64,
}
impl Catalog {
    /// Reserve before any model mutation. Failed work may consume a number,
    /// but can never reuse an old offline bookmark lifetime.
    pub fn reserve(&mut self, count: usize, replacing: Option<usize>) -> Result<u64, String> {
        if self.entries.len() != count || replacing.is_some_and(|index| index >= count) {
            return Err(STALE.into());
        }
        if replacing.is_none() && count >= MAX_SERVERS {
            return Err("At most 128 servers can be saved in this browser.".into());
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or("Browser catalog counter exhausted.")?;
        Ok(self.next)
    }
    pub fn commit(&mut self, index: usize, value: u64) {
        if index == self.entries.len() {
            self.entries.push(value);
        } else {
            self.entries[index] = value;
        }
    }
    pub fn remove(&mut self, index: usize, count: usize) -> Result<(), String> {
        if self.entries.len() != count || index >= count {
            return Err(STALE.into());
        }
        self.entries.remove(index);
        Ok(())
    }
    pub fn get(&self, index: usize) -> Option<u64> {
        self.entries.get(index).copied()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerRef {
    pub id: String,
    pub catalog_instance: String,
    pub generation: String,
    pub own_instance: Option<String>,
    pub muted: bool,
    pub deafened: bool,
}
impl ServerRef {
    pub fn capture(server: &ServerState, catalog: u64) -> Self {
        Self {
            id: server.id.clone(),
            catalog_instance: catalog.to_string(),
            generation: server.generation.to_string(),
            own_instance: server
                .own_session
                .and_then(|id| server.users.get(&id))
                .map(|user| user.instance.to_string()),
            muted: server.muted,
            deafened: server.deafened,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerRef {
    pub session: u32,
    pub instance: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selected {
    channel: u32,
    instance: String,
    peer: Option<PeerRef>,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Channel {
        id: u32,
        instance: String,
        peer: Option<PeerRef>,
    },
    User {
        id: u32,
        instance: String,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Voice {
    server_id: String,
    connection_generation: String,
    voice_generation: String,
    channel_id: u32,
    muted: bool,
    deafened: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    server: ServerRef,
    selected: Option<Selected>,
    target: Option<Target>,
    voice: Option<Voice>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    method: String,
    index: Option<usize>,
    target: Option<u32>,
    subject: Option<String>,
}
fn counter(value: &str) -> Option<u64> {
    if value.is_empty() || value.len() > 20 {
        return None;
    }
    let result = value.parse::<u64>().ok()?;
    (result.to_string() == value).then_some(result)
}
fn require(ok: bool) -> Result<(), String> {
    if ok { Ok(()) } else { Err(STALE.into()) }
}
fn server<'a>(
    servers: &'a [ServerState],
    catalog: &Catalog,
    index: usize,
    proof: &ServerRef,
    connection: bool,
    live: bool,
) -> Result<&'a ServerState, String> {
    let current = servers.get(index).ok_or(STALE)?;
    require(
        proof.id.len() <= 512
            && current.id == proof.id
            && catalog
                .get(index)
                .is_some_and(|value| counter(&proof.catalog_instance) == Some(value)),
    )?;
    if connection {
        require(counter(&proof.generation) == Some(current.generation))?;
    }
    if live {
        require(current.status == Status::Connected)?;
        let own = current
            .own_session
            .and_then(|id| current.users.get(&id))
            .ok_or(STALE)?;
        require(
            own.instance != 0
                && proof.own_instance.as_deref().and_then(counter) == Some(own.instance),
        )?;
    }
    Ok(current)
}

/// `call` is a projection of the concrete method arguments, not a UI claim
/// about which parameters that method will later use. The adapter validates
/// this synchronously and invokes the corresponding existing method without
/// yielding; current method/transport checks remain authoritative afterwards.
pub fn validate(
    servers: &[ServerState],
    catalog: &Catalog,
    selection: Option<(usize, usize)>,
    voice: Option<&crate::voice::VoiceView>,
    call: &str,
    proof: &str,
) -> Result<(), String> {
    if call.len() > 512 || proof.len() > 4096 {
        return Err(STALE.into());
    }
    let call: Call = serde_json::from_str(call).map_err(|_| STALE)?;
    let proof: Proof = serde_json::from_str(proof).map_err(|_| STALE)?;
    if call.method.len() > 32 {
        return Err(STALE.into());
    }
    match call.method.as_str() {
        "send_text" | "older" => {
            require(call.index.is_none() && call.target.is_none() && call.subject.is_none())?;
            let (index, channel) = selection.ok_or(STALE)?;
            let current = server(
                servers,
                catalog,
                index,
                &proof.server,
                true,
                call.method == "send_text",
            )?;
            let channel = current.channels.get(channel).ok_or(STALE)?;
            let selected = proof.selected.ok_or(STALE)?;
            require(
                selected.channel == channel.id
                    && counter(&selected.instance) == Some(channel.instance),
            )?;
            match channel.target {
                TextTarget::Direct(session) => {
                    let expected = selected.peer.ok_or(STALE)?;
                    let captured = channel.direct_peer.as_ref().ok_or(STALE)?;
                    require(
                        expected.session == session
                            && captured.session == session
                            && counter(&expected.instance) == Some(captured.instance),
                    )?;
                    if call.method == "send_text" {
                        require(current.users.get(&session).is_some_and(|user| {
                            user.instance == captured.instance && user.instance != 0
                        }))?;
                    }
                }
                TextTarget::Channel(target) => {
                    require(target == channel.id && selected.peer.is_none())?
                }
                _ if call.method == "older" => require(selected.peer.is_none())?,
                _ => return Err(STALE.into()),
            }
        }
        "join" | "open" | "open_direct" => {
            require(call.subject.is_none())?;
            let index = call.index.ok_or(STALE)?;
            let id = call.target.ok_or(STALE)?;
            let current = server(
                servers,
                catalog,
                index,
                &proof.server,
                true,
                call.method == "join",
            )?;
            match (call.method.as_str(), proof.target.ok_or(STALE)?) {
                (
                    "join" | "open",
                    Target::Channel {
                        id: expected,
                        instance,
                        peer,
                    },
                ) => {
                    let channel = current
                        .channels
                        .iter()
                        .find(|channel| channel.id == id)
                        .ok_or(STALE)?;
                    require(expected == id && counter(&instance) == Some(channel.instance))?;
                    match channel.target {
                        TextTarget::Channel(target) => require(target == id && peer.is_none())?,
                        TextTarget::Direct(session) if call.method == "open" => {
                            let expected = peer.ok_or(STALE)?;
                            let captured = channel.direct_peer.as_ref().ok_or(STALE)?;
                            require(
                                expected.session == session
                                    && captured.session == session
                                    && counter(&expected.instance) == Some(captured.instance),
                            )?;
                        }
                        _ => return Err(STALE.into()),
                    }
                }
                (
                    "open_direct",
                    Target::User {
                        id: expected,
                        instance,
                    },
                ) => {
                    let user = current.users.get(&id).ok_or(STALE)?;
                    require(
                        expected == id
                            && user.instance != 0
                            && counter(&instance) == Some(user.instance),
                    )?;
                }
                _ => return Err(STALE.into()),
            }
        }
        "toggle_mute" | "toggle_deaf" => {
            require(call.target.is_none() && call.subject.is_none())?;
            let current = server(
                servers,
                catalog,
                call.index.ok_or(STALE)?,
                &proof.server,
                true,
                true,
            )?;
            let expected = proof.voice.ok_or(STALE)?;
            let voice = voice.ok_or(STALE)?;
            require(
                voice.server_id == current.id
                    && voice.connection_generation == current.generation.to_string()
                    && voice.joined
                    && current.muted == proof.server.muted
                    && current.deafened == proof.server.deafened
                    && voice.server_id == expected.server_id
                    && voice.connection_generation == expected.connection_generation
                    && voice.voice_generation == expected.voice_generation
                    && voice.channel_id == expected.channel_id
                    && voice.muted == expected.muted
                    && voice.deafened == expected.deafened,
            )?;
        }
        "accept_certificate" => {
            require(call.target.is_none())?;
            let current = server(
                servers,
                catalog,
                call.index.ok_or(STALE)?,
                &proof.server,
                true,
                false,
            )?;
            let subject = call.subject.ok_or(STALE)?;
            require(
                subject.len() == 64
                    && current
                        .certificate
                        .as_ref()
                        .is_some_and(|challenge| challenge.sha256 == subject),
            )?;
        }
        "disconnect"
        | "browse_server"
        | "connect"
        | "connect_with_password"
        | "edit_server"
        | "remove_server"
        | "dismiss_notice"
        | "dismiss_welcome" => {
            require(call.target.is_none() && call.subject.is_none())?;
            server(
                servers,
                catalog,
                call.index.ok_or(STALE)?,
                &proof.server,
                call.method == "disconnect",
                false,
            )?;
        }
        _ => return Err("Unsupported browser action proof.".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschl_client::{conversation::ConversationState, model::User};
    use serde_json::{Value, json};
    fn model() -> (Vec<ServerState>, Catalog) {
        let mut catalog = Catalog::default();
        let mut servers = Vec::new();
        for index in 0..2 {
            let mut server = ServerState::new(
                crate::snapshot::bookmark(
                    "owned",
                    "127.0.0.1",
                    64738,
                    &format!("fixture{index}"),
                    None,
                )
                .unwrap(),
            );
            server.generation = 5 + index as u64;
            server.status = Status::Connected;
            server.own_session = Some(1);
            server.muted = false;
            server.deafened = false;
            server.users.insert(
                1,
                User {
                    session: 1,
                    instance: 21,
                    ..User::default()
                },
            );
            server.users.insert(
                2,
                User {
                    session: 2,
                    instance: 22,
                    ..User::default()
                },
            );
            server.channels[0].instance = 11;
            server.channels.push(ConversationState {
                instance: 12,
                ..ConversationState::channel(7, Some(0), "child".into(), 0)
            });
            let stamp = catalog.reserve(servers.len(), None).unwrap();
            catalog.commit(servers.len(), stamp);
            servers.push(server);
        }
        (servers, catalog)
    }
    fn proof(servers: &[ServerState], catalog: &Catalog) -> Value {
        json!({"server":ServerRef::capture(&servers[0],catalog.get(0).unwrap()),"selected":{"channel":0,"instance":"11","peer":null},"target":{"kind":"channel","id":7,"instance":"12","peer":null}})
    }
    fn guard(
        servers: &[ServerState],
        catalog: &Catalog,
        selection: Option<(usize, usize)>,
        call: Value,
        proof: &Value,
    ) -> bool {
        validate(
            servers,
            catalog,
            selection,
            None,
            &call.to_string(),
            &proof.to_string(),
        )
        .is_ok()
    }
    #[test]
    fn implicit_selection_and_presence_cannot_retarget_but_unrelated_changes_are_harmless() {
        let (mut servers, catalog) = model();
        let p = proof(&servers, &catalog);
        let call = json!({"method":"send_text"});
        assert!(guard(&servers, &catalog, Some((0, 0)), call.clone(), &p));
        assert!(!guard(&servers, &catalog, Some((0, 1)), call.clone(), &p));
        servers[0].channels[1].name = "renamed other".into();
        servers[0].users.get_mut(&2).unwrap().channel = 7;
        servers[0].users.get_mut(&2).unwrap().name = "renamed".into();
        assert!(guard(&servers, &catalog, Some((0, 0)), call.clone(), &p));
        servers[0].users.get_mut(&1).unwrap().instance += 1;
        assert!(!guard(&servers, &catalog, Some((0, 0)), call, &p));
        servers[0].users.clear();
        servers[0].status = Status::Disconnected;
        assert!(
            guard(
                &servers,
                &catalog,
                Some((0, 0)),
                json!({"method":"older"}),
                &p
            ),
            "offline history does not need a live own presence"
        );
    }
    #[test]
    fn numeric_channel_and_user_commands_require_complete_exact_lifetimes() {
        let (mut servers, catalog) = model();
        let mut p = proof(&servers, &catalog);
        for method in ["join", "open"] {
            assert!(guard(
                &servers,
                &catalog,
                None,
                json!({"method":method,"index":0,"target":7}),
                &p
            ));
        }
        let incomplete = json!({"server":p["server"]});
        assert!(!guard(
            &servers,
            &catalog,
            None,
            json!({"method":"join","index":0,"target":7}),
            &incomplete
        ));
        assert!(!guard(
            &servers,
            &catalog,
            None,
            json!({"method":"join","index":0,"target":0}),
            &p
        ));
        servers[0].channels[1].instance += 1;
        assert!(!guard(
            &servers,
            &catalog,
            None,
            json!({"method":"join","index":0,"target":7}),
            &p
        ));
        p["target"] = json!({"kind":"user","id":2,"instance":"22"});
        assert!(guard(
            &servers,
            &catalog,
            None,
            json!({"method":"open_direct","index":0,"target":2}),
            &p
        ));
        servers[0].users.get_mut(&2).unwrap().instance += 1;
        assert!(!guard(
            &servers,
            &catalog,
            None,
            json!({"method":"open_direct","index":0,"target":2}),
            &p
        ));
    }
    #[test]
    fn catalog_removal_offline_same_id_readd_and_reconnect_never_reuse_original_authority() {
        let (mut servers, mut catalog) = model();
        let p = proof(&servers, &catalog);
        servers[0].generation += 1;
        assert!(!guard(
            &servers,
            &catalog,
            None,
            json!({"method":"disconnect","index":0}),
            &p
        ));
        for method in [
            "browse_server",
            "connect",
            "connect_with_password",
            "edit_server",
            "remove_server",
            "dismiss_notice",
            "dismiss_welcome",
        ] {
            assert!(
                guard(
                    &servers,
                    &catalog,
                    None,
                    json!({"method":method,"index":0}),
                    &p
                ),
                "local same-bookmark intent is not tied to an old connection"
            );
        }
        let old = servers.remove(0);
        catalog.remove(0, 2).unwrap();
        assert!(!guard(
            &servers,
            &catalog,
            None,
            json!({"method":"edit_server","index":0}),
            &p
        ));
        let new = ServerState::new(old.bookmark);
        let stamp = catalog.reserve(servers.len(), None).unwrap();
        catalog.commit(servers.len(), stamp);
        servers.push(new);
        assert!(
            !guard(
                &servers,
                &catalog,
                None,
                json!({"method":"connect","index":1}),
                &p
            ),
            "same offline ID must not reuse an entry lifetime"
        );
    }
    #[test]
    fn direct_selected_peer_reuse_is_rejected_while_old_peer_history_still_works_offline() {
        let (mut servers, catalog) = model();
        let peer = servers[0].users[&2].clone();
        servers[0].channels.push(ConversationState {
            id: u32::MAX,
            target: TextTarget::Direct(2),
            direct_peer: Some(peer),
            ..ConversationState::default()
        });
        let mut p = proof(&servers, &catalog);
        p["selected"] =
            json!({"channel":u32::MAX,"instance":"0","peer":{"session":2,"instance":"22"}});
        assert!(guard(
            &servers,
            &catalog,
            Some((0, 2)),
            json!({"method":"send_text"}),
            &p
        ));
        servers[0].users.get_mut(&2).unwrap().instance += 1;
        assert!(!guard(
            &servers,
            &catalog,
            Some((0, 2)),
            json!({"method":"send_text"}),
            &p
        ));
        assert!(guard(
            &servers,
            &catalog,
            Some((0, 2)),
            json!({"method":"older"}),
            &p
        ));
    }
    #[test]
    fn stale_toggle_cannot_invert_new_privacy_state_and_certificate_choice_stays_original() {
        let (mut servers, catalog) = model();
        let mut p = proof(&servers, &catalog);
        let voice = crate::voice::VoiceView {
            server_id: servers[0].id.clone(),
            connection_generation: "5".into(),
            voice_generation: "9".into(),
            channel_id: 0,
            joined: true,
            muted: false,
            deafened: false,
            can_transmit: false,
            bitrate: 128000,
        };
        p["voice"] = json!({"server_id":voice.server_id,"connection_generation":"5","voice_generation":"9","channel_id":0,"muted":false,"deafened":false});
        for method in ["toggle_mute", "toggle_deaf"] {
            assert!(
                validate(
                    &servers,
                    &catalog,
                    None,
                    Some(&voice),
                    &json!({"method":method,"index":0}).to_string(),
                    &p.to_string()
                )
                .is_ok()
            );
            servers[0].muted = true;
            assert!(
                validate(
                    &servers,
                    &catalog,
                    None,
                    Some(&voice),
                    &json!({"method":method,"index":0}).to_string(),
                    &p.to_string()
                )
                .is_err()
            );
            servers[0].muted = false;
        }
        let fingerprint = "aa".repeat(32);
        servers[0].certificate = Some(nuschl_client::server::CertificatePrompt {
            sha256: fingerprint.clone(),
            changed: false,
            reason: String::new(),
        });
        let call = json!({"method":"accept_certificate","index":0,"subject":fingerprint});
        assert!(guard(&servers, &catalog, None, call.clone(), &p));
        servers[0].certificate.as_mut().unwrap().sha256 = "bb".repeat(32);
        assert!(!guard(&servers, &catalog, None, call, &p));
    }
    #[test]
    fn catalog_bound_replacement_alignment_and_exhaustion_are_checked_before_mutation() {
        let mut catalog = Catalog::default();
        for index in 0..MAX_SERVERS {
            let stamp = catalog.reserve(index, None).unwrap();
            catalog.commit(index, stamp);
        }
        let before = catalog.entries.clone();
        assert!(catalog.reserve(MAX_SERVERS, None).is_err());
        assert_eq!(catalog.entries, before);
        let stamp = catalog.reserve(MAX_SERVERS, Some(5)).unwrap();
        catalog.commit(5, stamp);
        assert_ne!(catalog.entries[5], before[5]);
        catalog.remove(2, MAX_SERVERS).unwrap();
        assert_eq!(catalog.entries[2], before[3]);
        let stamp = catalog.reserve(MAX_SERVERS - 1, None).unwrap();
        catalog.commit(MAX_SERVERS - 1, stamp);
        assert!(stamp > MAX_SERVERS as u64);
        catalog.next = u64::MAX;
        let before = catalog.entries.clone();
        assert!(catalog.reserve(MAX_SERVERS, Some(0)).is_err());
        assert_eq!(catalog.entries, before);
    }
    #[test]
    fn unknown_methods_malformed_counters_and_uncovered_argument_shapes_fail_closed() {
        let (servers, catalog) = model();
        let mut p = proof(&servers, &catalog);
        for method in [
            "voice_packet",
            "content_complete",
            "image_send",
            "new_unreviewed_action",
        ] {
            assert!(!guard(
                &servers,
                &catalog,
                Some((0, 0)),
                json!({"method":method}),
                &p
            ));
        }
        assert!(!guard(
            &servers,
            &catalog,
            Some((0, 0)),
            json!({"method":"send_text","index":0}),
            &p
        ));
        p["server"]["catalog_instance"] = json!("01");
        assert!(!guard(
            &servers,
            &catalog,
            None,
            json!({"method":"browse_server","index":0}),
            &p
        ));
        p["server"]["catalog_instance"] = json!("nonsense");
        assert!(!guard(
            &servers,
            &Catalog::default(),
            None,
            json!({"method":"browse_server","index":0}),
            &p
        ));
        assert!(validate(&servers, &catalog, None, None, &"x".repeat(513), "{}").is_err());
        assert!(validate(&servers, &catalog, None, None, "{}", &"x".repeat(4097)).is_err());
    }
}
