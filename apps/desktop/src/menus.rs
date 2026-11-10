// SPDX-License-Identifier: EUPL-1.2
//! Context menus for users, channels and servers, and the text prompt dialog.
//! Entries the server would refuse are hidden using its reported permissions.
use crate::{
    assets::icon,
    dialogs::{Tone, modal, styled_button},
    state::{Desktop, Panel, Status},
    theme::*,
};
use gpui_kit::{
    base::input::{Input, InputState},
    prelude::*,
    *,
};
use nuschel_session::{
    connection::{Command, TextTarget},
    model::{ChannelEdit, UserAction, VoiceTargetSpec, permission as perm},
};
use std::rc::Rc;

/// Voice target ids registered with the server (1–30 are available).
pub const WHISPER_TARGET: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuTarget {
    User(u32),
    Channel(u32),
    Server,
}

pub struct Menu {
    pub server: usize,
    pub target: MenuTarget,
    pub position: Point<Pixels>,
}

#[derive(Clone, Debug)]
pub enum PromptAction {
    CreateChannel { parent: u32 },
    Kick { session: u32 },
    Ban { session: u32 },
    Tokens,
    Comment,
    ImportCertificate { path: std::path::PathBuf },
    ExportCertificate,
}

pub struct Prompt {
    pub server: usize,
    pub title: String,
    pub detail: String,
    pub confirm: &'static str,
    pub danger: bool,
    pub input: Entity<InputState>,
    pub action: PromptAction,
}

type Action = Rc<dyn Fn(&mut Desktop, &mut Window, &mut Context<Desktop>)>;

enum Entry {
    Item {
        label: String,
        icon: &'static str,
        danger: bool,
        action: Action,
    },
    Volume {
        hash: String,
        volume: f32,
    },
    Separator,
}

fn item(
    label: impl Into<String>,
    icon: &'static str,
    action: impl Fn(&mut Desktop, &mut Window, &mut Context<Desktop>) + 'static,
) -> Entry {
    Entry::Item {
        label: label.into(),
        icon,
        danger: false,
        action: Rc::new(action),
    }
}

fn danger(
    label: impl Into<String>,
    icon: &'static str,
    action: impl Fn(&mut Desktop, &mut Window, &mut Context<Desktop>) + 'static,
) -> Entry {
    Entry::Item {
        label: label.into(),
        icon,
        danger: true,
        action: Rc::new(action),
    }
}

impl Desktop {
    pub fn open_menu(&mut self, server: usize, target: MenuTarget, position: Point<Pixels>) {
        let Some(view) = self.servers.get(server) else {
            return;
        };
        if view.status == Status::Connected {
            // Ask for the permissions this menu depends on; entries appear as they arrive.
            let channel = match target {
                MenuTarget::Channel(channel) => Some(channel),
                MenuTarget::User(session) => view.users.get(&session).map(|user| user.channel),
                MenuTarget::Server => None,
            };
            for channel in channel.into_iter().chain([0]) {
                if !view.permissions.contains_key(&channel) {
                    let _ = self
                        .backend
                        .command(&view.id, Command::RequestPermissions(channel));
                }
            }
        }
        self.menu = Some(Menu {
            server,
            target,
            position,
        });
    }

    pub fn run(&mut self, server: usize, command: Command) {
        if let Some(view) = self.servers.get_mut(server)
            && let Err(error) = self.backend.command(&view.id, command)
        {
            view.notice = Some(error);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_prompt(
        &mut self,
        server: usize,
        action: PromptAction,
        title: impl Into<String>,
        detail: impl Into<String>,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (confirm, danger) = match action {
            PromptAction::CreateChannel { .. } => ("Create", false),
            PromptAction::Kick { .. } => ("Kick", true),
            PromptAction::Ban { .. } => ("Ban", true),
            PromptAction::Tokens | PromptAction::Comment => ("Save", false),
            PromptAction::ImportCertificate { .. } => ("Import", true),
            PromptAction::ExportCertificate => ("Choose file…", false),
        };
        let secret = matches!(
            action,
            PromptAction::ImportCertificate { .. } | PromptAction::ExportCertificate
        );
        let title: String = title.into();
        let label = title.clone();
        let input = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx)
                    .default_value(initial)
                    .masked(secret),
                &label,
                false,
                secret,
            )
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        self.prompt = Some(Prompt {
            server,
            title,
            detail: detail.into(),
            confirm,
            danger,
            input,
            action,
        });
    }

    pub fn confirm_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let text = prompt.input.read(cx).value().to_string();
        let reason = (!text.trim().is_empty()).then(|| text.trim().to_string());
        let command = match prompt.action {
            PromptAction::ImportCertificate { path } => {
                let result = std::fs::metadata(&path)
                    .map_err(|error| error.to_string())
                    .and_then(|metadata| {
                        if metadata.len() > nuschel_session::config::MAX_PKCS12_BYTES as u64 {
                            Err("The certificate file is too large.".to_string())
                        } else {
                            std::fs::read(&path).map_err(|error| error.to_string())
                        }
                    })
                    .and_then(|bytes| self.profile.import_identity(&bytes, &text));
                self.identity_status = Some(match result {
                    Ok(identity) => format!(
                        "Imported certificate {}. Reconnect to servers to use it.",
                        identity.mumble_hash()
                    ),
                    Err(error) => error,
                });
                return;
            }
            PromptAction::ExportCertificate => {
                let directory = std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir);
                let target = cx.prompt_for_new_path(&directory, Some("nuschel-certificate.p12"));
                let password = text;
                cx.spawn(async move |this, cx| {
                    let Ok(Ok(Some(path))) = target.await else {
                        return;
                    };
                    let _ = this.update(cx, |this, cx| {
                        let result = this.profile.export_identity(&password).and_then(|bytes| write_private(&path, &bytes).map_err(|error| error.to_string()));
                        this.identity_status = Some(match result {
                            Ok(()) => format!("Exported to {}. Import it in Mumble via Configure → Certificate Wizard.", path.display()),
                            Err(error) => format!("Export failed: {error}"),
                        });
                        cx.notify();
                    });
                })
                .detach();
                return;
            }
            PromptAction::CreateChannel { parent } => {
                if text.trim().is_empty() {
                    return;
                }
                let temporary = self
                    .servers
                    .get(prompt.server)
                    .is_some_and(|server| !server.may(parent, perm::MAKE_CHANNEL));
                Command::CreateChannel {
                    parent,
                    name: text.trim().to_string(),
                    temporary,
                }
            }
            PromptAction::Kick { session } => Command::User {
                session,
                action: UserAction::Kick,
                reason,
            },
            PromptAction::Ban { session } => Command::User {
                session,
                action: UserAction::Ban,
                reason,
            },
            PromptAction::Comment => Command::SetComment(text),
            PromptAction::Tokens => {
                let tokens: Vec<String> = text
                    .split([',', '\n'])
                    .map(|token| token.trim().to_string())
                    .filter(|token| !token.is_empty())
                    .take(64)
                    .collect();
                if let Some(server) = self.servers.get_mut(prompt.server) {
                    server.bookmark.tokens = tokens.clone();
                }
                let bookmarks: Vec<_> = self
                    .servers
                    .iter()
                    .map(|server| server.bookmark.clone())
                    .collect();
                if let Err(error) = self.profile.save_servers(&bookmarks) {
                    self.error = Some(error.to_string());
                }
                Command::SetTokens(tokens)
            }
        };
        self.run(prompt.server, command);
    }

    pub fn import_certificate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import certificate".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.open_prompt(
                    usize::MAX,
                    PromptAction::ImportCertificate { path },
                    "Certificate password",
                    "The password chosen when the certificate was exported (often empty). Your current certificate is kept as a backup.",
                    String::new(),
                    window,
                    cx,
                );
                cx.notify();
            });
        })
        .detach();
    }

    pub fn whisper(&mut self, server: usize, target: VoiceTargetSpec, label: String) {
        self.run(
            server,
            Command::SetVoiceTarget {
                id: WHISPER_TARGET,
                targets: vec![target],
            },
        );
        self.run(server, Command::SelectVoiceTarget(WHISPER_TARGET));
        if let Some(view) = self.servers.get_mut(server) {
            view.whisper = Some(label);
        }
    }

    pub fn stop_whisper(&mut self, server: usize) {
        self.run(server, Command::SelectVoiceTarget(0));
        if let Some(view) = self.servers.get_mut(server) {
            view.whisper = None;
        }
    }

    fn menu_entries(&self, menu: &Menu) -> Vec<Entry> {
        let index = menu.server;
        let Some(server) = self.servers.get(index) else {
            return Vec::new();
        };
        let connected = server.status == Status::Connected;
        let own = server.own_session;
        let my_channel = server.voice_channel();
        let mut entries = Vec::new();
        match menu.target {
            MenuTarget::User(session) => {
                let Some(user) = server.users.get(&session).cloned() else {
                    return entries;
                };
                let name = user.name.clone();
                let mine = own == Some(session);
                let channel = user.channel;
                entries.push(item("Profile and connection", "info", move |this, _, _| {
                    this.run(index, Command::RequestStats(session));
                    this.render_comment(index, session);
                    this.panel = Some(Panel::UserInfo {
                        server: index,
                        session,
                    });
                }));
                if mine {
                    entries.push(item("Edit comment…", "pencil", move |this, window, cx| {
                        let current = this.servers[index].users.get(&session).and_then(|user| user.comment.clone()).unwrap_or_default();
                        this.open_prompt(index, PromptAction::Comment, "Your comment", "Shown to others in your profile. Plain text; line breaks are kept.", strip_tags(&current), window, cx);
                    }));
                    if user.user_id.is_none() && server.may_try(0, perm::SELF_REGISTER) {
                        entries.push(item(
                            "Register this name",
                            "certificate",
                            move |this, _, _| this.run(index, Command::RegisterSelf),
                        ));
                    }
                } else {
                    entries.push(item(
                        "Send direct message",
                        "message",
                        move |this, window, cx| this.open_direct(index, session, window, cx),
                    ));
                    if let Some(hash) = user.certificate_hash.clone() {
                        entries.push(Entry::Volume {
                            hash: hash.clone(),
                            volume: self.settings.volume_for(Some(&hash)),
                        });
                        let muted = self.settings.locally_muted(Some(&hash));
                        entries.push(item(
                            if muted {
                                "Unmute for me"
                            } else {
                                "Mute for me"
                            },
                            if muted { "volume" } else { "mic-off" },
                            move |this, _, _| {
                                let hash = hash.clone();
                                this.update_settings(|settings| {
                                    if !settings.local_mutes.remove(&hash) {
                                        settings.local_mutes.insert(hash);
                                    }
                                });
                            },
                        ));
                    }
                    if server.may_try(channel, perm::WHISPER) {
                        let label = format!("Whispering to {name}");
                        entries.push(item(
                            format!("Whisper to {name}"),
                            "whisper",
                            move |this, _, _| {
                                this.whisper(
                                    index,
                                    VoiceTargetSpec::Users(vec![session]),
                                    label.clone(),
                                );
                            },
                        ));
                    }
                    if let Some(mine) = my_channel.filter(|mine| *mine != channel)
                        && server.may(mine, perm::MOVE)
                    {
                        entries.push(item("Move to my channel", "volume", move |this, _, _| {
                            this.run(
                                index,
                                Command::User {
                                    session,
                                    action: UserAction::Move(mine),
                                    reason: None,
                                },
                            );
                        }));
                    }
                }
                let moderate = server.may(channel, perm::MUTE_DEAFEN);
                let root = |bits| server.may(0, bits);
                if moderate
                    || root(perm::KICK)
                    || root(perm::BAN)
                    || root(perm::REGISTER)
                    || root(perm::RESET_USER_CONTENT)
                {
                    entries.push(Entry::Separator);
                }
                if moderate {
                    let (muted, deafened, priority) =
                        (user.muted, user.deafened, user.priority_speaker);
                    entries.push(item(
                        if muted {
                            "Server unmute"
                        } else {
                            "Server mute"
                        },
                        "mic-off",
                        move |this, _, _| {
                            this.run(
                                index,
                                Command::User {
                                    session,
                                    action: UserAction::Mute(!muted),
                                    reason: None,
                                },
                            );
                        },
                    ));
                    entries.push(item(
                        if deafened {
                            "Server undeafen"
                        } else {
                            "Server deafen"
                        },
                        "headphones-off",
                        move |this, _, _| {
                            this.run(
                                index,
                                Command::User {
                                    session,
                                    action: UserAction::Deafen(!deafened),
                                    reason: None,
                                },
                            );
                        },
                    ));
                    entries.push(item(
                        if priority {
                            "Remove priority speaker"
                        } else {
                            "Make priority speaker"
                        },
                        "star",
                        move |this, _, _| {
                            this.run(
                                index,
                                Command::User {
                                    session,
                                    action: UserAction::PrioritySpeaker(!priority),
                                    reason: None,
                                },
                            );
                        },
                    ));
                }
                if !mine && user.user_id.is_none() && root(perm::REGISTER) {
                    entries.push(item(
                        format!("Register {name}"),
                        "certificate",
                        move |this, _, _| {
                            this.run(
                                index,
                                Command::User {
                                    session,
                                    action: UserAction::Register,
                                    reason: None,
                                },
                            );
                        },
                    ));
                }
                if root(perm::RESET_USER_CONTENT) {
                    entries.push(item("Reset comment", "pencil", move |this, _, _| {
                        this.run(
                            index,
                            Command::User {
                                session,
                                action: UserAction::ResetComment,
                                reason: None,
                            },
                        );
                    }));
                    entries.push(item("Reset avatar", "user", move |this, _, _| {
                        this.run(
                            index,
                            Command::User {
                                session,
                                action: UserAction::ResetAvatar,
                                reason: None,
                            },
                        );
                    }));
                }
                if !mine && root(perm::KICK) {
                    let title = format!("Kick {name}");
                    entries.push(danger("Kick…", "hang-up", move |this, window, cx| {
                        this.open_prompt(
                            index,
                            PromptAction::Kick { session },
                            title.clone(),
                            "Optional reason, shown to the user.",
                            String::new(),
                            window,
                            cx,
                        );
                    }));
                }
                if !mine && root(perm::BAN) {
                    let title = format!("Ban {name}");
                    entries.push(danger("Ban…", "shield", move |this, window, cx| {
                        this.open_prompt(
                            index,
                            PromptAction::Ban { session },
                            title.clone(),
                            "Bans this certificate and address. Optional reason.",
                            String::new(),
                            window,
                            cx,
                        );
                    }));
                }
                push_context_actions(
                    &mut entries,
                    server,
                    index,
                    |action| action.user,
                    Some(session),
                    None,
                );
            }
            MenuTarget::Channel(channel) => {
                let Some(view) = server
                    .channels
                    .iter()
                    .find(|entry| entry.target == TextTarget::Channel(channel))
                else {
                    return entries;
                };
                let links = view.links.clone();
                let listening = own
                    .and_then(|own| server.users.get(&own))
                    .is_some_and(|user| user.listening.contains(&channel));
                if connected && my_channel != Some(channel) && server.may_try(channel, perm::ENTER)
                {
                    entries.push(item("Join channel", "volume", move |this, _, _| {
                        this.run(index, Command::Join(channel))
                    }));
                }
                if connected && my_channel != Some(channel) && server.may_try(channel, perm::LISTEN)
                {
                    entries.push(item(
                        if listening {
                            "Stop listening"
                        } else {
                            "Listen to channel"
                        },
                        "ear",
                        move |this, _, _| {
                            this.run(
                                index,
                                Command::Listen {
                                    channel,
                                    listen: !listening,
                                },
                            );
                        },
                    ));
                }
                if connected && server.may_try(channel, perm::WHISPER) {
                    let label = format!("Shouting to #{}", view.name);
                    entries.push(item("Shout to channel", "whisper", move |this, _, _| {
                        this.whisper(
                            index,
                            VoiceTargetSpec::Channel {
                                channel,
                                links: false,
                                children: false,
                                group: None,
                            },
                            label.clone(),
                        );
                    }));
                    let label = format!("Shouting to #{} and sub-channels", view.name);
                    entries.push(item(
                        "Shout to channel and sub-channels",
                        "whisper",
                        move |this, _, _| {
                            this.whisper(
                                index,
                                VoiceTargetSpec::Channel {
                                    channel,
                                    links: false,
                                    children: true,
                                    group: None,
                                },
                                label.clone(),
                            );
                        },
                    ));
                }
                if let Some(mine) = my_channel.filter(|mine| *mine != channel)
                    && server.may(channel, perm::LINK_CHANNEL)
                {
                    let linked = links.contains(&mine);
                    entries.push(item(
                        if linked {
                            "Unlink from my channel"
                        } else {
                            "Link with my channel"
                        },
                        "link",
                        move |this, _, _| {
                            let (add, remove) = if linked {
                                (vec![], vec![mine])
                            } else {
                                (vec![mine], vec![])
                            };
                            this.run(
                                index,
                                Command::LinkChannels {
                                    channel,
                                    add,
                                    remove,
                                },
                            );
                        },
                    ));
                }
                if !links.is_empty() && server.may(channel, perm::LINK_CHANNEL) {
                    entries.push(item("Unlink all", "link", move |this, _, _| {
                        this.run(
                            index,
                            Command::LinkChannels {
                                channel,
                                add: vec![],
                                remove: links.iter().copied().collect(),
                            },
                        );
                    }));
                }
                entries.push(item("Copy link", "link", move |this, _, cx| {
                    if let Some(url) = this.channel_url(index, channel) {
                        cx.write_to_clipboard(ClipboardItem::new_string(url));
                    }
                }));
                let write = server.may(channel, perm::WRITE);
                let create = server.may(channel, perm::MAKE_CHANNEL)
                    || server.may(channel, perm::MAKE_TEMP_CHANNEL);
                if connected && (write || create) {
                    entries.push(Entry::Separator);
                }
                if connected && create {
                    entries.push(item("Create sub-channel…", "plus", move |this, window, cx| {
                        let temporary = !this.servers[index].may(channel, perm::MAKE_CHANNEL);
                        this.open_prompt(
                            index,
                            PromptAction::CreateChannel { parent: channel },
                            "Create a channel",
                            if temporary { "You may create temporary channels here; they disappear when empty." } else { "Name of the new channel." },
                            String::new(),
                            window,
                            cx,
                        );
                    }));
                }
                if connected && write {
                    entries.push(item(
                        "Edit channel…",
                        "pencil",
                        move |this, window, cx| {
                            this.open_channel_editor(index, channel, window, cx)
                        },
                    ));
                    entries.push(item("Edit permissions…", "shield", move |this, _, _| {
                        this.servers[index].acl = None;
                        this.run(index, Command::QueryAcl(channel));
                        this.panel = Some(Panel::Acl {
                            server: index,
                            channel,
                        });
                    }));
                    if channel != 0 {
                        entries.push(danger(
                            "Remove channel",
                            "window-close",
                            move |this, _, _| this.run(index, Command::RemoveChannel(channel)),
                        ));
                    }
                }
                push_context_actions(
                    &mut entries,
                    server,
                    index,
                    |action| action.channel,
                    None,
                    Some(channel),
                );
            }
            MenuTarget::Server => {
                entries.push(item("Connection info", "signal", move |this, _, _| {
                    this.panel = Some(Panel::Connection { server: index })
                }));
                if connected {
                    let registered = own
                        .and_then(|own| server.users.get(&own))
                        .is_some_and(|user| user.user_id.is_some());
                    if !registered && server.may_try(0, perm::SELF_REGISTER) {
                        entries.push(item(
                            "Register your name",
                            "certificate",
                            move |this, _, _| this.run(index, Command::RegisterSelf),
                        ));
                    }
                    let tokens = server.bookmark.tokens.join(", ");
                    entries.push(item("Access tokens…", "keyboard", move |this, window, cx| {
                        this.open_prompt(index, PromptAction::Tokens, "Access tokens", "Comma-separated. Tokens grant membership in password-protected groups and are saved with this server.", tokens.clone(), window, cx);
                    }));
                    if server.may(0, perm::REGISTER) {
                        entries.push(item("Registered users…", "user", move |this, _, _| {
                            this.servers[index].registered_users = None;
                            this.run(index, Command::QueryRegisteredUsers);
                            this.panel = Some(Panel::RegisteredUsers { server: index });
                        }));
                    }
                    if server.may(0, perm::BAN) {
                        entries.push(item("Ban list…", "shield", move |this, _, _| {
                            this.servers[index].bans = None;
                            this.run(index, Command::QueryBans);
                            this.panel = Some(Panel::Bans { server: index });
                        }));
                    }
                }
                entries.push(item("Edit server…", "pencil", move |this, window, cx| {
                    this.open_form(Some(index), window, cx)
                }));
                if connected {
                    entries.push(danger("Disconnect", "hang-up", move |this, _, _| {
                        this.disconnect(index)
                    }));
                }
                push_context_actions(
                    &mut entries,
                    server,
                    index,
                    |action| action.server,
                    None,
                    None,
                );
            }
        }
        entries
    }

    pub fn channel_url(&self, server: usize, channel: u32) -> Option<String> {
        let server = self.servers.get(server)?;
        let mut path = Vec::new();
        let mut current = Some(channel);
        while let Some(id) = current.filter(|id| *id != 0) {
            let view = server
                .channels
                .iter()
                .find(|entry| entry.target == TextTarget::Channel(id))?;
            path.push(percent_encode(&view.name));
            current = view.parent;
            if path.len() > 64 {
                return None;
            }
        }
        path.reverse();
        let host = if server.bookmark.host.contains(':') {
            format!("[{}]", server.bookmark.host)
        } else {
            server.bookmark.host.clone()
        };
        Some(format!(
            "mumble://{host}:{}/{}",
            server.bookmark.port,
            path.join("/")
        ))
    }

    pub fn open_channel_editor(
        &mut self,
        server: usize,
        channel: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.servers.get(server).and_then(|server| {
            server
                .channels
                .iter()
                .find(|entry| entry.target == TextTarget::Channel(channel))
        }) else {
            return;
        };
        let name = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx).default_value(view.name.clone()),
                "Channel name",
                false,
                false,
            )
        });
        let description = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx).default_value(strip_tags(&view.description_raw)),
                "Channel description",
                false,
                false,
            )
        });
        let position = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx).default_value(view.position.to_string()),
                "Position",
                false,
                false,
            )
        });
        let max_users = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx).default_value(view.max_users.to_string()),
                "Maximum users",
                false,
                false,
            )
        });
        if !view.description_raw.is_empty() || view.description.is_none() {
            let id = self.servers[server].id.clone();
            let _ = self.backend.command(
                &id,
                Command::RequestBlobs {
                    textures: vec![],
                    comments: vec![],
                    descriptions: vec![channel],
                },
            );
        }
        self.panel = Some(Panel::ChannelEdit {
            server,
            channel,
            name,
            description,
            position,
            max_users,
        });
    }

    pub fn save_channel_editor(&mut self, cx: &mut Context<Self>) {
        let Some(Panel::ChannelEdit {
            server,
            channel,
            name,
            description,
            position,
            max_users,
        }) = self.panel.take()
        else {
            return;
        };
        let read = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let edit = ChannelEdit {
            name: Some(read(&name).trim().to_string()).filter(|name| !name.is_empty()),
            description: Some(read(&description)),
            position: read(&position).trim().parse().ok(),
            max_users: read(&max_users).trim().parse().ok(),
            parent: None,
        };
        self.run(server, Command::EditChannel { channel, edit });
    }

    pub fn render_comment(&mut self, server: usize, session: u32) {
        let Some(view) = self.servers.get(server) else {
            return;
        };
        let Some(user) = view.users.get(&session) else {
            return;
        };
        match (&user.comment, &user.comment_hash) {
            (Some(comment), _) => {
                if view
                    .comments
                    .get(&session)
                    .is_none_or(|(source, _)| source != comment)
                {
                    self.backend.render_html(
                        view.id.clone(),
                        view.generation,
                        crate::backend::RenderKey::Comment(session),
                        comment.clone(),
                    );
                }
            }
            (None, Some(_)) => {
                let _ = self.backend.command(
                    &view.id,
                    Command::RequestBlobs {
                        textures: vec![],
                        comments: vec![session],
                        descriptions: vec![],
                    },
                );
            }
            (None, None) => {}
        }
    }
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn push_context_actions(
    entries: &mut Vec<Entry>,
    server: &crate::state::ServerView,
    index: usize,
    applies: impl Fn(&nuschel_session::model::ContextAction) -> bool,
    session: Option<u32>,
    channel: Option<u32>,
) {
    let actions: Vec<_> = server
        .context_actions
        .iter()
        .filter(|action| applies(action))
        .cloned()
        .collect();
    if actions.is_empty() {
        return;
    }
    entries.push(Entry::Separator);
    for action in actions {
        let name = action.action.clone();
        entries.push(item(action.text.clone(), "more", move |this, _, _| {
            this.run(
                index,
                Command::ContextAction {
                    action: name.clone(),
                    session,
                    channel,
                },
            );
        }));
    }
}

/// Plain text from Mumble's HTML for editing fields. Display goes through the content worker.
pub fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let name = tag
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(name.trim_end_matches('/'), "br" | "p" | "div") {
                    out.push('\n');
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

fn percent_encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

pub fn menu(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let menu = this.menu.as_ref()?;
    let entries = this.menu_entries(menu);
    if entries.is_empty() {
        return None;
    }
    let rows = entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| match entry {
            Entry::Separator => div()
                .my_1()
                .h(px(1.))
                .bg(color(BORDER_STRONG))
                .into_any_element(),
            Entry::Item {
                label,
                icon: name,
                danger,
                action,
            } => {
                let key_action = action.clone();
                let foreground = if danger { DANGER_HOVER } else { TEXT };
                div()
                    .id(("menu-item", index))
                    .group(SharedString::from(format!("menu-item-{index}")))
                    .role(Role::MenuItem)
                    .aria_label(label.clone())
                    .tab_index(0)
                    .px_2()
                    .py(px(6.))
                    .rounded(px(5.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(color(foreground))
                    .hover(move |style| {
                        style
                            .bg(color(if danger { DANGER } else { ACCENT }))
                            .text_color(color(0xffffff))
                    })
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.menu = None;
                        action(this, window, cx);
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "enter" || event.keystroke.key == "space" {
                            this.menu = None;
                            key_action(this, window, cx);
                            cx.notify();
                        }
                    }))
                    .child(
                        icon(
                            name,
                            15.,
                            color(if danger { DANGER_HOVER } else { TEXT_MUTED }),
                        )
                        .group_hover(SharedString::from(format!("menu-item-{index}")), |style| {
                            style.text_color(color(0xffffff))
                        }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(text!(label).with_id(("menu-label", index))),
                    )
                    .into_any_element()
            }
            Entry::Volume { hash, volume } => {
                let (down, up) = (hash.clone(), hash);
                div()
                    .id(("menu-volume", index))
                    .role(Role::Slider)
                    .aria_label(format!("Volume {:.0}%", volume * 100.))
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(icon("volume", 15., color(TEXT_MUTED)))
                    .child(
                        div().flex_1().child(
                            text!(format!("Volume {:.0}%", volume * 100.))
                                .with_id(("volume-label", index)),
                        ),
                    )
                    .child(styled_button(
                        ("volume-down", index),
                        "−",
                        Tone::Secondary,
                        move |this, _, cx| {
                            let hash = down.clone();
                            this.update_settings(|settings| {
                                let value = settings.volume_for(Some(&hash)) - 0.1;
                                settings.user_volumes.insert(hash, value.max(0.));
                            });
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(styled_button(
                        ("volume-up", index),
                        "+",
                        Tone::Secondary,
                        move |this, _, cx| {
                            let hash = up.clone();
                            this.update_settings(|settings| {
                                let value = settings.volume_for(Some(&hash)) + 0.1;
                                settings.user_volumes.insert(hash, value.min(4.));
                            });
                            cx.notify();
                        },
                        cx,
                    ))
                    .into_any_element()
            }
        })
        .collect::<Vec<_>>();
    Some(
        div()
            .id("menu-layer")
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.menu = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.menu = None;
                    cx.notify();
                }),
            )
            .child(deferred(
                anchored()
                    .position(menu.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("context-menu")
                            .role(Role::Menu)
                            .min_w(px(220.))
                            .max_w(px(320.))
                            .p_1()
                            .rounded(px(8.))
                            .bg(color(APP))
                            .border_1()
                            .border_color(color(BORDER_STRONG))
                            .shadow_lg()
                            .flex()
                            .flex_col()
                            .children(rows),
                    ),
            ))
            .into_any_element(),
    )
}

pub fn prompt(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let prompt = this.prompt.as_ref()?;
    let content = div()
        .flex()
        .flex_col()
        .child(
            div()
                .p_6()
                .pb_4()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .child(text!(prompt.title.clone()).with_id("prompt-title")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(prompt.detail.clone()).with_id("prompt-detail")),
                )
                .child(
                    div()
                        .mt_2()
                        .px_3()
                        .py(px(9.))
                        .rounded(px(6.))
                        .bg(color(APP))
                        .border_1()
                        .border_color(color(BORDER))
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                            if event.keystroke.key == "enter" {
                                this.confirm_prompt(cx);
                                cx.stop_propagation();
                                cx.notify();
                            }
                        }))
                        .child(Input::new(&prompt.input)),
                ),
        )
        .child(
            div()
                .px_6()
                .py_4()
                .bg(color(SIDEBAR))
                .rounded_b(px(12.))
                .flex()
                .gap_3()
                .justify_end()
                .child(styled_button(
                    "prompt-cancel",
                    "Cancel",
                    Tone::Ghost,
                    |this, _, cx| {
                        this.prompt = None;
                        cx.notify();
                    },
                    cx,
                ))
                .child(styled_button(
                    "prompt-confirm",
                    prompt.confirm,
                    if prompt.danger {
                        Tone::Danger
                    } else {
                        Tone::Primary
                    },
                    |this, _, cx| {
                        this.confirm_prompt(cx);
                        cx.notify();
                    },
                    cx,
                )),
        )
        .into_any_element();
    Some(modal("prompt-dialog", "Input", 460., content, cx, |this| {
        this.prompt = None
    }))
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that glob includes GPUI's `test` macro, which shadows `#[test]`.
    use super::{percent_encode, strip_tags};
    #[test]
    fn strips_mumble_html_for_editing() {
        assert_eq!(
            strip_tags("<p>Hi <b>there</b><br/>line&amp;more</p>"),
            "Hi there\nline&more"
        );
    }
    #[test]
    fn encodes_url_path_segments() {
        assert_eq!(percent_encode("Dev Talk/ü"), "Dev%20Talk%2F%C3%BC");
    }
}
