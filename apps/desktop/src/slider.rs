// SPDX-License-Identifier: EUPL-1.2
//! Continuous sliders: the value scales, the keyboard and pointer rules, the
//! settings a slider may edit, and when its edits are heard and saved.
//!
//! Volumes keep their stored meaning (linear amplitude, 0 = muted, 1 = as sent,
//! at most [`MAX_GAIN`]); only the slider's position follows a cubic taper,
//! the curve PulseAudio uses for its software volume, so equal slider travel
//! sounds roughly equally loud at every level. The taper is continuous down to
//! silence and exactly invertible for every stored value.
//!
//! Keys on a focused slider: Left/Down and Right/Up move 1 % of the travel,
//! Page Down/Up 10 %, Home and End go to the ends, and Delete or 0 resets
//! to the default (100 % for a volume, the default sensitivity).
//!
//! A drag is heard while it moves and saved once when it ends; keys are heard
//! at once and saved after half a second without another (see [`Edits`]).
use crate::{state::Desktop, theme::*};
use gpui_kit::{prelude::*, *};
use nuschl_session::config::{ClientSettings, ManagedSettings};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

/// The largest volume a person or server can be set to (unchanged).
pub const MAX_GAIN: f32 = 4.;

/// Pointer drags settle on unity when they pass within this many decibels.
const UNITY_DETENT_DB: f32 = 0.5;

/// Keyboard step in slider travel; Page Up/Down move ten of these.
const FINE_STEP: f32 = 0.01;

/// How a slider maps its 0..=1 travel to the value it edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    /// A linear amplitude gain from 0 to [`MAX_GAIN`] on a cubic taper.
    Gain,
    /// A linear amplitude from 0 to 1 on the same taper (sound effects).
    Amplitude,
    /// A fraction 0..=1, linear (voice activity sensitivity).
    Fraction,
}

impl Scale {
    /// The largest value.
    pub fn max(self) -> f32 {
        match self {
            Scale::Gain => MAX_GAIN,
            Scale::Amplitude | Scale::Fraction => 1.,
        }
    }

    /// Where `value` sits on the slider, 0..=1.
    pub fn position(self, value: f32) -> f32 {
        let value = if value.is_finite() {
            value.clamp(0., self.max())
        } else {
            0.
        };
        match self {
            Scale::Gain => (value / MAX_GAIN).cbrt(),
            Scale::Amplitude => value.cbrt(),
            Scale::Fraction => value,
        }
    }

    /// The value at slider `position`; the ends are exact.
    pub fn value(self, position: f32) -> f32 {
        let position = if position.is_finite() {
            position.clamp(0., 1.)
        } else {
            0.
        };
        if position >= 1. {
            return self.max();
        }
        match self {
            Scale::Gain => MAX_GAIN * position * position * position,
            Scale::Amplitude => position * position * position,
            Scale::Fraction => position,
        }
    }

    /// The value for a pointer at `position`: like [`Scale::value`], but a
    /// gain within half a decibel of unity settles exactly on it.
    pub fn pointer_value(self, position: f32) -> f32 {
        let value = self.value(position);
        if self == Scale::Gain && value > 0. && (20. * value.log10()).abs() < UNITY_DETENT_DB {
            1.
        } else {
            value
        }
    }

    /// `steps` keyboard steps from `value` (positive is louder or more): fine
    /// steps are 1 % of the travel. Unity is a stop, so stepping across it
    /// lands exactly on 1, and both ends are exact.
    pub fn step(self, value: f32, steps: i32) -> f32 {
        let from = self.position(value);
        let to = (from + steps as f32 * FINE_STEP).clamp(0., 1.);
        if self == Scale::Gain {
            let unity = self.position(1.);
            let crosses = (value < 1. && to >= unity) || (value > 1. && to <= unity);
            if crosses {
                return 1.;
            }
        }
        if to <= 0. { 0. } else { self.value(to) }
    }

    /// What a screen reader and the label say for `value`.
    pub fn text(self, value: f32) -> String {
        match self {
            Scale::Gain | Scale::Amplitude if value <= 0. => "Muted".into(),
            Scale::Gain | Scale::Amplitude => {
                format!("{:.0} % ({:+.1} dB)", value * 100., 20. * value.log10())
            }
            Scale::Fraction => format!("{:.0} %", value * 100.),
        }
    }
}

/// What a key does to a slider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Arrows: one fine step; Page Up/Down: ten.
    Steps(i32),
    /// Home: the smallest value (muted for a volume).
    Min,
    /// End: the largest value.
    Max,
    /// Delete or 0: the setting's default.
    Reset,
}

impl Key {
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "right" | "up" => Some(Key::Steps(1)),
            "left" | "down" => Some(Key::Steps(-1)),
            "pageup" => Some(Key::Steps(10)),
            "pagedown" => Some(Key::Steps(-10)),
            "home" => Some(Key::Min),
            "end" => Some(Key::Max),
            "delete" | "0" => Some(Key::Reset),
            _ => None,
        }
    }

    /// The value after this key on `scale`, whose default is `default`.
    pub fn apply(self, scale: Scale, value: f32, default: f32) -> f32 {
        match self {
            Key::Steps(steps) => scale.step(value, steps),
            Key::Min => 0.,
            Key::Max => scale.max(),
            Key::Reset => default,
        }
    }
}

/// The settings a slider can change without saving. Nothing else can be
/// previewed: none of these is read by the people preferences, the shortcuts
/// or the relay rule, so a preview needs none of `Desktop::update_settings`'
/// side effects, and the managed values are put back as there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    /// A person's playback volume, by certificate hash.
    UserVolume(String),
    /// A server's playback volume, by bookmark id.
    ServerVolume(String),
    /// Voice activity sensitivity.
    Sensitivity,
    /// Sound effect volume.
    SoundVolume,
}

impl Setting {
    pub fn scale(&self) -> Scale {
        match self {
            Setting::UserVolume(_) | Setting::ServerVolume(_) => Scale::Gain,
            Setting::SoundVolume => Scale::Amplitude,
            Setting::Sensitivity => Scale::Fraction,
        }
    }

    pub fn get(&self, settings: &ClientSettings) -> f32 {
        match self {
            Setting::UserVolume(hash) => settings.volume_for(Some(hash)),
            Setting::ServerVolume(id) => settings.server_volume(id),
            Setting::Sensitivity => settings.audio.vad_sensitivity,
            Setting::SoundVolume => settings.sound_volume,
        }
    }

    /// Not a number (NaN, infinity) sets nothing.
    pub fn set(&self, settings: &mut ClientSettings, value: f32) {
        if !value.is_finite() {
            return;
        }
        let value = value.clamp(0., self.scale().max());
        match self {
            Setting::UserVolume(hash) => {
                settings.user_volumes.insert(hash.clone(), value);
            }
            Setting::ServerVolume(id) => {
                settings.server_volumes.insert(id.clone(), value);
            }
            Setting::Sensitivity => settings.audio.vad_sensitivity = value,
            Setting::SoundVolume => settings.sound_volume = value,
        }
    }

    /// What Delete or 0 sets.
    pub fn default_value(&self) -> f32 {
        match self {
            Setting::UserVolume(_) | Setting::ServerVolume(_) => 1.,
            Setting::Sensitivity => ClientSettings::default().audio.vad_sensitivity,
            Setting::SoundVolume => ClientSettings::default().sound_volume,
        }
    }

    /// Its key in managed.json (`ManagedSettings::contains`).
    pub fn managed_key(&self) -> String {
        match self {
            Setting::UserVolume(hash) => format!("user_volumes.{hash}"),
            Setting::ServerVolume(id) => format!("server_volumes.{id}"),
            Setting::Sensitivity => "audio.vad_sensitivity".into(),
            Setting::SoundVolume => "sound_volume".into(),
        }
    }

    /// Whether the media worker must be told (a Configure): the gate's
    /// sensitivity. Volumes are read per packet or sound from the settings.
    pub fn reaches_worker(&self) -> bool {
        matches!(self, Setting::Sensitivity)
    }

    /// The value as the label and screen readers say it.
    pub fn readout(&self, value: f32) -> String {
        match self {
            Setting::Sensitivity => format!(
                "{:.0} % · opens above {:.0} % speech probability",
                value * 100.,
                nuschl_core::audio::gate_thresholds(value).open * 100.
            ),
            _ => self.scale().text(value),
        }
    }
}

/// `settings` with `setting` at `value`, as a preview makes them: sanitised,
/// with the managed values back in place. None when that changes nothing, and
/// for a value that is not a number, before anything is touched: sanitising
/// would drop a NaN volume, putting the person or server back to 100 %.
pub fn previewed(
    settings: &ClientSettings,
    managed: &ManagedSettings,
    setting: &Setting,
    value: f32,
) -> Option<ClientSettings> {
    if !value.is_finite() {
        return None;
    }
    let mut next = settings.clone();
    setting.set(&mut next, value);
    let next = managed.apply(next.sanitized());
    (next != *settings).then_some(next)
}

/// Where a slider is shown, by the invocation of the menu or of Settings that
/// shows it: each is new each time one opens. A slider acts only while that
/// same opening shows it, so a callback kept from an earlier one (a drag, a
/// key, an assistive technology's request) never acts in a later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Menu(u64),
    Settings(u64),
}

impl Owner {
    /// Whether this owner is showing: `menu` and `settings` are the
    /// invocations of the open menu and of open Settings.
    pub fn alive(self, menu: Option<u64>, settings: Option<u64>) -> bool {
        match self {
            Owner::Menu(invocation) => menu == Some(invocation),
            Owner::Settings(invocation) => settings == Some(invocation),
        }
    }
}

/// One slider: where it is and what it edits, fixed when it was drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub owner: Owner,
    pub setting: Setting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    Pointer,
    Keyboard,
}

/// The media worker hears a changed sensitivity at most this often.
pub const COALESCE: Duration = Duration::from_millis(100);
/// Keyboard edits are saved after this long without another.
pub const KEY_IDLE: Duration = Duration::from_millis(500);
/// How often pending edits are looked at (see `Edits::tick`).
pub const TICK: Duration = Duration::from_millis(100);

/// What the desktop does after an edit event: send the settings in memory
/// to the driver (the media path hears them), save them to disk, or both.
/// Neither names a person, a server or a value: a commit writes what memory
/// already holds, so a pending commit can never apply to another target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub driver: bool,
    pub save: bool,
}

/// Slider edits that memory holds but the disk (or the media worker) does
/// not yet. A drag is previewed on every move and committed once when it
/// ends; a key is previewed at once and committed after [`KEY_IDLE`]; a
/// sensitivity reaches the worker at most every [`COALESCE`], the last one
/// always. Owners that close commit what they previewed.
#[derive(Debug, Default)]
pub struct Edits {
    /// Memory differs from the disk.
    unsaved: bool,
    /// The worker has not heard the newest sensitivity.
    worker_behind: bool,
    worker_sent: Option<Instant>,
    last_key: Option<Instant>,
    /// The slider the pointer holds.
    drag: Option<Target>,
    /// A settle loop is running (see `Desktop::settle_settings_soon`).
    armed: bool,
}

impl Edits {
    /// The pointer pressed on `target`. A drag whose release was never seen
    /// is committed first.
    pub fn press(&mut self, target: Target, now: Instant) -> Effects {
        let effects = if self.drag.is_some() {
            self.commit(now)
        } else {
            Effects::default()
        };
        self.drag = Some(target);
        effects
    }

    /// Whether the pointer holds `target`'s slider.
    pub fn dragging(&self, target: &Target) -> bool {
        self.drag.as_ref() == Some(target)
    }

    /// The pointer let go of `target`'s slider: commit, once.
    pub fn release(&mut self, target: &Target, now: Instant) -> Effects {
        if self.dragging(target) {
            self.commit(now)
        } else {
            Effects::default()
        }
    }

    /// Memory now holds a new value; `reaches_worker` as [`Setting::reaches_worker`].
    pub fn changed(&mut self, reaches_worker: bool, input: Input, now: Instant) -> Effects {
        self.unsaved = true;
        if input == Input::Keyboard {
            self.last_key = Some(now);
        }
        let due = self
            .worker_sent
            .is_none_or(|sent| now.saturating_duration_since(sent) >= COALESCE);
        if reaches_worker && !due {
            self.worker_behind = true;
            return Effects::default();
        }
        if reaches_worker || self.worker_behind {
            self.worker_sent = Some(now);
        }
        self.worker_behind = false;
        Effects {
            driver: true,
            save: false,
        }
    }

    /// Save what memory holds, after the worker has heard the last of it.
    pub fn commit(&mut self, now: Instant) -> Effects {
        let effects = Effects {
            driver: std::mem::take(&mut self.worker_behind),
            save: std::mem::take(&mut self.unsaved),
        };
        if effects.driver {
            self.worker_sent = Some(now);
        }
        self.drag = None;
        self.last_key = None;
        effects
    }

    /// Start a settle loop unless one runs: true when the caller should.
    pub fn arm(&mut self) -> bool {
        !std::mem::replace(&mut self.armed, true)
    }

    /// Whether anything is still pending; the settle loop stops when not.
    pub fn armed(&self) -> bool {
        self.armed
    }

    /// Ten times a second while edits are pending: a held sensitivity reaches
    /// the worker, a drag whose owner closed is committed, and keys are
    /// committed once idle. `alive` says whether an owner still shows.
    pub fn tick(&mut self, now: Instant, alive: impl Fn(Owner) -> bool) -> Effects {
        let mut effects = Effects::default();
        if self.drag.as_ref().is_some_and(|drag| !alive(drag.owner)) {
            effects = self.commit(now);
        }
        if self.worker_behind
            && self
                .worker_sent
                .is_none_or(|sent| now.saturating_duration_since(sent) >= COALESCE)
        {
            self.worker_behind = false;
            self.worker_sent = Some(now);
            effects.driver = true;
        }
        let idle = self
            .last_key
            .is_none_or(|key| now.saturating_duration_since(key) >= KEY_IDLE);
        if self.drag.is_none() && self.unsaved && idle {
            let commit = self.commit(now);
            effects.driver |= commit.driver;
            effects.save |= commit.save;
        }
        self.armed = self.unsaved || self.worker_behind || self.drag.is_some();
        effects
    }
}

impl Desktop {
    /// The invocations of the open menu and of open Settings.
    pub(crate) fn slider_owners(&self) -> (Option<u64>, Option<u64>) {
        (
            self.menu.as_ref().map(|menu| menu.invocation),
            self.settings_open.then_some(self.settings_invocation),
        )
    }

    /// Whether `target`'s owner still shows it.
    fn slider_live(&self, target: &Target) -> bool {
        let (menu, settings) = self.slider_owners();
        target.owner.alive(menu, settings)
    }

    fn slider_press(&mut self, target: &Target, value: f32, cx: &mut Context<Self>) {
        if !self.slider_live(target) {
            return;
        }
        let effects = self.slider_edits.press(target.clone(), Instant::now());
        self.apply_slider_effects(effects);
        self.preview_setting(&target.setting, value, Input::Pointer, cx);
    }

    fn slider_drag(&mut self, target: &Target, value: f32, cx: &mut Context<Self>) {
        if self.slider_live(target) && self.slider_edits.dragging(target) {
            self.preview_setting(&target.setting, value, Input::Pointer, cx);
        }
    }

    /// True when it ended a drag.
    fn slider_release(&mut self, target: &Target) -> bool {
        if !self.slider_edits.dragging(target) {
            return false;
        }
        let effects = self.slider_edits.release(target, Instant::now());
        self.apply_slider_effects(effects);
        true
    }

    /// A key or an assistive technology's request: `change` maps the current value.
    fn slider_step(
        &mut self,
        target: &Target,
        change: impl FnOnce(f32) -> f32,
        cx: &mut Context<Self>,
    ) {
        if self.slider_live(target) {
            let value = change(target.setting.get(&self.settings));
            self.preview_setting(&target.setting, value, Input::Keyboard, cx);
        }
    }
}

/// What a slider shows and edits.
pub struct Spec {
    pub id: SharedString,
    /// For screen readers.
    pub name: SharedString,
    pub target: Target,
    pub value: f32,
    /// Set by the system configuration: shown, never changed.
    pub read_only: bool,
}

/// The slider the pointer holds. Every slider hears every drag of this type,
/// so each checks that the drag is its own.
struct Dragging(Target);

/// The thumb's diameter; the track is inset by half of it at both ends.
const THUMB: f32 = 16.;

/// A continuous slider (role Slider, keyboard and pointer, see the module).
pub fn slider(spec: Spec, cx: &mut Context<Desktop>) -> AnyElement {
    let Spec {
        id,
        name,
        target,
        value,
        read_only,
    } = spec;
    let scale = target.setting.scale();
    let position = scale.position(value);
    let text = target.setting.readout(value);
    // Values are announced in percent: of amplitude for volumes.
    let percent = |value: f32| f64::from(value) * 100.;
    // The travel of one key press here (the taper makes it vary).
    let step = (scale.step(value, 1) - value).max(value - scale.step(value, -1));
    // The track as last painted, from which a pointer's x is a position.
    let track = Rc::new(Cell::new(Bounds::<Pixels>::default()));
    let painted = track.clone();
    let at = move |x: Pixels| {
        let bounds = track.get();
        let travel = (bounds.size.width - px(THUMB)).max(px(1.));
        scale.pointer_value((x - bounds.left() - px(THUMB / 2.)) / travel)
    };
    let fill = color(if read_only { TEXT_FAINT } else { ACCENT });
    let mut control = div()
        .id(id.clone())
        .role(Role::Slider)
        .aria_label(name)
        .aria_numeric_value(percent(value))
        .aria_min_numeric_value(0.)
        .aria_max_numeric_value(percent(scale.max()))
        .aria_numeric_value_step(percent(step))
        .aria_value(text.clone())
        // AT-SPI has no value text: the description carries it there.
        .aria_description(text)
        .tab_index(0)
        .relative()
        .w_full()
        .min_w(px(96.))
        .h(px(24.))
        .rounded(px(6.))
        .border_2()
        .border_color(transparent_black())
        .focus_visible(|style| style.border_color(color(ACCENT)))
        .child(
            canvas(move |bounds, _, _| painted.set(bounds), |_, _, _, _| {})
                .absolute()
                .size_full(),
        )
        .child(
            div()
                .absolute()
                .top(px(7.))
                .left(px(THUMB / 2.))
                .right(px(THUMB / 2.))
                .h(px(6.))
                .rounded_full()
                .bg(color(BORDER_STRONG))
                .child(div().h_full().w(relative(position)).rounded_full().bg(fill)),
        )
        .child(
            div()
                .absolute()
                .top(px(2.))
                .left(px(THUMB / 2.))
                .right(px(THUMB / 2.))
                .h(px(THUMB))
                .child(
                    div()
                        .absolute()
                        .left(relative(position))
                        .ml(px(-THUMB / 2.))
                        .size(px(THUMB))
                        .rounded_full()
                        .bg(color(WHITE))
                        .border_2()
                        .border_color(fill)
                        .shadow_sm(),
                ),
        );
    if !read_only {
        let entity = cx.entity().downgrade();
        // Assistive technology's Increment, Decrement and SetValue act like keys.
        type Change = Rc<dyn Fn(f32, Option<&accesskit::ActionData>) -> Option<f32>>;
        let action = |change: Change| {
            let (entity, target) = (entity.clone(), target.clone());
            move |data: Option<&accesskit::ActionData>, _: &mut Window, cx: &mut App| {
                let data = data.cloned();
                entity
                    .update(cx, |this, cx| {
                        this.slider_step(
                            &target,
                            |current| change(current, data.as_ref()).unwrap_or(current),
                            cx,
                        );
                        cx.notify();
                    })
                    .ok();
            }
        };
        let default = target.setting.default_value();
        let (key, press, moved, up, out) = (
            target.clone(),
            target.clone(),
            target.clone(),
            target.clone(),
            target.clone(),
        );
        let (press_at, move_at) = (at.clone(), at);
        control = control
            .cursor_pointer()
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                let Some(pressed) = Key::from_key(&event.keystroke.key) else {
                    return;
                };
                this.slider_step(&key, |value| pressed.apply(scale, value, default), cx);
                cx.notify();
                cx.stop_propagation();
            }))
            .on_a11y_action(
                AccessibleAction::Increment,
                action(Rc::new(move |value, _| Some(scale.step(value, 1)))),
            )
            .on_a11y_action(
                AccessibleAction::Decrement,
                action(Rc::new(move |value, _| Some(scale.step(value, -1)))),
            )
            .on_a11y_action(
                AccessibleAction::SetValue,
                action(Rc::new(move |_, data| match data {
                    // Not a number is no value (see `previewed`).
                    Some(accesskit::ActionData::NumericValue(percent)) => {
                        Some(*percent as f32 / 100.).filter(|value| value.is_finite())
                    }
                    _ => None,
                })),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.slider_press(&press, press_at(event.position.x), cx);
                    cx.notify();
                }),
            )
            .on_drag(Dragging(target.clone()), |_, _, _, cx| {
                cx.new(|_| EmptyView)
            })
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<Dragging>, _, cx| {
                    if event.drag(cx).0 != moved {
                        return;
                    }
                    if event.event.pressed_button == Some(MouseButton::Left) {
                        this.slider_drag(&moved, move_at(event.event.position.x), cx);
                    } else {
                        // The release went elsewhere (the pointer left the window).
                        this.slider_release(&moved);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.slider_release(&up) {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.slider_release(&out) {
                        cx.notify();
                    }
                }),
            );
    }
    crate::layout::focusable(id, control)
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's `test` attribute would replace the standard one.
    use super::{
        ClientSettings, Edits, Effects, Input, Key, MAX_GAIN, ManagedSettings, Owner, Scale,
        Setting, TICK, Target, previewed,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn the_gain_taper_is_exact_at_mute_unity_and_maximum() {
        assert_eq!(Scale::Gain.value(0.), 0.);
        assert_eq!(Scale::Gain.value(1.), MAX_GAIN);
        assert_eq!(Scale::Gain.position(0.), 0.);
        assert_eq!(Scale::Gain.position(MAX_GAIN), 1.);
        // Unity is reachable exactly by keyboard from either side and by pointer.
        let below = Scale::Gain.step(0.5, 0);
        let mut value = below;
        while value < 1. {
            value = Scale::Gain.step(value, 1);
        }
        assert_eq!(value, 1.);
        let mut value = 3.;
        while value > 1. {
            value = Scale::Gain.step(value, -1);
        }
        assert_eq!(value, 1.);
        assert_eq!(
            Scale::Gain.pointer_value(Scale::Gain.position(1.) + 0.001),
            1.
        );
        assert_eq!(Scale::Gain.step(0.2, -1000), 0.);
        assert_eq!(Key::Max.apply(Scale::Gain, 0.2, 1.), MAX_GAIN);
        assert_eq!(Key::Min.apply(Scale::Gain, 3., 1.), 0.);
        assert_eq!(Key::Reset.apply(Scale::Gain, 3., 1.), 1.);
    }

    #[test]
    fn every_stored_gain_maps_back_to_itself() {
        // Old menus stored 0.1 steps; anything from 0 to 4 must round-trip.
        let mut gain = 0.;
        while gain <= MAX_GAIN {
            let back = Scale::Gain.value(Scale::Gain.position(gain));
            assert!((back - gain).abs() <= gain * 1e-5 + 1e-7, "{gain} → {back}");
            gain += 0.003;
        }
        for gain in [1e-6_f32, 0.004, 0.01, 0.1, 0.25, 1., 2.5, 4.] {
            let back = Scale::Gain.value(Scale::Gain.position(gain));
            assert!((back - gain).abs() <= gain * 1e-5, "{gain} → {back}");
        }
    }

    #[test]
    fn the_taper_is_continuous_and_monotonic_down_to_silence() {
        let mut previous = Scale::Gain.value(0.);
        for step in 1..=10_000 {
            let value = Scale::Gain.value(step as f32 / 10_000.);
            assert!(value > previous, "not increasing at {step}");
            // No jump: neighbouring positions differ by well under 0.2 % of full scale.
            assert!(value - previous < MAX_GAIN * 0.002, "jump at {step}");
            previous = value;
        }
        // Low positions reach very quiet gains continuously (no −40 dB floor).
        assert!(Scale::Gain.value(0.05) < 0.001);
    }

    #[test]
    fn fine_steps_move_the_knob_evenly_and_coarse_ones_ten_times_further() {
        let fine = Scale::Gain.position(Scale::Gain.step(0.3, 1)) - Scale::Gain.position(0.3);
        let coarse = Scale::Gain.position(Scale::Gain.step(0.3, 10)) - Scale::Gain.position(0.3);
        assert!((fine - 0.01).abs() < 1e-5);
        assert!((coarse - 0.1).abs() < 1e-5);
        assert!((Scale::Fraction.step(0.5, 1) - 0.51).abs() < 1e-6);
        assert_eq!(Scale::Fraction.step(0.995, 1), 1.);
        assert_eq!(Scale::Fraction.step(0.005, -1), 0.);
    }

    #[test]
    fn readouts_say_mute_percent_and_decibels() {
        assert_eq!(Scale::Gain.text(0.), "Muted");
        assert_eq!(Scale::Gain.text(1.), "100 % (+0.0 dB)");
        assert_eq!(Scale::Gain.text(0.5), "50 % (-6.0 dB)");
        assert_eq!(Scale::Gain.text(4.), "400 % (+12.0 dB)");
        assert_eq!(Scale::Fraction.text(0.42), "42 %");
        for key in [
            "right", "up", "left", "down", "pageup", "pagedown", "home", "end",
        ] {
            assert!(Key::from_key(key).is_some());
        }
        assert_eq!(Key::from_key("a"), None);
        assert_eq!(Key::from_key("pageup"), Some(Key::Steps(10)));
        assert_eq!(Key::from_key("delete"), Some(Key::Reset));
        assert_eq!(Key::from_key("0"), Some(Key::Reset));
    }

    fn user(hash: &str, invocation: u64) -> Target {
        Target {
            owner: Owner::Menu(invocation),
            setting: Setting::UserVolume(hash.into()),
        }
    }

    /// The disk, the media worker and memory, driven by `Edits` as the desktop does.
    #[derive(Default)]
    struct World {
        edits: Edits,
        memory: ClientSettings,
        heard: ClientSettings,
        disk: ClientSettings,
        sends: usize,
        saves: usize,
    }

    impl World {
        fn run(&mut self, effects: Effects) {
            if effects.driver {
                self.heard = self.memory.clone();
                self.sends += 1;
            }
            if effects.save {
                self.disk = self.memory.clone();
                self.saves += 1;
            }
        }
        /// As `Desktop::preview_setting`.
        fn preview(&mut self, setting: &Setting, value: f32, input: Input, now: Instant) {
            let Some(next) = previewed(&self.memory, &ManagedSettings::default(), setting, value)
            else {
                return;
            };
            self.memory = next;
            let effects = self.edits.changed(setting.reaches_worker(), input, now);
            self.run(effects);
        }
        fn tick(&mut self, now: Instant, alive: impl Fn(Owner) -> bool) {
            let effects = self.edits.tick(now, alive);
            self.run(effects);
        }
    }

    const SHOWN: fn(Owner) -> bool = |_| true;

    #[test]
    fn a_drag_is_heard_as_it_moves_and_saved_once_when_it_ends() {
        let mut world = World::default();
        let target = user("ada", 7);
        let start = Instant::now();
        let effects = world.edits.press(target.clone(), start);
        world.run(effects);
        for step in 0..200 {
            let now = start + Duration::from_millis(step * 4);
            world.preview(&target.setting, step as f32 / 100., Input::Pointer, now);
            // Volumes are heard at once, every move.
            assert_eq!(world.heard.volume_for(Some("ada")), step as f32 / 100.);
            if step % 25 == 0 {
                world.tick(now, SHOWN);
            }
        }
        assert_eq!(
            world.saves, 0,
            "nothing is saved while the pointer holds the slider"
        );
        assert!(world.edits.dragging(&target));
        let effects = world.edits.release(&target, start + Duration::from_secs(1));
        world.run(effects);
        assert_eq!(world.saves, 1);
        assert_eq!(world.disk.volume_for(Some("ada")), 1.99);
        // Later ticks and stray releases save nothing more.
        let effects = world.edits.release(&target, start + Duration::from_secs(2));
        world.run(effects);
        world.tick(start + Duration::from_secs(3), SHOWN);
        assert_eq!(world.saves, 1);
        assert!(!world.edits.armed());
    }

    #[test]
    fn a_dragged_sensitivity_reaches_the_worker_ten_times_a_second_and_always_last() {
        let mut world = World::default();
        let target = Target {
            owner: Owner::Settings(1),
            setting: Setting::Sensitivity,
        };
        let start = Instant::now();
        let effects = world.edits.press(target.clone(), start);
        world.run(effects);
        let mut last = 0.;
        for step in 0..1000_u64 {
            let now = start + Duration::from_millis(step);
            last = (step % 997) as f32 / 1000.;
            world.preview(&target.setting, last, Input::Pointer, now);
            if step % 100 == 50 {
                world.tick(now, SHOWN);
            }
        }
        assert!(world.sends <= 11, "{} sends in one second", world.sends);
        assert_ne!(world.heard.audio.vad_sensitivity, last);
        let effects = world
            .edits
            .release(&target, start + Duration::from_millis(1000));
        world.run(effects);
        assert_eq!(
            world.heard.audio.vad_sensitivity, last,
            "the final value reached the worker"
        );
        assert_eq!(world.disk.audio.vad_sensitivity, last);
        assert_eq!(world.saves, 1);
    }

    #[test]
    fn keys_are_heard_at_once_and_saved_after_half_a_second_idle() {
        let mut world = World::default();
        let setting = Setting::Sensitivity;
        let start = Instant::now();
        // From 51 %: a key to the value memory already holds is no change.
        let value = |press: u64| 0.51 + press as f32 / 100.;
        for press in 0..5_u64 {
            let now = start + Duration::from_millis(press * 20);
            world.preview(&setting, value(press), Input::Keyboard, now);
            world.tick(now, SHOWN);
        }
        // The first key was heard at once; the rest came within 100 ms of it.
        assert_eq!(world.heard.audio.vad_sensitivity, value(0));
        world.tick(start + Duration::from_millis(180), SHOWN);
        assert_eq!(world.heard.audio.vad_sensitivity, value(4));
        assert_eq!(world.saves, 0);
        world.tick(start + Duration::from_millis(500), SHOWN);
        assert_eq!(world.saves, 0, "still within half a second of the last key");
        world.tick(start + Duration::from_millis(600), SHOWN);
        assert_eq!(world.saves, 1);
        assert_eq!(world.disk.audio.vad_sensitivity, value(4));
        assert!(!world.edits.armed());
    }

    #[test]
    fn a_closed_owner_commits_its_drag_and_others_cannot_end_it() {
        let mut world = World::default();
        let ada = user("ada", 3);
        let start = Instant::now();
        let effects = world.edits.press(ada.clone(), start);
        world.run(effects);
        world.preview(&ada.setting, 0.3, Input::Pointer, start);
        // Another menu's slider (or the same person's in a later menu) is not this drag.
        for other in [
            user("grace", 3),
            user("ada", 4),
            Target {
                owner: Owner::Settings(1),
                setting: Setting::SoundVolume,
            },
        ] {
            assert!(!world.edits.dragging(&other));
            let effects = world.edits.release(&other, start);
            world.run(effects);
        }
        assert_eq!(world.saves, 0);
        // The menu closed (another opened): the drag is committed, once.
        world.tick(start + TICK, |owner| owner.alive(Some(4), None));
        assert_eq!(world.saves, 1);
        assert!(!world.edits.dragging(&ada));
        assert_eq!(world.disk.volume_for(Some("ada")), 0.3);
        assert_eq!(world.disk.volume_for(Some("grace")), 1.);
    }

    #[test]
    fn a_press_while_a_drag_was_never_released_commits_that_drag_first() {
        let mut world = World::default();
        let start = Instant::now();
        let ada = user("ada", 1);
        let effects = world.edits.press(ada.clone(), start);
        world.run(effects);
        world.preview(&ada.setting, 2., Input::Pointer, start);
        let grace = user("grace", 2);
        let effects = world.edits.press(grace.clone(), start);
        world.run(effects);
        assert_eq!(world.saves, 1);
        assert!(world.edits.dragging(&grace));
        assert_eq!(world.disk.volume_for(Some("ada")), 2.);
    }

    #[test]
    fn a_stale_menu_owner_is_not_alive() {
        assert!(Owner::Menu(5).alive(Some(5), None));
        assert!(
            !Owner::Menu(5).alive(Some(6), Some(5)),
            "a newer menu is another owner"
        );
        assert!(
            !Owner::Menu(5).alive(None, Some(5)),
            "a closed menu edits no one"
        );
        assert!(Owner::Settings(2).alive(None, Some(2)));
        assert!(
            !Owner::Settings(2).alive(Some(2), None),
            "closed Settings edits no one"
        );
        assert!(
            !Owner::Settings(1).alive(None, Some(2)),
            "reopened Settings is another owner"
        );
    }

    #[test]
    fn reopened_settings_never_resumes_an_old_drag() {
        let mut world = World::default();
        let first = Target {
            owner: Owner::Settings(1),
            setting: Setting::Sensitivity,
        };
        let start = Instant::now();
        let effects = world.edits.press(first.clone(), start);
        world.run(effects);
        world.preview(&first.setting, 0.7, Input::Pointer, start);
        // Escape closes Settings mid-drag: what was heard is saved, once.
        let effects = world.edits.commit(start);
        world.run(effects);
        world.tick(start + TICK, |owner| owner.alive(None, None));
        assert_eq!((world.saves, world.disk.audio.vad_sensitivity), (1, 0.7));
        // Settings opens again: the first opening's slider is stale. The
        // desktop acts on a move only for a live owner it is dragging, on a
        // key only for a live owner.
        let shown = |owner: Owner| owner.alive(None, Some(2));
        assert!(!shown(first.owner) && !world.edits.dragging(&first));
        let effects = world.edits.release(&first, start + TICK * 2);
        world.run(effects);
        world.tick(start + TICK * 3, shown);
        assert_eq!(world.saves, 1, "a stale release saves nothing more");
        // The new opening's slider drags anew.
        let second = Target {
            owner: Owner::Settings(2),
            ..first.clone()
        };
        assert!(shown(second.owner) && !world.edits.dragging(&second));
        let effects = world.edits.press(second.clone(), start + TICK * 4);
        world.run(effects);
        assert!(world.edits.dragging(&second) && !world.edits.dragging(&first));
    }

    #[test]
    fn a_value_that_is_not_a_number_changes_and_saves_nothing() {
        let mut world = World::default();
        world.memory.user_volumes.insert("ada".into(), 2.5);
        world.memory.server_volumes.insert("home".into(), 0.4);
        world.memory.audio.vad_sensitivity = 0.8;
        world.memory.sound_volume = 0.3;
        let before = world.memory.clone();
        let start = Instant::now();
        let settings = [
            Setting::UserVolume("ada".into()),
            Setting::ServerVolume("home".into()),
            Setting::Sensitivity,
            Setting::SoundVolume,
        ];
        for setting in &settings {
            for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut set = before.clone();
                setting.set(&mut set, value);
                assert_eq!(set, before, "{setting:?} set to {value}");
                assert_eq!(
                    previewed(&before, &ManagedSettings::default(), setting, value),
                    None
                );
                world.preview(setting, value, Input::Keyboard, start);
            }
        }
        world.tick(start + Duration::from_secs(1), |_| true);
        assert_eq!(world.memory, before);
        assert_eq!(
            (world.sends, world.saves),
            (0, 0),
            "nothing heard, nothing written"
        );
        assert!(!world.edits.armed());
        // A number still previews.
        for setting in &settings {
            assert!(previewed(&before, &ManagedSettings::default(), setting, 0.2).is_some());
        }
    }

    #[test]
    fn sound_effects_use_the_volume_taper_within_their_own_range() {
        let scale = Setting::SoundVolume.scale();
        assert_eq!(scale, Scale::Amplitude);
        assert_eq!(scale.max(), 1.);
        assert_eq!((scale.value(0.), scale.value(1.)), (0., 1.));
        assert_eq!((scale.position(0.), scale.position(1.)), (0., 1.));
        // Settings stored 5 % steps (0.7 by default): each maps back to itself.
        for step in 0..=20 {
            let volume = step as f32 / 20.;
            let back = scale.value(scale.position(volume));
            assert!(
                (back - volume).abs() <= volume * 1e-5 + 1e-7,
                "{volume} → {back}"
            );
        }
        for volume in [0.7_f32, 0.001, 1e-6, 0.333] {
            let back = scale.value(scale.position(volume));
            assert!((back - volume).abs() <= volume * 1e-5, "{volume} → {back}");
        }
        // The cubic taper, as for volumes: half the travel is −18 dB.
        assert!((scale.value(0.5) - 0.125).abs() < 1e-7);
        let mut previous = 0.;
        for step in 1..=1000 {
            let volume = scale.value(step as f32 / 1000.);
            assert!(volume > previous && volume <= 1.);
            previous = volume;
        }
        // No detent or stop inside its range; the ends are exact.
        assert_eq!(scale.pointer_value(0.999), scale.value(0.999));
        assert_eq!(scale.step(0.995, 10), 1.);
        assert_eq!(scale.step(0.001, -11), 0.);
        assert_eq!(Key::Max.apply(scale, 0.3, 0.7), 1.);
        assert_eq!(Setting::SoundVolume.readout(0.7), "70 % (-3.1 dB)");
        assert_eq!(Setting::SoundVolume.readout(0.), "Muted");
        // Sensitivity alone stays linear.
        assert_eq!(Setting::Sensitivity.scale(), Scale::Fraction);
    }

    #[test]
    fn a_setting_edits_only_its_own_field() {
        let base = ClientSettings::default();
        let cases = [
            (Setting::UserVolume("ada".into()), "user_volumes"),
            (Setting::ServerVolume("home".into()), "server_volumes"),
            (Setting::Sensitivity, "audio"),
            (Setting::SoundVolume, "sound_volume"),
        ];
        for (setting, field) in cases {
            let mut edited = base.clone();
            setting.set(&mut edited, 0.25);
            assert_eq!(setting.get(&edited), 0.25);
            let (before, after) = (
                serde_json::to_value(&base).unwrap(),
                serde_json::to_value(&edited).unwrap(),
            );
            let (before, after) = (before.as_object().unwrap(), after.as_object().unwrap());
            let keys: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
            let changed: Vec<_> = keys
                .into_iter()
                .filter(|key| before.get(*key) != after.get(*key))
                .collect();
            assert_eq!(changed, [field], "{setting:?}");
            // Nothing that preferences, shortcuts or the relay rule read.
            let mut audio = base.audio.clone();
            audio.vad_sensitivity = edited.audio.vad_sensitivity;
            assert_eq!(
                audio, edited.audio,
                "{setting:?} touched other audio settings"
            );
            assert_eq!(setting.reaches_worker(), setting == Setting::Sensitivity);
            let mut reset = edited.clone();
            setting.set(&mut reset, setting.default_value());
            assert_eq!(setting.get(&reset), setting.get(&base));
        }
        // Values outside the scale are clamped, as sanitising would.
        let mut edited = base.clone();
        Setting::ServerVolume("home".into()).set(&mut edited, 9.);
        assert_eq!(edited.server_volume("home"), MAX_GAIN);
        assert_eq!(
            Setting::Sensitivity.readout(0.5),
            "50 % · opens above 60 % speech probability"
        );
        assert_eq!(
            Setting::UserVolume("ada".into()).managed_key(),
            "user_volumes.ada"
        );
    }
}
