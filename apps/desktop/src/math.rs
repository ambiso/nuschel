// SPDX-License-Identifier: EUPL-1.2
//! Formulas in messages (docs/math.md). The content worker finds them; the
//! sandboxed math worker renders each one into a coverage mask at the size
//! it is shown in device pixels; they are cached here by (LaTeX, inline or
//! display, size) and tinted with the text colour where they are painted,
//! so a theme change needs no new rendering and a zoom renders them anew,
//! sharp. Until a formula is ready its source shows; one that cannot be
//! rendered shows its source as code with a reason.
//!
//! The math worker starts with the first formula and stops after a minute
//! without one.
use crate::{document::InlineMath, state::Desktop};
use gpui_kit::*;
use nuschel_ipc::math::{MathImage, MathRequest, MathWorker};
use std::{cell::RefCell, collections::HashMap, ops::Range, rc::Rc, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};

/// Formulas kept rendered; past it, the least recently shown go.
const CACHE: usize = 512;
/// Formulas waiting for the worker; more wait for a later frame.
const QUEUE: usize = 64;
/// The worker stops after this long without a formula.
const IDLE: Duration = Duration::from_secs(60);
/// Formulas are set this much larger than the text around them: Computer
/// Modern looks small beside interface fonts (KaTeX uses 1.21).
const SCALE: f32 = 1.2;

/// A rendered formula: a coverage mask in device pixels.
pub struct Formula {
    width: u32,
    height: u32,
    baseline: u32,
    /// The device pixels per em it was rendered at.
    size: f32,
    coverage: Vec<u8>,
    /// The mask in the colour it was last painted in.
    tinted: RefCell<Option<(u32, Arc<RenderImage>)>>,
}

impl Formula {
    fn new(image: MathImage, size: f32) -> Option<Self> {
        let coverage = image.decode()?;
        Some(Self {
            width: image.width,
            height: image.height,
            baseline: image.baseline,
            size,
            coverage,
            tinted: RefCell::new(None),
        })
    }

    /// Width, height and baseline (from the top) in logical pixels, at `em`
    /// logical pixels per em.
    pub fn metrics(&self, em: Pixels) -> (Pixels, Pixels, Pixels) {
        let scale = f32::from(em) / self.size;
        (
            px(self.width as f32 * scale),
            px(self.height as f32 * scale),
            px(self.baseline as f32 * scale),
        )
    }

    /// The mask as an image in `color` (straight alpha, BGRA, as GPUI takes it).
    pub fn image(&self, color: Hsla) -> Option<Arc<RenderImage>> {
        let rgba = Rgba::from(color);
        let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
        let [red, green, blue, alpha] = [rgba.r, rgba.g, rgba.b, rgba.a].map(channel);
        let key = u32::from_be_bytes([red, green, blue, alpha]);
        if let Some((tint, image)) = self.tinted.borrow().as_ref()
            && *tint == key
        {
            return Some(image.clone());
        }
        let mut pixels = Vec::with_capacity(self.coverage.len() * 4);
        for &coverage in &self.coverage {
            let alpha = (u16::from(coverage) * u16::from(alpha) / 255) as u8;
            pixels.extend_from_slice(&[blue, green, red, alpha]);
        }
        let buffer = image::RgbaImage::from_raw(self.width, self.height, pixels)?;
        let image =
            crate::images::track(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])));
        *self.tinted.borrow_mut() = Some((key, image.clone()));
        Some(image)
    }
}

/// What to show for a formula.
#[derive(Clone)]
pub enum Shown {
    Formula(Rc<Formula>),
    /// Being rendered: its source shows meanwhile.
    Pending,
    /// Cannot be rendered, and why.
    Failed(SharedString),
    /// "Render math" is off.
    Off,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    tex: SharedString,
    display: bool,
    /// Device pixels per em, in quarters.
    size: u32,
}

enum Entry {
    Pending,
    Ready(Rc<Formula>),
    Failed(SharedString),
}

struct Job {
    request: MathRequest,
    reply: oneshot::Sender<Result<MathImage, String>>,
}

/// The rendered formulas and the worker's queue, for the whole app.
struct Cache {
    entries: HashMap<Key, (Entry, u64)>,
    clock: u64,
    jobs: Option<mpsc::Sender<Job>>,
    runtime: tokio::runtime::Handle,
    desktop: WeakEntity<Desktop>,
    enabled: bool,
    /// The scale factor formulas were last painted at.
    scale: f32,
}

impl Global for Cache {}

/// Called before a document is laid out: the setting, and where to report
/// formulas that are ready.
pub fn prepare(this: &Desktop, cx: &mut Context<Desktop>) {
    let enabled = this.settings.appearance.render_math;
    let desktop = cx.entity().downgrade();
    if let Some(cache) = cx.try_global::<Cache>()
        && cache.enabled == enabled
    {
        return;
    }
    if !cx.has_global::<Cache>() {
        cx.set_global(Cache {
            entries: HashMap::new(),
            clock: 0,
            jobs: None,
            runtime: this.backend.runtime().clone(),
            desktop,
            enabled,
            scale: 1.,
        });
    } else {
        cx.update_global::<Cache, _>(|cache, _| cache.enabled = enabled);
    }
}

/// The scale factor formulas are painted at; display formulas are looked
/// up at the last one seen.
pub fn painted_at(scale: f32, cx: &mut App) {
    if cx
        .try_global::<Cache>()
        .is_some_and(|cache| cache.scale != scale)
    {
        cx.update_global::<Cache, _>(|cache, _| cache.scale = scale);
    }
}

fn last_scale(cx: &App) -> f32 {
    cx.try_global::<Cache>().map_or(1., |cache| cache.scale)
}

/// A formula at `em` logical pixels per em and `scale` device pixels per
/// logical one: rendered, or asked for. While the exact size is on its way,
/// another size of the same formula stands in.
pub fn lookup(tex: &SharedString, display: bool, em: Pixels, scale: f32, cx: &mut App) -> Shown {
    let Some(cache) = cx.try_global::<Cache>() else {
        return Shown::Off;
    };
    if !cache.enabled {
        return Shown::Off;
    }
    let size =
        (f32::from(em) * scale).clamp(*nuschel_math_sizes().start(), *nuschel_math_sizes().end());
    let key = Key {
        tex: tex.clone(),
        display,
        size: (size * 4.).round() as u32,
    };
    let mut ask = false;
    let shown = cx.update_global::<Cache, _>(|cache, _| {
        cache.clock += 1;
        let clock = cache.clock;
        match cache.entries.get_mut(&key) {
            Some((entry, used)) => {
                *used = clock;
                match entry {
                    Entry::Ready(formula) => return Shown::Formula(formula.clone()),
                    Entry::Failed(reason) => return Shown::Failed(reason.clone()),
                    Entry::Pending => {}
                }
            }
            None => {
                cache.entries.insert(key.clone(), (Entry::Pending, clock));
                ask = true;
            }
        }
        // Another size of the same formula, until this one is ready.
        cache
            .entries
            .iter()
            .filter(|(other, _)| other.tex == key.tex && other.display == display)
            .find_map(|(_, (entry, _))| match entry {
                Entry::Ready(formula) => Some(Shown::Formula(formula.clone())),
                _ => None,
            })
            .unwrap_or(Shown::Pending)
    });
    if ask && !request(key.clone(), size, cx) {
        // The queue is full: forget it, so a later frame asks again.
        cx.update_global::<Cache, _>(|cache, _| {
            cache.entries.remove(&key);
        });
    }
    shown
}

/// The sizes the math worker renders at (`nuschel_math::SIZES`).
fn nuschel_math_sizes() -> std::ops::RangeInclusive<f32> {
    4.0..=128.0
}

/// Send a formula to the worker; the desktop redraws when it is back.
fn request(key: Key, size: f32, cx: &mut App) -> bool {
    let (reply, answer) = oneshot::channel();
    let job = Job {
        request: MathRequest {
            tex: key.tex.to_string(),
            display: key.display,
            size,
        },
        reply,
    };
    let sent = cx.update_global::<Cache, _>(|cache, _| {
        let jobs = cache.jobs.get_or_insert_with(|| {
            let (jobs, queue) = mpsc::channel(QUEUE);
            cache.runtime.spawn(serve(queue));
            jobs
        });
        jobs.try_send(job).is_ok()
    });
    if !sent {
        return false;
    }
    cx.spawn(async move |cx| {
        let result = answer
            .await
            .unwrap_or_else(|_| Err("the math worker stopped".into()));
        cx.update(|cx| {
            let desktop = cx.update_global::<Cache, _>(|cache, _| {
                let entry = match result.map(|image| Formula::new(image, size)) {
                    Ok(Some(formula)) => Entry::Ready(Rc::new(formula)),
                    Ok(None) => Entry::Failed("the math worker answered wrongly".into()),
                    Err(reason) => Entry::Failed(reason.into()),
                };
                if let Some((slot, _)) = cache.entries.get_mut(&key) {
                    *slot = entry;
                }
                evict(cache);
                cache.desktop.clone()
            });
            let _ = desktop.update(cx, |_, cx| cx.notify());
        });
    })
    .detach();
    true
}

/// Keep the most recently shown formulas.
fn evict(cache: &mut Cache) {
    if cache.entries.len() <= CACHE {
        return;
    }
    let mut used: Vec<u64> = cache
        .entries
        .values()
        .filter(|(entry, _)| !matches!(entry, Entry::Pending))
        .map(|(_, used)| *used)
        .collect();
    used.sort_unstable();
    let cutoff = used.get(used.len().saturating_sub(CACHE / 2)).copied();
    if let Some(cutoff) = cutoff {
        cache
            .entries
            .retain(|_, (entry, used)| matches!(entry, Entry::Pending) || *used >= cutoff);
    }
}

/// The math worker's loop on the tokio runtime: one formula at a time,
/// stopping the worker when idle.
async fn serve(mut jobs: mpsc::Receiver<Job>) {
    let executable = nuschel_session::workers::sibling("nuschel-math-worker")
        .unwrap_or_else(|_| "nuschel-math-worker".into());
    let mut worker = MathWorker::new(executable);
    loop {
        let job = if worker.running() {
            match tokio::time::timeout(IDLE, jobs.recv()).await {
                Ok(Some(job)) => job,
                Ok(None) => break,
                Err(_) => {
                    worker.stop().await;
                    continue;
                }
            }
        } else {
            match jobs.recv().await {
                Some(job) => job,
                None => break,
            }
        };
        let result = worker
            .render(&job.request)
            .await
            .map_err(|error| match error {
                nuschel_ipc::host::Error::Rejected(reason) => reason,
                nuschel_ipc::host::Error::Timeout => "it took too long".into(),
                nuschel_ipc::host::Error::MissingSandboxHelper(helper) => {
                    format!("{helper} is missing")
                }
                _ => "the math worker failed".into(),
            });
        let _ = job.reply.send(result);
    }
    worker.stop().await;
}

/// A display formula as an element: the formula, centred on its own line,
/// or what stands in for it.
pub fn display(
    index: usize,
    source: &SharedString,
    tex: &SharedString,
    em: Pixels,
    cx: &mut App,
) -> AnyElement {
    let em = em * SCALE;
    let shown = lookup(tex, true, em, last_scale(cx), cx);
    let container = div()
        .id(("math", index))
        .role(Role::Math)
        .aria_label(source.clone());
    match shown {
        Shown::Formula(formula) => {
            let (_, height, _) = formula.metrics(em);
            container
                .w_full()
                .flex()
                .justify_center()
                .overflow_x_hidden()
                .child(FormulaElement {
                    tex: tex.clone(),
                    formula,
                    em,
                    height,
                })
                .into_any_element()
        }
        Shown::Pending => container
            .text_color(crate::theme::color(crate::theme::TEXT_FAINT))
            .child(source.clone())
            .into_any_element(),
        Shown::Off => container.child(source.clone()).into_any_element(),
        Shown::Failed(reason) => failed(container, source, &reason),
    }
}

/// A formula that cannot be shown: its source as code, and why, quietly.
fn failed(container: Stateful<Div>, source: &SharedString, reason: &str) -> AnyElement {
    use crate::theme::{APP, BORDER, TEXT_FAINT, color};
    let hint: SharedString = format!("Formula not shown: {reason}").into();
    container
        .rounded(px(8.))
        .bg(color(APP))
        .border_1()
        .border_color(color(BORDER))
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .font_family(crate::theme::mono_font())
                .text_sm()
                .child(source.clone()),
        )
        .child(div().text_xs().text_color(color(TEXT_FAINT)).child(hint))
        .into_any_element()
}

/// A rendered formula, laid out at its logical size and painted in the
/// text colour, sharp at the window's scale (asked for anew when the scale
/// changes; the old size stands in meanwhile).
struct FormulaElement {
    tex: SharedString,
    formula: Rc<Formula>,
    em: Pixels,
    height: Pixels,
}

impl IntoElement for FormulaElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for FormulaElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let (width, height, _) = self.formula.metrics(self.em);
        let style = Style {
            size: size(width.into(), height.into()),
            flex_shrink: 0.,
            ..Default::default()
        };
        self.height = height;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let scale = window.scale_factor();
        painted_at(scale, cx);
        // At another scale than it was rendered for: ask for this one.
        let formula = match lookup(&self.tex, true, self.em, scale, cx) {
            Shown::Formula(formula) => formula,
            _ => self.formula.clone(),
        };
        let color = window.text_style().color;
        if let Some(image) = formula.image(color) {
            let _ = window.paint_image(bounds, bounds, Corners::default(), image, 0, false);
        }
    }
}

/// Inline formulas of one text block placed in its text: each rendered one
/// is replaced by non-breaking spaces as wide as it is, and painted over
/// them on the text's baseline.
pub struct Placed {
    /// The formula's text as written, in the block's text.
    pub source: Range<usize>,
    /// Its stand-in in the laid-out text.
    pub shown: Range<usize>,
    pub formula: Rc<Formula>,
    pub width: Pixels,
}

/// The laid-out text of a block with inline formulas, and the formulas'
/// places in it; `failed` and `pending` are source ranges shown as text.
pub struct Placement {
    pub text: String,
    pub placed: Vec<Placed>,
    pub failed: Vec<Range<usize>>,
    pub pending: Vec<Range<usize>>,
    /// The line height the tallest formula needs, if more than the text's.
    pub line_height: Option<Pixels>,
    /// From a line's top to its baseline, at that line height.
    pub baseline: Pixels,
    /// The formulas' size, logical pixels per em.
    pub em: Pixels,
}

/// Place the inline formulas of `text` for laying it out in the window's
/// current text style.
pub fn place(text: &str, math: &[InlineMath], window: &mut Window, cx: &mut App) -> Placement {
    let style = window.text_style();
    let em = style.font_size.to_pixels(window.rem_size());
    let math_em = em * SCALE;
    let scale = window.scale_factor();
    painted_at(scale, cx);
    let text_system = window.text_system().clone();
    let font_id = text_system.resolve_font(&style.font());
    let space = |c: char| {
        text_system
            .advance(font_id, em, c)
            .map(|size| size.width)
            .ok()
            .filter(|width| *width > px(0.1))
    };
    let (nbsp, narrow) = (space('\u{a0}').unwrap_or(em * 0.25), space('\u{202f}'));
    let normal = style.line_height_in_pixels(window.rem_size());
    let (ascent, descent) = (
        text_system.ascent(font_id, em),
        text_system.descent(font_id, em),
    );
    let mut placement = Placement {
        text: String::with_capacity(text.len()),
        placed: Vec::new(),
        failed: Vec::new(),
        pending: Vec::new(),
        line_height: None,
        baseline: px(0.),
        em: math_em,
    };
    let mut line_height = normal;
    let mut done = 0;
    for formula in math {
        let range = formula.range.clone();
        if range.start < done
            || range.end > text.len()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            continue;
        }
        match lookup(&formula.tex, false, math_em, scale, cx) {
            Shown::Formula(rendered) => {
                let (width, height, baseline) = rendered.metrics(math_em);
                placement.text.push_str(&text[done..range.start]);
                let start = placement.text.len();
                let shown = spaces(width, nbsp, narrow);
                placement.text.push_str(&shown.0);
                placement.placed.push(Placed {
                    source: range.clone(),
                    shown: start..placement.text.len(),
                    formula: rendered,
                    width: shown.1,
                });
                done = range.end;
                // Room above and below the baseline for the formula, with
                // the text centred in the line as GPUI does.
                line_height = line_height
                    .max(baseline * 2. - ascent + descent)
                    .max((height - baseline) * 2. + ascent - descent);
            }
            Shown::Failed(_) => placement.failed.push(range),
            Shown::Pending => placement.pending.push(range),
            Shown::Off => {}
        }
    }
    placement.text.push_str(&text[done..]);
    let line_height = line_height.ceil();
    if line_height > normal {
        placement.line_height = Some(line_height);
    }
    placement.baseline = (line_height - ascent - descent) / 2. + ascent;
    placement
}

/// Non-breaking spaces at least `width` wide, as few too many as can be:
/// no-break spaces, then narrow ones. Returns them and their width.
fn spaces(width: Pixels, nbsp: Pixels, narrow: Option<Pixels>) -> (String, Pixels) {
    let whole = (width / nbsp).ceil().max(1.) as usize;
    let mut best = (whole, 0, nbsp * whole as f32);
    if let Some(narrow) = narrow.filter(|narrow| *narrow < nbsp) {
        for wide in whole.saturating_sub(4)..whole {
            let rest = width - nbsp * wide as f32;
            let narrows = (rest / narrow).ceil().max(0.) as usize;
            let total = nbsp * wide as f32 + narrow * narrows as f32;
            if total >= width && total < best.2 && wide + narrows > 0 {
                best = (wide, narrows, total);
            }
        }
    }
    let mut text = "\u{a0}".repeat(best.0);
    text.push_str(&"\u{202f}".repeat(best.1));
    (text, best.2)
}

#[cfg(test)]
mod tests {
    use super::spaces;
    use gpui_kit::px;

    #[test]
    fn stand_ins_are_as_wide_as_the_formula_and_barely_wider() {
        let (text, width) = spaces(px(20.), px(4.), Some(px(3.)));
        assert!(width >= px(20.) && width < px(23.), "{width:?}");
        assert!(text.chars().all(|c| c == '\u{a0}' || c == '\u{202f}'));
        let (text, width) = spaces(px(10.5), px(4.), None);
        assert_eq!((text.chars().count(), width), (3, px(12.)));
        // Something always stands in.
        assert_eq!(spaces(px(0.), px(4.), None).0.chars().count(), 1);
    }
}
