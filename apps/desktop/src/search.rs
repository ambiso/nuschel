// SPDX-License-Identifier: EUPL-1.2
//! Message search (Ctrl+F, Cmd+F on macOS; Ctrl+Shift+F for all servers) in
//! local history: full text in the open conversation, its server, or every
//! server. Choosing a result opens its message in its conversation, where
//! older and newer history loads around it as the reader scrolls.
use crate::{
    assets::icon,
    dialogs::{Tone, styled_button},
    state::{Desktop, accessible, local_time},
    theme::{self, *},
};
use gpui_kit::{
    base::input::{Input, InputEvent, InputState},
    prelude::*,
    *,
};
pub use nuschel_client::search::Scope;
use nuschel_client::{
    Conversation,
    search::{Hit, Located},
    timeline::Page,
};
use nuschel_core::{SearchCursor, SearchPage, search::Query};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// How long the message a result opened stays highlighted.
const HIGHLIGHT: Duration = Duration::from_millis(2500);

static REQUESTS: AtomicU64 = AtomicU64::new(0);

pub struct SearchView {
    input: Entity<InputState>,
    scope: Scope,
    /// Where it was opened, which the conversation and server scopes search.
    origin: Conversation,
    /// The query, scope and place the results are for.
    searched: Option<(String, Scope, Conversation)>,
    results: Vec<SearchHit>,
    next: Option<SearchCursor>,
    loading: bool,
    indexing: bool,
    error: Option<String>,
    /// Replies for an older query or scope are dropped on arrival.
    request: u64,
    /// Out of the way while a conversation is shown; Ctrl+F brings it back.
    hidden: bool,
    _changes: Subscription,
}

/// A result as the SDK names it (`nuschel_client::search`), with its local time.
struct SearchHit {
    hit: Hit,
    time: String,
}

impl std::ops::Deref for SearchHit {
    type Target = Hit;
    fn deref(&self) -> &Hit {
        &self.hit
    }
}

/// The message a result opened, highlighted for a moment once in view.
pub struct Jumped {
    conversation: Conversation,
    id: i64,
    until: Option<Instant>,
}

impl SearchView {
    fn query(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }
}

impl Desktop {
    /// Search messages in the open conversation, or with `global` in every
    /// server; the field keeps what was searched last. Not behind a dialog.
    pub fn open_search(&mut self, global: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog_open() {
            return;
        }
        let Some(origin) = self.client.navigation.selection().cloned() else {
            return;
        };
        let scope = if global {
            Scope::All
        } else {
            Scope::Conversation
        };
        let search = match &mut self.search {
            Some(search) => search,
            None => {
                let input = cx.new(|cx| {
                    accessible(
                        InputState::new(window, cx)
                            .placeholder("Search messages: words, \"exact phrase\", from:name"),
                        "Search messages",
                        false,
                        false,
                    )
                });
                // Search as you type.
                let changes = cx.subscribe(&input, |this, _, event, cx| {
                    if matches!(event, InputEvent::Change | InputEvent::PressEnter { .. }) {
                        this.run_search(cx);
                        cx.notify();
                    }
                });
                self.search.insert(SearchView {
                    input,
                    scope,
                    origin: origin.clone(),
                    searched: None,
                    results: Vec::new(),
                    next: None,
                    loading: false,
                    indexing: false,
                    error: None,
                    request: 0,
                    hidden: false,
                    _changes: changes,
                })
            }
        };
        search.hidden = false;
        search.scope = scope;
        search.origin = origin;
        search.input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.run_search(cx);
        cx.notify();
    }

    /// The conversation header's search button: open search, or close it.
    pub fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.as_ref().is_some_and(|search| !search.hidden) {
            self.close_search(window, cx);
        } else {
            self.open_search(false, window, cx);
        }
    }

    pub fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search = None;
        self.composer
            .update(cx, |input, cx| input.focus(window, cx));
    }

    pub fn set_search_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if let Some(search) = &mut self.search {
            search.scope = scope;
            self.run_search(cx);
        }
    }

    /// A conversation was browsed: the switcher ranks it, and search makes
    /// way for it.
    pub fn browsed(&mut self, conversation: &Conversation) {
        self.finder.visited(conversation);
        if let Some(search) = &mut self.search {
            search.hidden = true;
        }
    }

    /// The history identities and the conversation (storage key) a search
    /// covers (the SDK decides).
    fn search_scope(&self) -> Result<(Vec<String>, Option<String>), &'static str> {
        let search = self
            .search
            .as_ref()
            .ok_or("Connect to this server once to search its history.")?;
        self.client.search_scope(&search.origin, search.scope)
    }

    /// Search again if the query, scope or place changed.
    fn run_search(&mut self, cx: &mut Context<Self>) {
        let scope = self.search_scope();
        let Some(search) = &mut self.search else {
            return;
        };
        let query = search.query(cx);
        let key = (query.clone(), search.scope, search.origin.clone());
        if search.searched.as_ref() == Some(&key) {
            return;
        }
        search.searched = Some(key);
        search.results.clear();
        search.next = None;
        search.error = None;
        search.indexing = false;
        search.loading = false;
        search.request = REQUESTS.fetch_add(1, Ordering::Relaxed) + 1;
        if Query::parse(&query).is_empty() {
            return;
        }
        match scope {
            Ok(scope) => match self.client.search(search.request, scope, query, None) {
                Ok(()) => search.loading = true,
                Err(error) => search.error = Some(error),
            },
            Err(error) => search.error = Some(error.into()),
        }
    }

    /// The next page of results.
    fn more_results(&mut self, cx: &mut Context<Self>) {
        let scope = self.search_scope();
        let Some(search) = &mut self.search else {
            return;
        };
        let (Some(next), Ok(scope)) = (search.next, scope) else {
            return;
        };
        if search.loading {
            return;
        }
        match self
            .client
            .search(search.request, scope, search.query(cx), Some(next))
        {
            Ok(()) => search.loading = true,
            Err(error) => search.error = Some(error),
        }
    }

    pub fn apply_search_results(
        &mut self,
        request: u64,
        result: Result<SearchPage, String>,
        cx: &App,
    ) {
        let Some(search) = self
            .search
            .as_mut()
            .filter(|search| search.request == request)
        else {
            return;
        };
        search.loading = false;
        let page = match result {
            Ok(page) => page,
            Err(error) => {
                search.error = Some(error);
                return;
            }
        };
        search.indexing = page.indexing;
        search.next = page.next;
        let query = search.query(cx);
        let hits = self.client.search_hits(page.hits, &query);
        if let Some(search) = &mut self.search {
            search.results.extend(hits.into_iter().map(|hit| SearchHit {
                time: local_time(hit.observed_at_ms),
                hit,
            }));
        }
    }

    fn open_search_result(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = &mut self.search else {
            return;
        };
        let Some(hit) = search.results.get(index) else {
            return;
        };
        let (server, key, id) = (
            hit.server.clone(),
            hit.storage_key.clone(),
            (hit.id, hit.seq),
        );
        search.hidden = true;
        self.composer
            .update(cx, |input, cx| input.focus(window, cx));
        self.jump_to_message(&server, &key, id, window, cx);
    }

    /// Show stored message `id` in its conversation (`storage_key` on
    /// `server`), loading the history around it, and highlight it briefly.
    pub fn jump_to_message(
        &mut self,
        server: &str,
        storage_key: &str,
        (id, seq): (i64, Option<i64>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .client
            .servers
            .iter()
            .position(|entry| entry.id == server)
        else {
            return;
        };
        let mut located = self.client.locate(index, storage_key, (id, seq));
        // An earlier direct conversation, with someone who is online again.
        if let Ok(Located::Reopen { session }) = located {
            self.open_direct(index, session, window, cx);
            located = self.client.locate(index, storage_key, (id, seq));
        }
        let (position, id) = match located {
            Ok(Located::Found { position, id }) => (position, id),
            Ok(Located::Reopen { .. }) => {
                self.client.servers[index].notice =
                    Some(nuschel_client::search::OPENS_ONLINE.into());
                return;
            }
            Err(notice) => {
                self.client.servers[index].notice = Some(notice.into());
                return;
            }
        };
        let channel = self.client.servers[index].channels[position].id;
        let conversation = Conversation {
            server: server.to_string(),
            channel,
        };
        if self.client.navigation.selection() != Some(&conversation) {
            self.browse_channel(index, channel, window, cx);
        }
        self.jumped = Some(Jumped {
            conversation,
            id,
            until: None,
        });
        match self.client.show_message(index, position, id) {
            Ok(true) => self.reveal_jumped(cx),
            Ok(false) => {}
            Err(error) => {
                self.older_paused = true;
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    /// Ask for a page of the conversation at `position` on server `index`;
    /// replies for an earlier request are dropped.
    fn request_page(&mut self, index: usize, position: usize, page: Page) {
        if let Err(error) = self.client.load_page(index, position, page) {
            self.older_paused = true;
            self.error = Some(error);
        }
    }

    /// Scroll the jumped-to message into view and start its highlight.
    pub(crate) fn reveal_jumped(&mut self, cx: &mut Context<Self>) {
        let Some(jumped) = &mut self.jumped else {
            return;
        };
        self.anchor.reveal(jumped.id);
        let until = Instant::now() + HIGHLIGHT;
        jumped.until = Some(until);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HIGHLIGHT).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .jumped
                    .as_ref()
                    .is_some_and(|jumped| jumped.until == Some(until))
                {
                    this.jumped = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Before each frame, for a timeline showing older history apart from
    /// the newest (after a jump): it does not follow the bottom, and the next
    /// newer page loads before the reader reaches the end of what is loaded.
    /// Sending a message or "Jump to latest" (which follow the bottom) go
    /// back to the newest page. Returns false for any other timeline.
    pub fn settle_detached(&mut self) -> bool {
        let Some((index, position)) = self.selected_indices() else {
            return false;
        };
        let channel = &self.client.servers[index].channels[position];
        let Some(after) = channel.timeline.next_after else {
            return false;
        };
        if self.client.focus.following {
            if !channel.timeline.loading {
                self.load_history(None);
            }
            return false;
        }
        if let Some((painted, _)) = self.anchor.painted()
            && self.scroll.offset().y != painted
        {
            // The reader is scrolling again: a failed page may be retried.
            self.older_paused = false;
        }
        let below = self.scroll.max_offset().y + self.scroll.offset().y;
        if !channel.timeline.loading
            && !self.older_paused
            && below < self.scroll.bounds().size.height.max(px(200.))
        {
            self.request_page(index, position, Page::After(after));
        }
        true
    }
}

/// Whether the search panel is shown.
pub fn panel_open(this: &Desktop) -> bool {
    this.search.as_ref().is_some_and(|search| !search.hidden)
}

/// The message row, highlighted if a search result just opened it.
pub fn mark_jumped(this: &Desktop, id: Option<i64>, row: AnyElement) -> AnyElement {
    let jumped = this.jumped.as_ref().is_some_and(|jumped| {
        jumped.until.is_some()
            && Some(jumped.id) == id
            && this.client.navigation.selection() == Some(&jumped.conversation)
    });
    if !jumped {
        return row;
    }
    div()
        .rounded(px(8.))
        .bg(tint(ACCENT, 0x30))
        .child(row)
        .into_any_element()
}

fn scope_tab(
    scope: Scope,
    current: Scope,
    label: &'static str,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let selected = scope == current;
    div()
        .id(label)
        .role(Role::Tab)
        .aria_label(label)
        .aria_selected(selected)
        .tab_index(0)
        .px_3()
        .py(px(4.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_sm()
        .text_color(color(if selected { TEXT } else { TEXT_MUTED }))
        .when(selected, |tab| tab.bg(color(SELECTED)))
        .hover(|style| style.bg(color(HOVER)))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.set_search_scope(scope, cx);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                this.set_search_scope(scope, cx);
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .child(text!(label).with_id("scope-label"))
        .into_any_element()
}

fn result_row(
    hit: &SearchHit,
    index: usize,
    global: bool,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let place = if global {
        format!("{} · {}", hit.place, hit.server_name)
    } else {
        hit.place.clone()
    };
    let label = format!("{} in {}, {}: {}", hit.author, place, hit.time, hit.snippet);
    let snippet =
        StyledText::new(hit.snippet.clone()).with_highlights(hit.highlights.iter().map(|range| {
            (
                range.clone(),
                HighlightStyle {
                    color: Some(color(TEXT).into()),
                    background_color: Some(tint(ACCENT, 0x55).into()),
                    font_weight: Some(FontWeight::SEMIBOLD),
                    ..Default::default()
                },
            )
        }));
    let open = move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
        this.open_search_result(index, window, cx);
        cx.notify();
    };
    div()
        .id(("search-hit", index))
        .role(Role::ListItem)
        .aria_label(label)
        .tab_index(0)
        .p_3()
        .rounded(px(8.))
        .cursor_pointer()
        .hover(|style| style.bg(color(HOVER)))
        .flex()
        .flex_col()
        .gap_1()
        .on_click(cx.listener(move |this, _, window, cx| open(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                cx.stop_propagation();
                open(this, window, cx);
            }
        }))
        .child(
            div()
                .flex()
                .gap_2()
                .text_xs()
                .text_color(color(TEXT_FAINT))
                .child(Text::new_inaccessible(place.into()))
                .child(Text::new_inaccessible(hit.time.clone().into())),
        )
        .child(
            div()
                .flex()
                .gap_2()
                .child(
                    div()
                        .flex_shrink_0()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::name_color(&hit.author))
                        .child(Text::new_inaccessible(hit.author.clone().into())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(color(TEXT_MUTED))
                        .child(snippet),
                ),
        )
        .into_any_element()
}

/// The search panel, in place of the timeline while it is shown.
pub fn panel(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let search = this.search.as_ref().filter(|_| panel_open(this))?;
    let query = search.query(cx);
    let count = search.results.len();
    let mut status = if let Some(error) = &search.error {
        format!("Search failed: {error}")
    } else if Query::parse(&query).is_empty() {
        "Searches messages saved on this device. All words must match, the last one also as you \
         type; \"quoted words\" match together, in order; from:name finds someone's messages."
            .to_string()
    } else if search.loading && count == 0 {
        "Searching…".to_string()
    } else if count == 0 {
        format!("No messages match “{}”.", query.trim())
    } else {
        format!(
            "{count} {}{}",
            if count == 1 { "message" } else { "messages" },
            if search.next.is_some() {
                " so far, best first"
            } else {
                ", best first"
            }
        )
    };
    if search.indexing {
        status.push_str(" Older messages are still being indexed.");
    }
    let global = search.scope == Scope::All;
    let results = search
        .results
        .iter()
        .enumerate()
        .map(|(index, hit)| result_row(hit, index, global, cx))
        .collect::<Vec<_>>();
    let more = search.next.is_some().then(|| {
        div().py_2().flex().justify_center().child(styled_button(
            "search-more",
            if search.loading {
                "Loading…"
            } else {
                "More results"
            },
            Tone::Secondary,
            |this, _, cx| {
                this.more_results(cx);
                cx.notify();
            },
            cx,
        ))
    });
    Some(
        div()
            .id("search-panel")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .px_4()
            .py_3()
            .gap_3()
            .child(
                div()
                    .px_3()
                    .py(px(9.))
                    .rounded(px(8.))
                    .bg(color(INPUT))
                    .border_1()
                    .border_color(color(BORDER_STRONG))
                    .flex()
                    .items_center()
                    .gap_2()
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            this.close_search(window, cx);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .child(icon("search", 16., color(TEXT_FAINT)))
                    .child(div().flex_1().child(Input::new(&search.input))),
            )
            .child(
                div()
                    .id("search-scope")
                    .role(Role::TabList)
                    .aria_label("Search in")
                    .flex()
                    .gap_1()
                    .child(scope_tab(
                        Scope::Conversation,
                        search.scope,
                        "This conversation",
                        cx,
                    ))
                    .child(scope_tab(Scope::Server, search.scope, "This server", cx))
                    .child(scope_tab(Scope::All, search.scope, "All servers", cx)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(color(if search.error.is_some() {
                        DANGER
                    } else {
                        TEXT_MUTED
                    }))
                    .child(text!(status).with_id("search-status")),
            )
            .child(
                div()
                    .id("search-results")
                    .role(Role::List)
                    .aria_label(format!("{count} results"))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(results)
                    .children(more),
            )
            .into_any_element(),
    )
}
