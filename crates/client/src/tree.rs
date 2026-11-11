// SPDX-License-Identifier: EUPL-1.2
//! A server's channel tree as a list: channels in tree order, each followed
//! by its people and listeners; a closed branch counts who is inside.
use crate::{
    model::{TextTarget, User},
    server::ServerState,
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeRow {
    Channel(ChannelRow),
    User {
        session: u32,
        depth: usize,
    },
    /// Someone listening to (not in) this channel, Mumble 1.4+.
    Listener {
        session: u32,
        depth: usize,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelRow {
    /// Into `ServerState::channels`.
    pub index: usize,
    pub depth: usize,
    pub has_children: bool,
    pub expanded: bool,
    /// Users hidden inside a collapsed branch, shown as a count.
    pub hidden_users: usize,
    /// Someone in that collapsed branch is talking.
    pub hidden_talking: bool,
}
impl ServerState {
    /// Channels in tree order, each followed by its people and listeners;
    /// a closed branch counts the people inside it. One pass over channels
    /// and users, so it is cheap enough to run every frame.
    pub fn tree_rows(&self) -> Vec<TreeRow> {
        // One entry per channel id; direct conversations are listed apart.
        let mut index_of = HashMap::with_capacity(self.channels.len());
        for (index, channel) in self.channels.iter().enumerate() {
            if let TextTarget::Channel(id) = channel.target {
                index_of.entry(id).or_insert(index);
            }
        }
        // Families by parent (roots under `None`), each in display order.
        let mut children = index_of
            .values()
            .map(|&index| (self.channels[index].parent, index))
            .collect::<Vec<_>>();
        children.sort_unstable();
        let family = |parent| run(&children, parent, |&(parent, _)| parent);
        let mut people = self.users.values().collect::<Vec<_>>();
        people.sort_by_cached_key(|user| (user.channel, user.name.to_lowercase(), user.session));
        let mut listeners = self
            .users
            .values()
            .flat_map(|user| user.listening.iter().map(|&id| (id, user.session)))
            .collect::<Vec<_>>();
        listeners.sort_unstable();
        // The branch holding our voice channel starts open.
        let parent_of = |index: usize| {
            let parent = self.channels[index].parent?;
            index_of.get(&parent).copied()
        };
        let mut holds_voice = vec![false; self.channels.len()];
        let mut voice = self
            .voice_channel()
            .and_then(|id| index_of.get(&id).copied());
        while let Some(parent) = voice.and_then(parent_of)
            && !holds_voice[parent]
        {
            holds_voice[parent] = true;
            voice = Some(parent);
        }

        // Depth first through every branch, closed ones too: they count
        // their people. A child always follows its parent. With one entry
        // per id each channel is in one family, so a cycle is unreachable.
        struct Node<'a> {
            row: ChannelRow,
            parent: Option<usize>,
            visible: bool,
            people: &'a [&'a User],
        }
        let mut nodes = Vec::<Node>::with_capacity(index_of.len());
        let mut pending = family(None)
            .iter()
            .rev()
            .map(|&(_, index)| (index, 0, None, true))
            .collect::<Vec<_>>();
        while let Some((index, depth, parent, visible)) = pending.pop() {
            let channel = &self.channels[index];
            let expanded = self
                .expansion
                .get(&channel.id)
                .copied()
                .unwrap_or(channel.parent.is_none() || holds_voice[index]);
            let children = family(Some(channel.id));
            let slot = nodes.len();
            pending.extend(
                children
                    .iter()
                    .rev()
                    .map(|&(_, child)| (child, depth + 1, Some(slot), visible && expanded)),
            );
            nodes.push(Node {
                row: ChannelRow {
                    index,
                    depth,
                    has_children: !children.is_empty(),
                    expanded,
                    hidden_users: 0,
                    hidden_talking: false,
                },
                parent,
                visible,
                people: run(&people, channel.id, |user| user.channel),
            });
        }
        // Children before parents: everyone inside adds up towards the roots.
        for slot in (0..nodes.len()).rev() {
            let node = &nodes[slot];
            let Some(parent) = node.parent else {
                continue;
            };
            let inside = node.row.hidden_users + node.people.len();
            let talking = node.row.hidden_talking
                || node
                    .people
                    .iter()
                    .any(|user| self.talking.contains(&user.session));
            let parent = &mut nodes[parent].row;
            parent.hidden_users += inside;
            parent.hidden_talking |= talking;
        }

        let mut rows = Vec::new();
        for Node {
            mut row, people, ..
        } in nodes.into_iter().filter(|node| node.visible)
        {
            if row.expanded {
                (row.hidden_users, row.hidden_talking) = (0, false);
            }
            let id = self.channels[row.index].id;
            let depth = row.depth + 1;
            rows.push(TreeRow::Channel(row));
            rows.extend(people.iter().map(|user| TreeRow::User {
                session: user.session,
                depth,
            }));
            rows.extend(
                run(&listeners, id, |&(channel, _)| channel)
                    .iter()
                    .map(|&(_, session)| TreeRow::Listener { session, depth }),
            );
        }
        rows
    }
    /// Open or close a branch by hand; the choice outlasts the defaults.
    pub fn set_expanded(&mut self, id: u32, expanded: bool) {
        self.expansion.insert(id, expanded);
    }
    /// Branches where people are open on every connect; anyone who arrives
    /// later in a collapsed branch shows as a count on it.
    pub fn expand_occupied(&mut self) {
        let mut parents = HashMap::with_capacity(self.channels.len());
        for channel in &self.channels {
            if let TextTarget::Channel(id) = channel.target {
                parents.entry(id).or_insert(channel.parent);
            }
        }
        // Each channel's ancestors are walked once.
        let mut walked = HashSet::new();
        for user in self.users.values() {
            let mut current = user.channel;
            while walked.insert(current)
                && let Some(&Some(parent)) = parents.get(&current)
            {
                self.expansion.insert(parent, true);
                current = parent;
            }
        }
    }
    /// Channels in display order: by position, then name.
    pub fn sort_channels(&mut self) {
        self.channels.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.name.cmp(&b.name))
        });
    }
}

/// The run of `sorted` (ascending by `key`) whose key is `wanted`.
fn run<T, K: Ord>(sorted: &[T], wanted: K, key: impl Fn(&T) -> K) -> &[T] {
    let start = sorted.partition_point(|item| key(item) < wanted);
    let len = sorted[start..].partition_point(|item| key(item) == wanted);
    &sorted[start..start + len]
}

#[cfg(test)]
mod tests {
    use super::{ChannelRow, TreeRow};
    use crate::{
        conversation::ConversationState,
        model::{TextTarget, User},
        server::ServerState,
    };
    use nuschel_core::bookmark::ServerBookmark;

    fn channel(id: u32, parent: Option<u32>, name: &str, position: i32) -> ConversationState {
        ConversationState {
            id,
            parent,
            name: name.into(),
            position,
            target: TextTarget::Channel(id),
            storage_key: id.to_string(),
            ..Default::default()
        }
    }

    fn user(session: u32, name: &str, channel: u32) -> User {
        User {
            session,
            name: name.into(),
            channel,
            ..Default::default()
        }
    }

    /// Root holds AFK, Lobby (Development; Gaming with Shooters and Strategy)
    /// and Music (Listening party), which its position sorts first. We are in
    /// Development; Margaret talks in Listening party; bob listens to
    /// Development from AFK.
    fn sample() -> ServerState {
        let mut server =
            ServerState::new(ServerBookmark::new("Test", "localhost", 64738, "andi").unwrap());
        server.channels = vec![
            channel(0, None, "Root", 0),
            channel(1, Some(0), "AFK", 0),
            channel(2, Some(0), "Lobby", 0),
            channel(3, Some(2), "Development", 0),
            channel(4, Some(2), "Gaming", 0),
            channel(5, Some(4), "Shooters", 0),
            channel(6, Some(4), "Strategy", 0),
            channel(7, Some(0), "Music", -1),
            channel(8, Some(7), "Listening party", 0),
            // Its parent is unknown: never shown.
            channel(9, Some(98), "Orphan", 0),
        ];
        server.sort_channels();
        // A direct conversation shares the list but is not part of the tree.
        server.channels.push(ConversationState {
            id: u32::MAX,
            name: "@ Zed".into(),
            position: i32::MAX,
            target: TextTarget::Direct(7),
            ..Default::default()
        });
        let mut bob = user(6, "bob", 1);
        bob.listening.insert(3);
        for user in [
            user(1, "andi", 3),
            user(2, "Ada Lovelace", 3),
            user(3, "grace", 3),
            user(4, "Linus", 5),
            user(5, "Margaret", 8),
            bob,
            user(7, "Zed", 0),
            user(8, "Lost", 99),
            user(9, "Orphaned", 9),
            user(10, "ada lovelace", 3),
        ] {
            server.users.insert(user.session, user);
        }
        server.own_session = Some(1);
        server.talking.insert(5);
        server
    }

    /// One line per row: `v`/`>` for an open/closed branch, `@` a person,
    /// `~` a listener, `[n]` people inside a closed branch.
    fn describe(server: &ServerState) -> Vec<String> {
        server
            .tree_rows()
            .into_iter()
            .map(|row| match row {
                TreeRow::Channel(ChannelRow {
                    index,
                    depth,
                    has_children,
                    expanded,
                    hidden_users,
                    hidden_talking,
                }) => {
                    let channel = &server.channels[index];
                    let branch = match (has_children, expanded) {
                        (false, _) => " ",
                        (true, true) => "v",
                        (true, false) => ">",
                    };
                    let hidden = match hidden_users {
                        0 => String::new(),
                        count if hidden_talking => format!(" [{count} talking]"),
                        count => format!(" [{count}]"),
                    };
                    format!("{}{branch} {}{hidden}", "  ".repeat(depth), channel.name)
                }
                TreeRow::User { session, depth } => {
                    format!("{}@{}", "  ".repeat(depth), server.users[&session].name)
                }
                TreeRow::Listener { session, depth } => {
                    format!("{}~{}", "  ".repeat(depth), server.users[&session].name)
                }
            })
            .collect()
    }

    #[test]
    fn roots_and_the_voice_branch_start_open() {
        assert_eq!(
            describe(&sample()),
            [
                "v Root",
                "  @Zed",
                "  > Music [1 talking]",
                "    AFK",
                "    @bob",
                "  v Lobby",
                "      Development",
                "      @Ada Lovelace",
                "      @ada lovelace",
                "      @andi",
                "      @grace",
                "      ~bob",
                "    > Gaming [1]",
            ]
        );
    }

    #[test]
    fn closed_branches_count_who_is_inside_and_keep_their_own_people() {
        let mut server = sample();
        server.set_expanded(4, true);
        assert_eq!(
            describe(&server)[12..],
            [
                "    v Gaming",
                "        Shooters",
                "        @Linus",
                "        Strategy",
            ]
        );
        server.talking.insert(4);
        server.set_expanded(2, false);
        assert_eq!(describe(&server)[5..], ["  > Lobby [5 talking]"]);
        server.set_expanded(0, false);
        assert_eq!(describe(&server), ["> Root [7 talking]", "  @Zed"]);
    }

    #[test]
    fn the_branch_follows_our_voice_channel_unless_chosen_by_hand() {
        let mut server = sample();
        server.users.get_mut(&1).unwrap().channel = 6;
        let rows = describe(&server);
        assert!(rows.contains(&"    v Gaming".to_string()), "{rows:#?}");
        assert!(rows.contains(&"        @andi".to_string()), "{rows:#?}");
        server.set_expanded(2, false);
        assert_eq!(describe(&server)[5..], ["  > Lobby [5]"]);
        server.own_session = None;
        server.expansion.clear();
        assert_eq!(describe(&server)[5..], ["  > Lobby [5]"]);
    }

    #[test]
    fn connecting_opens_every_branch_with_people() {
        let mut server = sample();
        server.set_expanded(2, false);
        server.expand_occupied();
        assert_eq!(
            describe(&server),
            [
                "v Root",
                "  @Zed",
                "  v Music",
                "      Listening party",
                "      @Margaret",
                "    AFK",
                "    @bob",
                "  v Lobby",
                "      Development",
                "      @Ada Lovelace",
                "      @ada lovelace",
                "      @andi",
                "      @grace",
                "      ~bob",
                "    v Gaming",
                "        Shooters",
                "        @Linus",
                "        Strategy",
            ]
        );
    }

    #[test]
    fn a_parent_cycle_or_duplicate_never_hangs() {
        let mut server = sample();
        server.channels.push(channel(20, Some(21), "Ping", 0));
        server.channels.push(channel(21, Some(20), "Pong", 0));
        server.channels.push(channel(22, Some(22), "Self", 0));
        server.users.insert(11, user(11, "Loop", 20));
        server.own_session = Some(11);
        server.expand_occupied();
        assert_eq!(describe(&server).len(), 18);
        // A second Lobby beneath one of Lobby's own descendants.
        server.channels.push(channel(2, Some(5), "Lobby again", 0));
        server.expand_occupied();
        describe(&server);
    }

    /// 2,000 channels eight to a parent and 5,000 people:
    /// `cargo test --release -p nuschel-client large_tree -- --ignored --nocapture`
    #[test]
    #[ignore = "timing; run in release"]
    fn large_tree_timing() {
        use std::{hint::black_box, time::Instant};
        let channels = || {
            (0..2000u32).map(|id| {
                let name = format!("Channel {}", id * 7919 % 2000);
                channel(
                    id,
                    id.checked_sub(1).map(|id| id / 8),
                    &name,
                    (id % 3) as i32,
                )
            })
        };
        let mut server =
            ServerState::new(ServerBookmark::new("Test", "localhost", 64738, "andi").unwrap());
        server.channels = channels().collect();
        server.sort_channels();
        for session in 0..5000u32 {
            let name = ["ada", "Bob", "carol", "Dave"][session as usize % 4];
            let mut user = user(session, &format!("{name} {session}"), session * 31 % 2000);
            if session % 50 == 0 {
                user.listening
                    .extend([session * 7 % 2000, session * 13 % 2000]);
            }
            server.users.insert(session, user);
        }
        server.own_session = Some(4999);
        server.talking.extend((0..5000).step_by(100));
        let rows = |label: &str, server: &ServerState| {
            let rounds = 20;
            let start = Instant::now();
            let mut rows = 0;
            for _ in 0..rounds {
                rows = black_box(server.tree_rows()).len();
            }
            println!(
                "tree_rows, {label}: {rows} rows, {:?}",
                start.elapsed() / rounds
            );
        };
        rows("default expansion", &server);
        let start = Instant::now();
        server.expand_occupied();
        println!("expand_occupied: {:?}", start.elapsed());
        rows("occupied branches open", &server);

        // Joining: every channel arrives, sorted after each or once at the end.
        let start = Instant::now();
        server.channels.clear();
        for channel in channels() {
            server.channels.push(channel);
            server.sort_channels();
        }
        println!("2,000 arrivals, sorting after each: {:?}", start.elapsed());
        let start = Instant::now();
        server.channels.clear();
        server.channels.extend(channels());
        server.sort_channels();
        println!("2,000 arrivals, sorting once: {:?}", start.elapsed());
    }
}
