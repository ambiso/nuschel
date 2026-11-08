// SPDX-License-Identifier: EUPL-1.2
//! Explicitly labelled local fixtures until the network session adapter exists.
use nuschel_client::{HistorySource, SessionSecurity};
use nuschel_core::{ContentBlock, MessageDocument};

use crate::view::{ChannelView, MessageView, ServerView};

pub(super) fn servers(document: MessageDocument, has_more: bool) -> Vec<ServerView> {
    let mut community = vec![
        channel(
            0,
            None,
            "welcome",
            "A place for familiar voices",
            "Welcome to the Nuschel preview.\nChoose a channel to browse its conversation.",
        ),
        channel(
            1,
            None,
            "lounge",
            "Drop in, stay a while",
            "Channel trees keep related conversations together.\nSelecting a channel here does not join its voice conversation.",
        ),
        channel(
            2,
            Some(1),
            "general",
            "Everyday conversations",
            "Good morning!\n\nWhat is everyone working on today?",
        ),
        channel(
            3,
            Some(1),
            "development",
            "Ideas, experiments, and things we're making",
            "The Rust room is nested under this channel.\nYou can collapse branches without leaving the conversation you are reading.",
        ),
        channel(
            4,
            Some(3),
            "rust",
            "Talk Rust and share code",
            "Code blocks keep their syntax highlighting.\nBlank lines and ordinary newlines remain visible.",
        ),
        channel(
            5,
            None,
            "music",
            "What is on your playlist?",
            "A quiet place to share what you are listening to.",
        ),
    ];
    community[0].messages.push(MessageView {
        author: "Nuschel".into(),
        time: "12:08".into(),
        document,
        expanded: false,
        has_more,
    });
    vec![
        ServerView {
            id: "preview:nuschel".into(),
            name: "Nuschel community".into(),
            initials: "N".into(),
            history_source: HistorySource::ServerArchive,
            // This fixture demonstrates the non-E2EE mode, not a negotiated connection.
            security: SessionSecurity::TransportOnly,
            channels: community,
        },
        ServerView {
            id: "preview:mumble".into(),
            name: "Mumble friends".into(),
            initials: "M".into(),
            history_source: HistorySource::LocalObserved,
            security: SessionSecurity::TransportOnly,
            channels: vec![
                channel(
                    0,
                    None,
                    "root",
                    "A familiar Mumble channel tree",
                    "Legacy servers keep working with Nuschel.\nTheir chat history is saved locally by default.",
                ),
                channel(
                    1,
                    Some(0),
                    "friends",
                    "A place to catch up",
                    "Local history contains conversations observed on this device.\nIt cannot recover messages sent while you were offline.",
                ),
            ],
        },
    ]
}

fn channel(id: u32, parent: Option<u32>, name: &str, topic: &str, body: &str) -> ChannelView {
    ChannelView {
        id,
        parent,
        name: name.into(),
        topic: topic.into(),
        messages: vec![MessageView {
            author: "Nuschel".into(),
            time: "12:00".into(),
            expanded: false,
            has_more: false,
            document: MessageDocument {
                blocks: vec![ContentBlock::Text { text: body.into() }],
            },
        }],
    }
}
