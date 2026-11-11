// SPDX-License-Identifier: EUPL-1.2
//! What already happens to the microphone signal before Nuschl receives it.
//!
//! A PipeWire microphone is often a chain: hardware → echo-cancel → RNNoise
//! filter-chain → the default source. Nuschl walks that chain upstream from
//! the source it captures so it neither doubles a system echo canceller or
//! noise suppressor nor starves one of its playback reference.

/// One PipeWire node, reduced to the properties the walk needs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub media_name: String,
    pub class: String,
    /// Shared by the nodes one module creates (filter-chain, echo-cancel, loopback).
    pub link_group: Option<String>,
    /// Where a stream asks to be linked (`target.object`): a node name or serial.
    pub target: Option<String>,
    pub serial: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Link {
    pub output: u32,
    pub input: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub links: Vec<Link>,
}

/// Processing found upstream of a source.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Upstream {
    /// A readable name for the stage that cancels echo.
    pub echo: Option<String>,
    /// A readable name for the stage that suppresses noise.
    pub noise: Option<String>,
    /// The echo canceller's sink: Nuschl's playback must reach it as the reference.
    pub reference: Option<String>,
    /// The walk met a capture stream without links (a passive filter that
    /// nobody records from yet): the answer may be incomplete.
    pub dangling: bool,
}

const MAX_STAGES: usize = 16;

const NOISE_MARKERS: &[&str] = &[
    "rnnoise",
    "noise cancel",
    "noise-cancel",
    "noise_cancel",
    "noisecancel",
    "noise suppress",
    "noise-suppress",
    "noise_suppress",
    "noise reduction",
    "noise-reduction",
    "noise_reduction",
    "denois",
    "deepfilter",
    "deep_filter",
    "deep-filter",
    "noisetorch",
];

const ECHO_MARKERS: &[&str] = &["echo-cancel", "echo_cancel", "echocancel", "echo cancel"];

impl Node {
    fn label(&self) -> String {
        [&self.description, &self.media_name, &self.name]
            .into_iter()
            .find(|text| !text.is_empty())
            .cloned()
            .unwrap_or_default()
    }
}

impl Graph {
    fn node(&self, id: u32) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// The nodes one module created together with `node`.
    fn stage<'a>(&'a self, node: &'a Node) -> Vec<&'a Node> {
        match &node.link_group {
            Some(group) => self
                .nodes
                .iter()
                .filter(|other| other.link_group.as_ref() == Some(group))
                .collect(),
            None => vec![node],
        }
    }

    /// Walk upstream from the source named `source` (a PipeWire `node.name`).
    pub fn upstream(&self, source: &str) -> Upstream {
        let mut found = Upstream::default();
        let Some(mut current) = self.nodes.iter().find(|node| node.name == source) else {
            return found;
        };
        let mut visited = vec![current.id];
        for _ in 0..MAX_STAGES {
            let stage = self.stage(current);
            let text = stage
                .iter()
                .flat_map(|node| {
                    [
                        node.name.as_str(),
                        node.description.as_str(),
                        node.media_name.as_str(),
                        node.link_group.as_deref().unwrap_or_default(),
                    ]
                })
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            // The node that produces the signal names the stage best.
            let label = stage
                .iter()
                .find(|node| node.class.starts_with("Audio/Source"))
                .unwrap_or(&current)
                .label();
            if found.echo.is_none() && ECHO_MARKERS.iter().any(|marker| text.contains(marker)) {
                found.echo = Some(label.clone());
                found.reference = stage
                    .iter()
                    .find(|node| node.class == "Audio/Sink")
                    .map(|node| node.name.clone());
            }
            if found.noise.is_none() && NOISE_MARKERS.iter().any(|marker| text.contains(marker)) {
                found.noise = Some(label);
            }
            // Follow the capture side only: a sink in the stage receives other
            // applications' playback, not the microphone.
            let capture: Vec<u32> = stage
                .iter()
                .filter(|node| {
                    node.class != "Audio/Sink" && !node.class.starts_with("Stream/Output")
                })
                .map(|node| node.id)
                .collect();
            let linked = self
                .links
                .iter()
                .filter(|link| capture.contains(&link.input))
                .map(|link| link.output)
                .find(|id| !visited.contains(id))
                .and_then(|id| self.node(id));
            // An unlinked capture stream: follow the node it asks for, if any.
            let unlinked = stage.iter().find(|node| {
                node.class.starts_with("Stream/Input")
                    && !self.links.iter().any(|link| link.input == node.id)
            });
            found.dangling |= linked.is_none() && unlinked.is_some();
            let next = linked.or_else(|| {
                let target = unlinked?.target.as_deref()?;
                self.nodes.iter().find(|node| {
                    !visited.contains(&node.id)
                        && (node.name == target || node.serial.as_deref() == Some(target))
                })
            });
            let Some(next) = next else {
                break;
            };
            visited.extend(self.stage(next).iter().map(|node| node.id));
            current = next;
        }
        found
    }
}

/// Read the current graph from the PipeWire daemon, or `None` without one.
#[cfg(target_os = "linux")]
pub fn snapshot() -> Option<Graph> {
    use pipewire as pw;
    use std::{cell::RefCell, rc::Rc, time::Duration};

    let runtime =
        std::env::var_os("PIPEWIRE_RUNTIME_DIR").or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))?;
    let socket = std::env::var("PIPEWIRE_REMOTE").unwrap_or_else(|_| "pipewire-0".into());
    if !std::path::Path::new(&runtime).join(&socket).exists() {
        return None;
    }
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).ok()?;
    let context = pw::context::ContextRc::new(&mainloop, None).ok()?;
    let core = context.connect_rc(None).ok()?;
    let registry = core.get_registry_rc().ok()?;
    let graph = Rc::new(RefCell::new(Graph::default()));
    // The registry announces only a few properties per node; the module
    // grouping (`node.link-group`) and stream targets come with each node's
    // info, so every node is bound and read in a second round trip.
    let bound = Rc::new(RefCell::new(Vec::new()));
    let pending = Rc::new(std::cell::Cell::new(core.sync(0).ok()?));
    let rounds = Rc::new(std::cell::Cell::new(0_u8));
    let finished = Rc::new(std::cell::Cell::new(false));
    let _core_listener = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            let finished = finished.clone();
            let core = core.clone();
            let pending = pending.clone();
            let rounds = rounds.clone();
            move |id, seq| {
                if id != pw::core::PW_ID_CORE || seq != pending.get() {
                    return;
                }
                rounds.set(rounds.get() + 1);
                match core.sync(0) {
                    Ok(next) if rounds.get() < 2 => pending.set(next),
                    _ => {
                        finished.set(rounds.get() >= 2);
                        mainloop.quit();
                    }
                }
            }
        })
        .register();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let graph = graph.clone();
            let bound = bound.clone();
            let registry = registry.downgrade();
            move |global| {
                let Some(props) = global.props else {
                    return;
                };
                let text = |key: &str| props.get(key).unwrap_or_default().to_string();
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        graph.borrow_mut().nodes.push(Node {
                            id: global.id,
                            name: text("node.name"),
                            description: text("node.description"),
                            media_name: text("media.name"),
                            class: text("media.class"),
                            link_group: None,
                            target: None,
                            serial: props.get("object.serial").map(str::to_string),
                        });
                        let Some(registry) = registry.upgrade() else {
                            return;
                        };
                        let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                            return;
                        };
                        let id = global.id;
                        let graph = graph.clone();
                        let listener = node
                            .add_listener_local()
                            .info(move |info| {
                                let Some(props) = info.props() else {
                                    return;
                                };
                                let mut graph = graph.borrow_mut();
                                let Some(node) = graph.nodes.iter_mut().find(|node| node.id == id)
                                else {
                                    return;
                                };
                                node.link_group = props.get("node.link-group").map(str::to_string);
                                node.target = props
                                    .get("target.object")
                                    .or_else(|| props.get("node.target"))
                                    .map(str::to_string);
                                if let Some(name) = props.get("media.name") {
                                    node.media_name = name.to_string();
                                }
                            })
                            .register();
                        bound.borrow_mut().push((node, listener));
                    }
                    pw::types::ObjectType::Link => {
                        let id = |key: &str| props.get(key).and_then(|value| value.parse().ok());
                        if let (Some(output), Some(input)) =
                            (id("link.output.node"), id("link.input.node"))
                        {
                            graph.borrow_mut().links.push(Link { output, input });
                        }
                    }
                    _ => {}
                }
            }
        })
        .register();
    // A wedged daemon must not hold up opening the devices.
    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    timer.update_timer(Some(Duration::from_millis(750)), None);
    mainloop.run();
    if !finished.get() {
        return None;
    }
    let graph = graph.borrow().clone();
    Some(graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, name: &str, description: &str, class: &str, group: Option<&str>) -> Node {
        Node {
            id,
            name: name.into(),
            description: description.into(),
            media_name: String::new(),
            class: class.into(),
            link_group: group.map(str::to_string),
            target: None,
            serial: None,
        }
    }

    /// Hardware mic → echo-cancel → RNNoise filter-chain (PipeWire's own
    /// source-rnnoise.conf) → default source; Firefox plays into echo-cancel-sink.
    fn rnnoise_after_echo_cancel() -> Graph {
        Graph {
            nodes: vec![
                node(
                    40,
                    "alsa_input.usb-mic",
                    "USB Microphone",
                    "Audio/Source",
                    None,
                ),
                node(41, "alsa_output.pci", "Speakers", "Audio/Sink", None),
                node(
                    50,
                    "echo-cancel-capture",
                    "Echo-Cancel Capture",
                    "Stream/Input/Audio",
                    Some("echo-cancel-7-1"),
                ),
                node(
                    51,
                    "echo-cancel-source",
                    "Echo-Cancel Source",
                    "Audio/Source",
                    Some("echo-cancel-7-1"),
                ),
                node(
                    52,
                    "echo-cancel-sink",
                    "Echo-Cancel Sink",
                    "Audio/Sink",
                    Some("echo-cancel-7-1"),
                ),
                node(
                    53,
                    "echo-cancel-playback",
                    "Echo-Cancel Playback",
                    "Stream/Output/Audio",
                    Some("echo-cancel-7-1"),
                ),
                node(
                    60,
                    "effect_input.rnnoise",
                    "Noise Canceling source",
                    "Stream/Input/Audio",
                    Some("filter-chain-7-2"),
                ),
                node(
                    61,
                    "effect_output.rnnoise",
                    "Noise Canceling source",
                    "Audio/Source",
                    Some("filter-chain-7-2"),
                ),
                node(70, "Firefox", "", "Stream/Output/Audio", None),
            ],
            links: vec![
                Link {
                    output: 40,
                    input: 50,
                },
                Link {
                    output: 40,
                    input: 50,
                },
                Link {
                    output: 53,
                    input: 41,
                },
                Link {
                    output: 51,
                    input: 60,
                },
                Link {
                    output: 70,
                    input: 52,
                },
            ],
        }
    }

    #[test]
    fn rnnoise_over_echo_cancel_is_found_with_its_reference() {
        let found = rnnoise_after_echo_cancel().upstream("effect_output.rnnoise");
        assert_eq!(found.noise.as_deref(), Some("Noise Canceling source"));
        assert_eq!(found.echo.as_deref(), Some("Echo-Cancel Source"));
        assert_eq!(found.reference.as_deref(), Some("echo-cancel-sink"));
    }

    #[test]
    fn capturing_the_echo_canceller_directly_finds_no_noise_suppressor() {
        let found = rnnoise_after_echo_cancel().upstream("echo-cancel-source");
        assert_eq!(found.echo.as_deref(), Some("Echo-Cancel Source"));
        assert_eq!(found.noise, None);
    }

    #[test]
    fn a_plain_microphone_has_no_upstream_processing() {
        let found = rnnoise_after_echo_cancel().upstream("alsa_input.usb-mic");
        assert_eq!(found, Upstream::default());
    }

    #[test]
    fn renamed_echo_cancel_nodes_are_recognised_by_their_module_group() {
        let mut graph = rnnoise_after_echo_cancel();
        for node in &mut graph.nodes {
            if node.link_group.as_deref() == Some("echo-cancel-7-1") {
                node.name = node.name.replace("echo-cancel", "aec");
                node.description = "Clean mic".into();
            }
        }
        let found = graph.upstream("effect_output.rnnoise");
        assert_eq!(found.echo.as_deref(), Some("Clean mic"));
        assert_eq!(found.reference.as_deref(), Some("aec-sink"));
    }

    #[test]
    fn playback_into_the_echo_canceller_is_not_mistaken_for_the_microphone() {
        // Walking from echo-cancel-sink's side must not reach Firefox's stream.
        let graph = rnnoise_after_echo_cancel();
        let found = graph.upstream("effect_output.rnnoise");
        assert!(found.noise.is_some());
        let cyclic = Graph {
            links: [
                graph.links.clone(),
                vec![Link {
                    output: 61,
                    input: 50,
                }],
            ]
            .concat(),
            ..graph
        };
        // A cycle back into the chain ends the walk instead of looping.
        assert!(cyclic.upstream("effect_output.rnnoise").echo.is_some());
    }

    #[test]
    fn a_passive_filter_without_links_follows_its_target() {
        let mut graph = rnnoise_after_echo_cancel();
        graph.links.retain(|link| link.input != 60);
        let found = graph.upstream("effect_output.rnnoise");
        assert!(found.dangling && found.echo.is_none(), "{found:?}");
        graph
            .nodes
            .iter_mut()
            .find(|node| node.id == 60)
            .unwrap()
            .target = Some("echo-cancel-source".into());
        let found = graph.upstream("effect_output.rnnoise");
        assert_eq!(found.echo.as_deref(), Some("Echo-Cancel Source"));
        assert!(found.dangling);
    }

    #[test]
    fn a_noise_suppressor_through_a_loopback_into_a_virtual_source_is_found() {
        // NoiseTorch-style: filter → loopback playback → null sink used as a source.
        let graph = Graph {
            nodes: vec![
                node(1, "alsa_input.pci", "Built-in Mic", "Audio/Source", None),
                node(
                    2,
                    "nt-capture",
                    "NoiseTorch Microphone",
                    "Stream/Input/Audio",
                    Some("loopback-1-1"),
                ),
                node(
                    3,
                    "nt-playback",
                    "NoiseTorch Microphone",
                    "Stream/Output/Audio",
                    Some("loopback-1-1"),
                ),
                node(
                    4,
                    "nt_virtual_mic",
                    "NoiseTorch Microphone",
                    "Audio/Source/Virtual",
                    None,
                ),
            ],
            links: vec![
                Link {
                    output: 1,
                    input: 2,
                },
                Link {
                    output: 3,
                    input: 4,
                },
            ],
        };
        let found = graph.upstream("nt_virtual_mic");
        assert_eq!(found.noise.as_deref(), Some("NoiseTorch Microphone"));
        assert_eq!(found.echo, None);
    }
}
