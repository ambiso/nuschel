// SPDX-License-Identifier: EUPL-1.2
use gpui_kit::{prelude::*, *};
use nuschel_core::{ContentBlock, MessageDocument};

pub struct Desktop {
    pub document: MessageDocument,
    pub expanded: bool,
    pub has_more: bool,
    pub history_enabled: bool,
}

impl Render for Desktop {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let blocks = self
            .document
            .blocks
            .iter()
            .enumerate()
            .map(|(index, block)| match block {
                ContentBlock::Text { text: source } => div()
                    .id(("text", index))
                    .child(text!(source.clone()).with_id(("body", index)))
                    .into_any_element(),
                ContentBlock::Code { language, runs } => {
                    let mut source = String::new();
                    let mut highlights = Vec::new();
                    for run in runs {
                        let start = source.len();
                        source.push_str(&run.text);
                        let color = (u32::from(run.foreground[0]) << 16)
                            | (u32::from(run.foreground[1]) << 8)
                            | u32::from(run.foreground[2]);
                        highlights.push((
                            start..source.len(),
                            HighlightStyle {
                                color: Some(rgb(color).into()),
                                font_weight: run.bold.then_some(FontWeight::BOLD),
                                font_style: run.italic.then_some(FontStyle::Italic),
                                ..Default::default()
                            },
                        ));
                    }
                    div()
                        .id(("code", index))
                        .p_4()
                        .rounded_md()
                        .bg(rgb(0x151923))
                        .font_family("monospace")
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            text!(language.clone().unwrap_or_else(|| "code".into()))
                                .with_id(("language", index)),
                        )
                        .child(
                            div()
                                .id(("code-text", index))
                                .role(Role::Label)
                                .aria_label(source.clone())
                                .child(StyledText::new(source).with_highlights(highlights)),
                        )
                        .into_any_element()
                }
            })
            .collect::<Vec<_>>();

        div()
            .size_full()
            .flex()
            .bg(rgb(0x202530))
            .text_color(rgb(0xe7eaf0))
            .font_family("sans-serif")
            .text_base()
            .child(
                div()
                    .w(px(224.))
                    .h_full()
                    .p_5()
                    .bg(rgb(0x171b24))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(text!("nuschel").with_id("brand"))
                    .child(text!("# welcome").with_id("channel"))
                    .child(text!("Not connected").with_id("connection")),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(text!("# welcome").with_id("heading"))
                    .child(
                        text!("Desktop foundation · Mumble sessions are not connected yet")
                            .with_id("status"),
                    )
                    .child(
                        div()
                            .id("message")
                            .flex_1()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .id("message-content")
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .when(!self.expanded, |view| {
                                        view.max_h(px(320.)).overflow_hidden()
                                    })
                                    .children(blocks),
                            )
                            .when(self.has_more, |view| {
                                view.child(
                                    div()
                                        .id("show-more")
                                        .role(Role::Button)
                                        .tab_index(0)
                                        .cursor_pointer()
                                        .text_color(rgb(0x9ab5ff))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.expanded = !this.expanded;
                                            cx.notify();
                                        }))
                                        .on_key_down(cx.listener(
                                            |this, event: &KeyDownEvent, _, cx| {
                                                if event.keystroke.key == "enter"
                                                    || event.keystroke.key == "space"
                                                {
                                                    this.expanded = !this.expanded;
                                                    cx.notify();
                                                }
                                            },
                                        ))
                                        .child(
                                            text!(if self.expanded {
                                                "Show less"
                                            } else {
                                                "Show more"
                                            })
                                            .with_id("expand-label"),
                                        ),
                                )
                            }),
                    )
                    .child(
                        text!(if self.history_enabled {
                            "Local history enabled by default"
                        } else {
                            "Local history disabled"
                        })
                        .with_id("history"),
                    ),
            )
    }
}
