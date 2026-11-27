//! `gameinput.dll`: input devices from the host. Gamepads come from XInput
//! (host slots 0-3); the keyboard and the mouse drive one stand-in gamepad
//! (`FALLBACK_BUTTONS`, `MOUSE_BUTTONS`, mouse look on the right stick while
//! the mouse is captured). The keyboard is a device of its own too. A poll
//! thread turns host state into readings (a new one per change) and runs
//! device callbacks on connect and disconnect.
#![allow(non_snake_case)]
// Title code calls these through the objects, passing pointers the real API
// would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use super::xasync::{E_INVALIDARG, E_NOTIMPL};
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleExport, HleModule};

/// The documented errors.
pub const DEVICE_DISCONNECTED: HRESULT = 0x838A_0001_u32 as i32;
pub const DEVICE_NOT_FOUND: HRESULT = 0x838A_0002_u32 as i32;
pub const READING_NOT_FOUND: HRESULT = 0x838A_0003_u32 as i32;
pub const REFERENCE_TOO_OLD: HRESULT = 0x838A_0004_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;

/// Input kinds (bits) our devices have.
const KEYBOARD_KIND: u32 = 0x10;
const GAMEPAD_KIND: u32 = 0x4_0000;
/// Device status of a connected device: connected, input enabled.
const STATUS_CONNECTED: u32 = 0x3;
const ASYNC_ENUMERATION: u32 = 1;

/// Gamepad buttons; the title maps each bit to one of its pad buttons.
const MENU: u32 = 0x1;
const VIEW: u32 = 0x2;
const A: u32 = 0x4;
const B: u32 = 0x8;
const X: u32 = 0x10;
const Y: u32 = 0x20;
const UP: u32 = 0x40;
const DOWN: u32 = 0x80;
const LEFT: u32 = 0x100;
const RIGHT: u32 = 0x200;
const LEFT_SHOULDER: u32 = 0x400;
const RIGHT_SHOULDER: u32 = 0x800;
const LEFT_STICK: u32 = 0x1000;
const RIGHT_STICK: u32 = 0x2000;

/// Devices: host pad slots, the keyboard's stand-in pad, the keyboard.
const PADS: usize = 4;
const FALLBACK: usize = PADS;
const KEYBOARD: usize = PADS + 1;
const DEVICES: usize = PADS + 2;

/// Readings kept per device for next/previous lookups.
const HISTORY: usize = 256;
const POLL: Duration = Duration::from_millis(8);

/// The keyboard's stand-in gamepad (virtual key, button); README lists it.
/// Sticks: W/A/S/D left, I/J/K/L right; triggers Z and C.
const FALLBACK_BUTTONS: [(u8, u32); 14] = [
    (0x1B, MENU), // Esc
    (0x09, VIEW), // Tab
    (0x0D, A),    // Enter
    (0x08, B),    // Backspace
    (b'X', X),
    (b'Y', Y),
    (0x26, UP), // arrows
    (0x28, DOWN),
    (0x25, LEFT),
    (0x27, RIGHT),
    (b'Q', LEFT_SHOULDER),
    (b'E', RIGHT_SHOULDER),
    (b'F', LEFT_STICK),
    (b'H', RIGHT_STICK),
];

/// Mouse buttons, as bits of [`MouseInput::buttons`].
const MOUSE_LEFT: u8 = 0x1;
const MOUSE_RIGHT: u8 = 0x2;
const MOUSE_MIDDLE: u8 = 0x4;
const MOUSE_BACK: u8 = 0x8;
const MOUSE_FORWARD: u8 = 0x10;

/// What the mouse presses on the stand-in pad while captured; README lists
/// it. Left and right are the right and left triggers (`mouse_pad`).
const MOUSE_BUTTONS: [(u8, u32); 3] = [
    (MOUSE_MIDDLE, RIGHT_STICK),
    (MOUSE_BACK, LEFT_SHOULDER),
    (MOUSE_FORWARD, RIGHT_SHOULDER),
];

/// A gamepad reading: triggers 0 to 1, sticks -1 to 1 (up positive).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PadState {
    buttons: u32,
    left_trigger: f32,
    right_trigger: f32,
    left_x: f32,
    left_y: f32,
    right_x: f32,
    right_y: f32,
}

/// One key held down: scan code (0xE0xx when extended), character, key.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct KeyState {
    scan_code: u32,
    code_point: u32,
    virtual_key: u8,
    dead_key: bool,
}

const _: () = assert!(size_of::<KeyState>() == 12 && size_of::<PadState>() == 28);

/// Host input, or a fake in tests.
trait Source: Send {
    /// The pad in host slot `slot`, `None` when the slot is empty.
    fn pad(&mut self, slot: usize) -> Option<PadState>;
    /// Keys held down; none while the title's window lacks focus.
    fn keys(&mut self) -> Vec<KeyState>;
    /// Mouse motion since the last call and buttons held; none while the
    /// mouse is not captured ([`Capture`]).
    fn mouse(&mut self) -> MouseInput;
    /// Motor speeds: low frequency (left), high frequency (right).
    fn rumble(&mut self, slot: usize, low: u16, high: u16);
}

fn fallback(keys: &[KeyState]) -> PadState {
    let on = |vk: u8| {
        if keys.iter().any(|k| k.virtual_key == vk) {
            1.0
        } else {
            0.0
        }
    };
    PadState {
        buttons: FALLBACK_BUTTONS
            .iter()
            .filter(|(vk, _)| on(*vk) != 0.0)
            .fold(0, |all, (_, bit)| all | bit),
        left_trigger: on(b'Z'),
        right_trigger: on(b'C'),
        left_x: on(b'D') - on(b'A'),
        left_y: on(b'W') - on(b'S'),
        right_x: on(b'L') - on(b'J'),
        right_y: on(b'I') - on(b'K'),
    }
}

/// The captured mouse's part of the stand-in pad: the buttons, and `look`
/// (from [`Look`]) on the right stick.
fn mouse_pad(mouse: MouseInput, look: (f32, f32)) -> PadState {
    let held = |button: u8| mouse.buttons & button != 0;
    let pressed = |button: u8| if held(button) { 1.0 } else { 0.0 };
    PadState {
        buttons: MOUSE_BUTTONS
            .iter()
            .filter(|(button, _)| held(*button))
            .fold(0, |all, (_, bit)| all | bit),
        left_trigger: pressed(MOUSE_RIGHT),
        right_trigger: pressed(MOUSE_LEFT),
        right_x: look.0,
        right_y: look.1,
        ..Default::default()
    }
}

/// The mouse in one poll: motion in screen pixels (y down), buttons held
/// (`MOUSE_*` bits), and whether it is captured (mouse look stops at once
/// when it is not).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MouseInput {
    motion: (i32, i32),
    buttons: u8,
    captured: bool,
}

/// A screen rectangle; right and bottom are outside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Rect {
    fn contains(&self, (x, y): (i32, i32)) -> bool {
        (self.left..self.right).contains(&x) && (self.top..self.bottom).contains(&y)
    }

    fn center(&self) -> (i32, i32) {
        (
            self.left + (self.right - self.left) / 2,
            self.top + (self.bottom - self.top) / 2,
        )
    }
}

/// What the host sees of the mouse in one poll.
#[derive(Clone, Copy, Debug, Default)]
struct Seen {
    /// The client area of the title's window while it is the foreground
    /// window and not minimized.
    window: Option<Rect>,
    cursor: (i32, i32),
    /// Held while the title's window is the foreground window, else none.
    buttons: u8,
    escape: bool,
}

/// Mouse capture. A left click in the title's window captures the mouse;
/// Esc (also the pad's Menu), the window losing the foreground or being told
/// it lost activation ([`focus_lost`]) releases it.
/// While captured the host keeps the cursor hidden at the window's centre,
/// and how far it got from there is the motion.
#[derive(Default)]
struct Capture {
    /// The captured window's client area; none while released.
    window: Option<Rect>,
    /// Buttons count once all were up after the click that captured: that
    /// click is no shot.
    armed: bool,
    /// Held at the last poll.
    buttons: u8,
    escape: bool,
}

impl Capture {
    fn step(&mut self, seen: Seen) -> MouseInput {
        let clicked = seen.buttons & !self.buttons & MOUSE_LEFT != 0;
        let escaped = seen.escape && !self.escape;
        self.buttons = seen.buttons;
        self.escape = seen.escape;
        let before = self.window;
        self.window = match (before, seen.window) {
            (Some(_), Some(_)) if escaped => None,
            (Some(_), window) => window,
            (None, Some(window)) if clicked && window.contains(seen.cursor) => {
                self.armed = false;
                Some(window)
            }
            (None, _) => None,
        };
        if before.is_some() != self.window.is_some() {
            let why = match self.window {
                Some(_) => "click",
                None if escaped => "Esc",
                None => "the title's window is not the foreground window",
            };
            tracing::info!(captured = self.window.is_some(), why, "mouse");
        }
        let Some(window) = self.window else {
            return MouseInput::default();
        };
        self.armed |= seen.buttons == 0;
        // The first poll, or one after the window moved, has only the
        // cursor's way to the new centre.
        let motion = if before == Some(window) {
            let (x, y) = window.center();
            (seen.cursor.0 - x, seen.cursor.1 - y)
        } else {
            (0, 0)
        };
        MouseInput {
            motion,
            buttons: if self.armed { seen.buttons } else { 0 },
            captured: true,
        }
    }

    /// Ends a capture the host could not hold; a click captures again.
    fn lose(&mut self, why: &str) {
        if self.window.take().is_some() {
            tracing::warn!(captured = false, why, "mouse");
        }
    }
}

/// The host's cursor, or a fake in tests.
trait Pointer {
    /// The mouse in the title's window, as [`Seen`] describes it.
    fn see(&mut self) -> Seen;
    /// Confines the cursor to `clip`, or frees it (`None`); false when the
    /// system refused.
    fn set_clip(&mut self, clip: Option<Rect>) -> bool;
    /// The clip in effect (the whole screen while there is none).
    fn clip(&mut self) -> Option<Rect>;
    fn move_to(&mut self, at: (i32, i32));
    /// Whether the title's window was told it lost activation since the
    /// last call ([`focus_lost`]).
    fn focus_lost(&mut self) -> bool;
}

/// The mouse in the title's window ([`Capture`]): while captured the cursor
/// is clipped to the window and put back at its centre at every poll.
///
/// The clip is held only while it is the one we made. Whatever takes it off
/// or replaces it (the system when the foreground changes, another program,
/// Wine) ends the capture instead of being fought: a clip is never set again
/// over someone else's, and one the system refused is never taken for ours.
#[derive(Default)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Mouse {
    capture: Capture,
    /// The clip we asked for, and the one the system made of it (it keeps a
    /// clip on the screen), taken off again on release.
    clip: Option<(Rect, Rect)>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Mouse {
    fn poll(&mut self, pointer: &mut impl Pointer) -> MouseInput {
        let seen = pointer.see();
        // Before the clip is held or the cursor centred: only a new click
        // captures again.
        if pointer.focus_lost() {
            self.capture.lose("the title's window lost activation");
        }
        let mut input = self.capture.step(seen);
        if let Some(window) = self.capture.window {
            match self.hold(window, pointer) {
                Ok(()) => {
                    let center = window.center();
                    if seen.cursor != center {
                        pointer.move_to(center);
                    }
                }
                Err(why) => {
                    self.capture.lose(why);
                    input = MouseInput::default();
                }
            }
        }
        if self.capture.window.is_none() {
            self.release(pointer);
        }
        CAPTURED.store(self.capture.window.is_some(), Ordering::Relaxed);
        input
    }

    /// Keeps our clip on `window`'s client area.
    fn hold(&mut self, window: Rect, pointer: &mut impl Pointer) -> Result<(), &'static str> {
        // A pixel inside on every side: Wine takes a clip as large as the
        // screen (a borderless window filling it) for no clip, and then does
        // not grab the pointer.
        let clip = Rect {
            left: window.left + 1,
            top: window.top + 1,
            right: (window.right - 1).max(window.left + 2),
            bottom: (window.bottom - 1).max(window.top + 2),
        };
        if let Some((asked, made)) = self.clip {
            if asked == clip && pointer.clip() == Some(made) {
                return Ok(());
            }
            // A window that moved or changed size asks for a new clip (the
            // system may have taken the old one off for it). For the same
            // window, ours is gone.
            if asked == clip {
                self.clip = None;
                return Err("the cursor clip was taken off or replaced");
            }
        }
        if !pointer.set_clip(Some(clip)) {
            // Whatever clip is in effect is not ours to take off later.
            self.clip = None;
            return Err("the system refused the cursor clip");
        }
        match pointer.clip() {
            Some(made) => {
                self.clip = Some((clip, made));
                Ok(())
            }
            None => {
                // Not known, so it could not be told apart later: off again.
                pointer.set_clip(None);
                Err("the cursor clip could not be read back")
            }
        }
    }

    fn release(&mut self, pointer: &mut impl Pointer) {
        // Another program's clip is left alone.
        if let Some((_, made)) = self.clip.take()
            && pointer.clip() == Some(made)
        {
            pointer.set_clip(None);
        }
    }
}

/// How mouse motion becomes right-stick deflection.
#[derive(Clone, Copy, Debug, PartialEq)]
struct LookSettings {
    /// `VICTORY_MOUSE_SPEED`: a multiplier.
    speed: f32,
    /// `VICTORY_MOUSE_DEAD_ZONE`: the deflection a moving mouse starts at,
    /// over the title's own dead zone. The default is XInput's documented
    /// right-stick dead zone (8689 of 32767).
    dead_zone: f32,
}

impl Default for LookSettings {
    fn default() -> LookSettings {
        LookSettings {
            speed: 1.0,
            dead_zone: 0.265,
        }
    }
}

impl LookSettings {
    fn parse(speed: Option<&str>, dead_zone: Option<&str>) -> LookSettings {
        let default = LookSettings::default();
        let number = |name: &str, value: Option<&str>, valid: fn(f32) -> bool, default: f32| {
            let Some(value) = value else {
                return default;
            };
            match value.trim().parse() {
                Ok(n) if valid(n) => n,
                _ => {
                    tracing::warn!(value, "ignoring invalid {name}");
                    default
                }
            }
        };
        LookSettings {
            speed: number(
                "VICTORY_MOUSE_SPEED",
                speed,
                |n| n.is_finite() && n > 0.0,
                default.speed,
            ),
            dead_zone: number(
                "VICTORY_MOUSE_DEAD_ZONE",
                dead_zone,
                |n| (0.0..0.95).contains(&n),
                default.dead_zone,
            ),
        }
    }

    fn from_env() -> LookSettings {
        let var = |name| std::env::var(name).ok();
        LookSettings::parse(
            var("VICTORY_MOUSE_SPEED").as_deref(),
            var("VICTORY_MOUSE_DEAD_ZONE").as_deref(),
        )
    }
}

/// Whether a click in the title's window captures the mouse (on unless
/// `VICTORY_MOUSE=0`).
#[cfg_attr(not(windows), allow(dead_code))]
fn mouse_enabled() -> bool {
    std::env::var_os("VICTORY_MOUSE").is_none_or(|v| v != "0")
}

/// Mouse speed (pixels a second) that tilts the stick all the way at speed 1.
const FULL_TILT: f32 = 2500.0;
/// Seconds over which mouse speed is averaged. The title samples the pad
/// once a frame, and under Wine motion arrives once a title frame (its window
/// thread takes it in), so a stick fed from each poll's motion alone would
/// miss most of it. An average keeps the sum: the camera turns as far as the
/// mouse went.
const LOOK_SMOOTHING: f32 = 0.05;
/// Below this deflection (12.5 pixels a second at speed 1) the stick is at
/// rest; a single pixel's move stays above it for a poll.
const LOOK_REST: f32 = 0.005;

/// Mouse look: the right stick tilts with the mouse's average speed.
#[derive(Default)]
struct Look {
    settings: LookSettings,
    /// Pixels a second, y down.
    velocity: (f32, f32),
    /// The last step's time (µs).
    last: Option<u64>,
}

impl Look {
    fn new(settings: LookSettings) -> Look {
        Look {
            settings,
            ..Default::default()
        }
    }

    /// The right stick (up positive) after `motion` up to `now` (µs).
    fn step(&mut self, motion: (i32, i32), now: u64) -> (f32, f32) {
        let dt = self
            .last
            .map_or(POLL.as_secs_f32(), |last| {
                now.saturating_sub(last) as f32 / 1e6
            })
            .clamp(0.001, 0.1);
        self.last = Some(now);
        let blend = 1.0 - (-dt / LOOK_SMOOTHING).exp();
        let toward = |v: f32, d: i32| v + (d as f32 / dt - v) * blend;
        self.velocity = (
            toward(self.velocity.0, motion.0),
            toward(self.velocity.1, motion.1),
        );
        let scale = self.settings.speed / FULL_TILT;
        let (x, y) = (self.velocity.0 * scale, -self.velocity.1 * scale);
        let length = x.hypot(y);
        if length < LOOK_REST {
            self.velocity = (0.0, 0.0);
            return (0.0, 0.0);
        }
        let dead_zone = self.settings.dead_zone;
        let tilt = dead_zone + (1.0 - dead_zone) * length.min(1.0);
        (x / length * tilt, y / length * tilt)
    }

    /// The stick at rest, with no speed left to carry into a later capture.
    fn stop(&mut self) -> (f32, f32) {
        *self = Look::new(self.settings);
        (0.0, 0.0)
    }
}

/// Set by [`focus_lost`], taken by the next poll.
static FOCUS_LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The title's window was told it lost activation (WM_CANCELMODE,
/// WM_ACTIVATE inactive, WM_ACTIVATEAPP false): the next poll ends the
/// capture. Under Wine with a window manager, an X11 window outside Wine that
/// takes the keyboard focus can leave the title's window the foreground
/// window with our clip in place, and WM_CANCELMODE is then the only sign.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn focus_lost() {
    FOCUS_LOST.store(true, Ordering::Relaxed);
}

/// Set while the mouse is captured: the title's window hides the cursor.
static CAPTURED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the mouse is captured (the cursor hidden over the title's window).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn mouse_captured() -> bool {
    CAPTURED.load(Ordering::Relaxed)
}

/// Two pads as one: buttons of both, and of each axis and trigger the one
/// pushed further.
fn merged(a: PadState, b: PadState) -> PadState {
    let further = |x: f32, y: f32| if y.abs() > x.abs() { y } else { x };
    PadState {
        buttons: a.buttons | b.buttons,
        left_trigger: further(a.left_trigger, b.left_trigger),
        right_trigger: further(a.right_trigger, b.right_trigger),
        left_x: further(a.left_x, b.left_x),
        left_y: further(a.left_y, b.left_y),
        right_x: further(a.right_x, b.right_x),
        right_y: further(a.right_y, b.right_y),
    }
}

/// Whether the title sees one pad that is always there, fed by the keyboard
/// and by every host pad (on by default; `VICTORY_ONE_PAD=0` shows each host
/// pad as a device of its own, and the stand-in only while there is none).
/// The title keeps to the pad it started with: with the stand-in taken away
/// when a pad was switched on, it showed its controller-disconnected screen
/// until the pad was off again. It also keeps a pad the host lists twice (its
/// own slot and a remapper's) from being two players.
fn one_pad() -> bool {
    std::env::var_os("VICTORY_ONE_PAD").is_none_or(|v| v != "0")
}

#[repr(C)]
struct Table<const N: usize>([*const (); N]);
// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Table<N> {}

/// The input object; one per hub.
#[repr(C)]
pub struct Object {
    vtable: &'static Table<22>,
    hub: *const Hub,
}

#[repr(C)]
pub struct Device {
    vtable: &'static Table<21>,
    hub: *const Hub,
    index: usize,
    info: DeviceInfo,
}

/// What a device says about itself. The title reads `id` (+0x20); the rest
/// follows the public layout and is zero where we have nothing to say.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // read by title code
struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    /// Counts and optional detail pointers.
    rest: [u64; 26],
}

const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20);

fn kind(index: usize) -> u32 {
    if index == KEYBOARD {
        KEYBOARD_KIND
    } else {
        GAMEPAD_KIND
    }
}

impl Device {
    fn new(index: usize) -> Device {
        let mut id = [0; 32];
        id[..7].copy_from_slice(b"victory");
        id[31] = index as u8 + 1;
        let info = DeviceInfo {
            size: size_of::<DeviceInfo>() as u32,
            id,
            root_id: id,
            supported_input: kind(index),
            // Low and high frequency.
            rumble_motors: if index <= FALLBACK { 0x3 } else { 0 },
            ..Default::default()
        };
        Device {
            vtable: &DEVICE,
            hub: std::ptr::null(),
            index,
            info,
        }
    }

    fn hub(&self) -> &'static Hub {
        // SAFETY: set by `Hub::new`; hubs are never freed.
        unsafe { &*self.hub }
    }
}

#[derive(PartialEq)]
enum Value {
    Pad(PadState),
    Keys(Vec<KeyState>),
}

#[repr(C)]
pub struct Reading {
    vtable: &'static Table<25>,
    refs: AtomicU32,
    device: *const Device,
    sequence: u64,
    timestamp: u64,
    value: Value,
}

impl Reading {
    fn kind(&self) -> u32 {
        match self.value {
            Value::Pad(_) => GAMEPAD_KIND,
            Value::Keys(_) => KEYBOARD_KIND,
        }
    }
}

/// A counted reference to a reading.
struct Ref(NonNull<Reading>);

impl Ref {
    fn new(reading: Reading) -> Ref {
        Ref(NonNull::from(Box::leak(Box::new(reading))))
    }

    /// Another reference, for the title.
    fn share(&self) -> *const Reading {
        reading_add_ref(self.0.as_ptr());
        self.0.as_ptr()
    }
}

impl Deref for Ref {
    type Target = Reading;
    fn deref(&self) -> &Reading {
        // SAFETY: we hold a reference, so it is alive.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        reading_release(self.0.as_ptr());
    }
}

type DeviceCallback = extern "system" fn(u64, *mut c_void, *const Device, u64, u32, u32);

#[derive(Clone, Copy)]
struct Registration {
    token: u64,
    /// One device's address, or 0 for any.
    device: usize,
    kind: u32,
    status: u32,
    context: usize,
    callback: DeviceCallback,
}

impl Registration {
    fn matches(&self, devices: &[Device; DEVICES], index: usize) -> bool {
        kind(index) & self.kind != 0
            && (self.device == 0 || self.device == &devices[index] as *const Device as usize)
    }

    fn event(&self, device: usize, timestamp: u64, current: u32, previous: u32) -> Event {
        Event {
            registration: *self,
            device,
            timestamp,
            current,
            previous,
        }
    }
}

struct Event {
    registration: Registration,
    device: usize,
    timestamp: u64,
    current: u32,
    previous: u32,
}

#[derive(Default)]
struct Slot {
    connected: bool,
    sequence: u64,
    history: VecDeque<Ref>,
}

struct State {
    slots: [Slot; DEVICES],
    callbacks: Vec<Registration>,
    /// Asynchronous enumerations, delivered by the next poll.
    pending: Vec<Event>,
    /// Device callbacks running now: token and thread.
    running: Vec<(u64, ThreadId)>,
    look: Look,
}

pub struct Hub {
    object: Object,
    devices: [Device; DEVICES],
    /// Apart from the state: XInput can be slow, and the title reads
    /// readings every frame.
    source: Mutex<Box<dyn Source>>,
    /// The title sees one pad ([`one_pad`]).
    one_pad: bool,
    state: Mutex<State>,
    /// A device callback returned.
    returned: Condvar,
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// SAFETY: the back pointers are set before the hub is shared and never
// change; what changes sits behind the mutex or is atomic.
unsafe impl Send for Hub {}
// SAFETY: as above.
unsafe impl Sync for Hub {}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Microseconds, the clock of every timestamp we hand out.
fn now() -> u64 {
    START.elapsed().as_micros() as u64
}

impl Hub {
    fn new(source: Box<dyn Source>, one_pad: bool, look: LookSettings) -> &'static Hub {
        let hub = Box::into_raw(Box::new(Hub {
            object: Object {
                vtable: &INPUT,
                hub: std::ptr::null(),
            },
            devices: std::array::from_fn(Device::new),
            source: Mutex::new(source),
            one_pad,
            state: Mutex::new(State {
                slots: Default::default(),
                callbacks: Vec::new(),
                pending: Vec::new(),
                running: Vec::new(),
                look: Look::new(look),
            }),
            returned: Condvar::new(),
        }));
        // SAFETY: just allocated and never freed; nothing else sees it yet.
        unsafe {
            (*hub).object.hub = hub;
            for device in &mut (*hub).devices {
                device.hub = hub;
            }
            &*hub
        }
    }

    /// A hub over the host's input, polled from now on.
    fn start(source: Box<dyn Source>) -> &'static Hub {
        let hub = Hub::new(source, one_pad(), LookSettings::from_env());
        hub.poll();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL);
                hub.poll();
            }
        });
        hub
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Reads the host once; device callbacks run on the calling thread.
    fn poll(&self) {
        let (pads, keys, mouse) = {
            let mut source = lock(&self.source);
            let pads: [_; PADS] = std::array::from_fn(|slot| source.pad(slot));
            (pads, source.keys(), source.mouse())
        };
        let events = self
            .state()
            .poll(&self.devices, self.one_pad, pads, keys, mouse);
        self.deliver(events);
    }

    /// Runs callbacks, never under our lock: they call back into us. One
    /// unregistered since the event was taken does not run, and
    /// `UnregisterCallback` waits for one that runs.
    fn deliver(&self, events: Vec<Event>) {
        let me = std::thread::current().id();
        for e in events {
            let r = e.registration;
            {
                let mut state = self.state();
                if !state.callbacks.iter().any(|c| c.token == r.token) {
                    continue;
                }
                state.running.push((r.token, me));
            }
            (r.callback)(
                r.token,
                r.context as *mut c_void,
                &self.devices[e.device],
                e.timestamp,
                e.current,
                e.previous,
            );
            let mut state = self.state();
            if let Some(i) = state.running.iter().position(|&x| x == (r.token, me)) {
                state.running.swap_remove(i);
            }
            self.returned.notify_all();
        }
    }

    fn index(&self, device: *const Device) -> Option<usize> {
        self.devices.iter().position(|d| std::ptr::eq(d, device))
    }
}

impl State {
    fn poll(
        &mut self,
        devices: &[Device; DEVICES],
        one_pad: bool,
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        mouse: MouseInput,
    ) -> Vec<Event> {
        let now = now();
        let mut events = std::mem::take(&mut self.pending);
        // Every poll while captured, so the stick comes to rest after the
        // mouse stops; at once on release.
        let look = if mouse.captured {
            self.look.step(mouse.motion, now)
        } else {
            self.look.stop()
        };
        let stand_in = merged(fallback(&keys), mouse_pad(mouse, look));
        if one_pad {
            let pad = pads.into_iter().flatten().fold(stand_in, merged);
            self.update(devices, FALLBACK, Some(Value::Pad(pad)), now, &mut events);
            self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
            return events;
        }
        let any_pad = pads.iter().any(Option::is_some);
        for (slot, pad) in pads.into_iter().enumerate() {
            self.update(devices, slot, pad.map(Value::Pad), now, &mut events);
        }
        let fallback = (!any_pad).then_some(Value::Pad(stand_in));
        self.update(devices, FALLBACK, fallback, now, &mut events);
        self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
        events
    }

    /// Records a device's state: a reading when it changed, callbacks when
    /// the device came or went.
    fn update(
        &mut self,
        devices: &[Device; DEVICES],
        index: usize,
        value: Option<Value>,
        now: u64,
        events: &mut Vec<Event>,
    ) {
        let slot = &mut self.slots[index];
        let was = slot.connected;
        slot.connected = value.is_some();
        if let Some(value) = value
            && (!was || slot.history.back().is_none_or(|r| r.value != value))
        {
            slot.sequence += 1;
            if slot.history.len() == HISTORY {
                slot.history.pop_front();
            }
            slot.history.push_back(Ref::new(Reading {
                vtable: &READING,
                refs: AtomicU32::new(1),
                device: &devices[index],
                sequence: slot.sequence,
                timestamp: now,
                value,
            }));
        }
        if was != slot.connected {
            tracing::info!(device = index, connected = !was, "input device");
            let (current, previous) = if was {
                (0, STATUS_CONNECTED)
            } else {
                (STATUS_CONNECTED, 0)
            };
            events.extend(
                self.callbacks
                    .iter()
                    .filter(|r| r.matches(devices, index) && r.status & STATUS_CONNECTED != 0)
                    .map(|r| r.event(index, now, current, previous)),
            );
        }
    }
}

#[cfg(windows)]
fn host() -> Box<dyn Source> {
    Box::new(host::Host::new(mouse_enabled()))
}

#[cfg(not(windows))]
fn host() -> Box<dyn Source> {
    struct Nothing;
    impl Source for Nothing {
        fn pad(&mut self, _: usize) -> Option<PadState> {
            None
        }
        fn keys(&mut self) -> Vec<KeyState> {
            Vec::new()
        }
        fn mouse(&mut self) -> MouseInput {
            MouseInput::default()
        }
        fn rumble(&mut self, _: usize, _: u16, _: u16) {}
    }
    Box::new(Nothing)
}

static HUB: LazyLock<&'static Hub> = LazyLock::new(|| Hub::start(host()));

type This = *const Object;
type Out = *mut *const c_void;

/// The DLL's only export (ordinal 1): the input object, static and shared.
pub extern "system" fn GameInputCreate(out: *mut This) -> HRESULT {
    tracing::info!("GameInputCreate");
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a pointer.
    unsafe { out.write(&HUB.object) };
    S_OK
}

fn hub(this: This) -> &'static Hub {
    // SAFETY: `this` is a hub's object; hubs are never freed.
    unsafe { &*(*this).hub }
}

/// Clears a COM out pointer and returns `hr`.
fn fail(out: Out, hr: HRESULT) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(std::ptr::null()) };
    }
    hr
}

/// Gives the title a reference to `reading`, or fails as with none.
fn hand(out: Out, reading: Option<&Ref>) -> HRESULT {
    match reading {
        _ if out.is_null() => E_INVALIDARG,
        Some(reading) => {
            // SAFETY: checked non-null; the title passes room for a pointer.
            unsafe { out.write(reading.share().cast()) };
            S_OK
        }
        None => fail(out, READING_NOT_FOUND),
    }
}

extern "system" fn no_interface(_: *const c_void, _iid: *const c_void, out: Out) -> HRESULT {
    tracing::warn!("input QueryInterface not implemented");
    fail(out, E_NOINTERFACE)
}
/// The input object and devices live as long as the process.
extern "system" fn add_ref(_: *const c_void) -> u32 {
    1
}

extern "system" fn timestamp(_: This) -> u64 {
    now()
}

extern "system" fn current_reading(
    this: This,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let state = hub.state();
    let newest = |i: usize| {
        state.slots[i]
            .history
            .back()
            .filter(|r| r.kind() & kind != 0)
    };
    let found = if device.is_null() {
        (0..DEVICES)
            .filter(|&i| state.slots[i].connected)
            .filter_map(newest)
            .max_by_key(|r| r.timestamp)
    } else {
        match hub.index(device) {
            None => return fail(out, E_INVALIDARG),
            Some(i) if !state.slots[i].connected => return fail(out, DEVICE_DISCONNECTED),
            Some(i) => newest(i),
        }
    };
    tracing::trace!(
        kind = format_args!("{kind:#x}"),
        device = ?hub.index(device),
        sequence = found.map(|r| r.sequence),
        "current reading"
    );
    hand(out, found)
}

/// The reading after (`forward`) or before `reference` on its device.
fn relative(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
    forward: bool,
) -> HRESULT {
    if reference.is_null() {
        return fail(out, E_INVALIDARG);
    }
    let hub = hub(this);
    // SAFETY: a reading we handed out and the title still holds.
    let reference = unsafe { &*reference };
    // shortcut: with no device given only the reference's device is walked,
    // not every device's readings in time order; the title walks
    // only keyboard readings (one device) and the current pad's previous one.
    let Some(index) = hub.index(reference.device) else {
        return fail(out, E_INVALIDARG);
    };
    if !device.is_null() && !std::ptr::eq(device, reference.device) {
        return fail(out, E_INVALIDARG);
    }
    let state = hub.state();
    let history = &state.slots[index].history;
    if history
        .front()
        .is_none_or(|oldest| reference.sequence < oldest.sequence)
    {
        return fail(out, REFERENCE_TOO_OLD);
    }
    let wanted = |r: &&Ref| r.kind() & kind != 0;
    let found = if forward {
        history
            .iter()
            .filter(|r| r.sequence > reference.sequence)
            .find(wanted)
    } else {
        history
            .iter()
            .rev()
            .filter(|r| r.sequence < reference.sequence)
            .find(wanted)
    };
    hand(out, found)
}
extern "system" fn next_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, true)
}
extern "system" fn previous_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, false)
}
/// The device's reading at `time`, among those still kept.
extern "system" fn temporal_reading(
    this: This,
    time: u64,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let Some(index) = hub.index(device) else {
        return fail(out, E_INVALIDARG);
    };
    let state = hub.state();
    let history = &state.slots[index].history;
    hand(out, history.iter().rev().find(|r| r.timestamp <= time))
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn token(out: *mut u64) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)) };
    }
    S_OK
}

// shortcut: reading callbacks never run; the title registers none
// (it polls).
extern "system" fn register_reading(
    _: This,
    _device: *const c_void,
    _kind: u32,
    _threshold: f32,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    tracing::warn!("input reading callbacks are never called");
    token(out)
}

/// Device callbacks run on our poll thread when a device comes or goes.
/// Enumeration reports the devices already there: blocking on the caller's
/// thread before we return (the title registers holding a lock its callback
/// takes again), asynchronous on the next poll.
extern "system" fn register_device(
    this: This,
    device: *const Device,
    kind: u32,
    status: u32,
    enumeration: u32,
    context: *mut c_void,
    callback: Option<DeviceCallback>,
    out: *mut u64,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let hub = hub(this);
    let registration = Registration {
        token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        device: device as usize,
        kind,
        status,
        context: context as usize,
        callback,
    };
    let mut found = Vec::new();
    {
        let mut state = hub.state();
        if enumeration != 0 && status & STATUS_CONNECTED != 0 {
            let now = now();
            found = (0..DEVICES)
                .filter(|&i| state.slots[i].connected && registration.matches(&hub.devices, i))
                .map(|i| registration.event(i, now, STATUS_CONNECTED, 0))
                .collect();
        }
        tracing::info!(
            kind = format_args!("{kind:#x}"),
            enumeration,
            devices = found.len(),
            "input device callback"
        );
        state.callbacks.push(registration);
        if enumeration == ASYNC_ENUMERATION {
            state.pending.append(&mut found);
        }
    }
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(registration.token) };
    }
    hub.deliver(found);
    S_OK
}
/// Guide button and keyboard layout callbacks: neither ever changes here.
extern "system" fn register_other(
    _: This,
    _device: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    token(out)
}

fn forget(this: This, token: u64) {
    let mut state = hub(this).state();
    state.callbacks.retain(|r| r.token != token);
    state.pending.retain(|e| e.registration.token != token);
}
extern "system" fn stop_callback(this: This, token: u64) {
    forget(this, token);
}
/// Stops the callback, then waits up to `timeout` µs for a call of it that
/// is running; false on time-out, or from inside that callback (it cannot
/// wait for itself), as documented. Either way it is never called again.
extern "system" fn unregister_callback(this: This, token: u64, timeout: u64) -> bool {
    forget(this, token);
    let hub = hub(this);
    let me = std::thread::current().id();
    let deadline = Instant::now().checked_add(Duration::from_micros(timeout));
    let mut state = hub.state();
    loop {
        let running = state.running.iter().filter(|r| r.0 == token);
        if running.clone().any(|r| r.1 == me) {
            return false;
        }
        if running.count() == 0 {
            return true;
        }
        state = match deadline {
            None => hub.returned.wait(state).unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                let waited = hub.returned.wait_timeout(state, left);
                waited.unwrap_or_else(PoisonError::into_inner).0
            }
        };
    }
}

/// Dispatcher and aggregate device: no title of ours asks for them.
extern "system" fn create_dispatcher(_: This, out: Out) -> HRESULT {
    tracing::warn!("input dispatcher not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn create_aggregate(_: This, _kind: u32, out: Out) -> HRESULT {
    tracing::warn!("aggregate input device not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn find_by_id(this: This, id: *const [u8; 32], out: Out) -> HRESULT {
    if id.is_null() || out.is_null() {
        return fail(out, E_INVALIDARG);
    }
    // SAFETY: checked non-null; the title passes a device id.
    let id = unsafe { &*id };
    match hub(this).devices.iter().find(|d| d.info.id == *id) {
        Some(device) => {
            // SAFETY: checked non-null.
            unsafe { out.write((device as *const Device).cast()) };
            S_OK
        }
        None => fail(out, DEVICE_NOT_FOUND),
    }
}
/// Lookup by object, platform handle or platform string: our devices have
/// none of these.
extern "system" fn find_device(_: This, _value: *const c_void, out: Out) -> HRESULT {
    fail(out, DEVICE_NOT_FOUND)
}
extern "system" fn enable_oem_device(
    _: This,
    _vendor: u16,
    _product: u16,
    _interface: u8,
    _collection: u8,
) -> HRESULT {
    S_OK
}
extern "system" fn set_focus_policy(_: This, _policy: u32) {}

static INPUT: Table<22> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    timestamp as *const (),           // 0x18
    current_reading as *const (),     // 0x20
    next_reading as *const (),        // 0x28
    previous_reading as *const (),    // 0x30
    temporal_reading as *const (),    // 0x38
    register_reading as *const (),    // 0x40
    register_device as *const (),     // 0x48
    register_other as *const (),      // 0x50 guide button
    register_other as *const (),      // 0x58 keyboard layout
    stop_callback as *const (),       // 0x60
    unregister_callback as *const (), // 0x68
    create_dispatcher as *const (),   // 0x70
    create_aggregate as *const (),    // 0x78
    find_by_id as *const (),          // 0x80
    find_device as *const (),         // 0x88 by object
    find_device as *const (),         // 0x90 by platform handle
    find_device as *const (),         // 0x98 by platform string
    enable_oem_device as *const (),   // 0xa0
    set_focus_policy as *const (),    // 0xa8
]);

extern "system" fn device_info(this: *const Device) -> *const DeviceInfo {
    // SAFETY: one of our devices.
    unsafe { &(*this).info }
}
extern "system" fn device_status(this: *const Device) -> u32 {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    if device.hub().state().slots[device.index].connected {
        STATUS_CONNECTED
    } else {
        0
    }
}
// shortcut: trigger motors are dropped (XInput has none). The host API that
// drives them without a redistributable is Windows.Gaming.Input (WinRT
// `Gamepad.Vibration`), which needs WinRT bindings this crate lacks outside
// diagnostics builds; the title sends trigger levels in slot 0x50.
extern "system" fn set_rumble(this: *const Device, params: *const [f32; 4]) {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    let one_pad = device.hub().one_pad;
    if device.index >= PADS && !(one_pad && device.index == FALLBACK) {
        return;
    }
    let [low, high, ..] = if params.is_null() {
        [0.0; 4]
    } else {
        // SAFETY: checked non-null; four motor levels.
        unsafe { *params }
    };
    let speed = |level: f32| (level.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16;
    let mut source = lock(&device.hub().source);
    if one_pad {
        // The one pad is every host pad.
        for slot in 0..PADS {
            if source.pad(slot).is_some() {
                source.rumble(slot, speed(low), speed(high));
            }
        }
    } else {
        source.rumble(device.index, speed(low), speed(high));
    }
}
extern "system" fn unsupported<const SLOT: usize>(_: *const c_void) -> HRESULT {
    tracing::warn!(
        slot = format_args!("{SLOT:#x}"),
        "input device method not implemented"
    );
    E_NOTIMPL
}

static DEVICE: Table<21> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    device_info as *const (),         // 0x18
    device_status as *const (),       // 0x20
    unsupported::<0x28> as *const (), // battery
    unsupported::<0x30> as *const (), // force feedback
    unsupported::<0x38> as *const (), //
    unsupported::<0x40> as *const (), //
    unsupported::<0x48> as *const (), // haptics
    set_rumble as *const (),          // 0x50
    unsupported::<0x58> as *const (), // input synchronization
    unsupported::<0x60> as *const (), //
    unsupported::<0x68> as *const (), // power off
    unsupported::<0x70> as *const (), // raw device access
    unsupported::<0x78> as *const (), //
    unsupported::<0x80> as *const (), //
    unsupported::<0x88> as *const (), //
    unsupported::<0x90> as *const (), //
    unsupported::<0x98> as *const (), //
    unsupported::<0xa0> as *const (), //
]);

fn reading<'a>(this: *const Reading) -> &'a Reading {
    // SAFETY: a reading we handed out and the title still holds.
    unsafe { &*this }
}
extern "system" fn reading_add_ref(this: *const Reading) -> u32 {
    reading(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
extern "system" fn reading_release(this: *const Reading) -> u32 {
    let left = reading(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference is gone; `Ref::new` boxed it.
        drop(unsafe { Box::from_raw(this.cast_mut()) });
    }
    left
}
extern "system" fn input_kind(this: *const Reading) -> u32 {
    reading(this).kind()
}
extern "system" fn sequence_number(this: *const Reading, kind: u32) -> u64 {
    let reading = reading(this);
    if reading.kind() & kind != 0 {
        reading.sequence
    } else {
        0
    }
}
extern "system" fn reading_timestamp(this: *const Reading) -> u64 {
    reading(this).timestamp
}
extern "system" fn reading_device(this: *const Reading, out: *mut *const Device) {
    if !out.is_null() {
        // SAFETY: checked non-null; devices need no reference.
        unsafe { out.write(reading(this).device) };
    }
}
extern "system" fn key_count(this: *const Reading) -> u32 {
    match &reading(this).value {
        Value::Keys(keys) => keys.len() as u32,
        Value::Pad(_) => 0,
    }
}
extern "system" fn key_state(this: *const Reading, count: u32, out: *mut KeyState) -> u32 {
    let Value::Keys(keys) = &reading(this).value else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let n = keys.len().min(count as usize);
    // SAFETY: the title passes room for `count` keys.
    unsafe { std::ptr::copy_nonoverlapping(keys.as_ptr(), out, n) };
    n as u32
}
extern "system" fn gamepad_state(this: *const Reading, out: *mut PadState) -> bool {
    match &reading(this).value {
        Value::Pad(state) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(*state) };
            true
        }
        _ => false,
    }
}
/// Kinds our devices do not have: no count, no state.
extern "system" fn nothing(_: *const c_void) -> usize {
    0
}

static READING: Table<25> = Table([
    no_interface as *const (),      // 0x00
    reading_add_ref as *const (),   // 0x08
    reading_release as *const (),   // 0x10
    input_kind as *const (),        // 0x18
    sequence_number as *const (),   // 0x20
    reading_timestamp as *const (), // 0x28
    reading_device as *const (),    // 0x30
    nothing as *const (),           // 0x38 raw report
    nothing as *const (),           // 0x40 controller axes, buttons, switches
    nothing as *const (),           // 0x48
    nothing as *const (),           // 0x50
    nothing as *const (),           // 0x58
    nothing as *const (),           // 0x60
    nothing as *const (),           // 0x68
    key_count as *const (),         // 0x70
    key_state as *const (),         // 0x78
    nothing as *const (),           // 0x80 mouse
    nothing as *const (),           // 0x88 touch
    nothing as *const (),           // 0x90
    nothing as *const (),           // 0x98 motion
    nothing as *const (),           // 0xa0 arcade stick
    nothing as *const (),           // 0xa8 flight stick
    gamepad_state as *const (),     // 0xb0
    nothing as *const (),           // 0xb8 racing wheel
    nothing as *const (),           // 0xc0 UI navigation
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "gameinput",
        dlls: &["gameinput.dll"],
        // Titles import it by ordinal only.
        exports: vec![HleExport {
            name: "#1",
            address: GameInputCreate as *const () as usize,
        }],
    }
}

/// Host input: XInput pads, the keyboard while our process has focus, the
/// mouse while captured in the title's window.
#[cfg(windows)]
mod host {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, VIRTUAL_KEY, VK_ESCAPE, VK_LBUTTON,
        VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
    };
    use windows_sys::Win32::UI::Input::XboxController::{
        XINPUT_GAMEPAD, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
        XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
        XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
        XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
        XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE, XINPUT_VIBRATION, XInputGetState,
        XInputSetState,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ClipCursor, GetClipCursor, GetCursorPos, GetForegroundWindow, GetWindowInfo,
        GetWindowThreadProcessId, IsIconic, SetCursorPos, WINDOWINFO,
    };

    use super::*;

    /// Asking XInput about an empty slot is slow; look again this often.
    const EMPTY_SLOT_RECHECK: Duration = Duration::from_secs(1);

    const BUTTONS: [(u16, u32); 14] = [
        (XINPUT_GAMEPAD_START, MENU),
        (XINPUT_GAMEPAD_BACK, VIEW),
        (XINPUT_GAMEPAD_A, A),
        (XINPUT_GAMEPAD_B, B),
        (XINPUT_GAMEPAD_X, X),
        (XINPUT_GAMEPAD_Y, Y),
        (XINPUT_GAMEPAD_DPAD_UP, UP),
        (XINPUT_GAMEPAD_DPAD_DOWN, DOWN),
        (XINPUT_GAMEPAD_DPAD_LEFT, LEFT),
        (XINPUT_GAMEPAD_DPAD_RIGHT, RIGHT),
        (XINPUT_GAMEPAD_LEFT_SHOULDER, LEFT_SHOULDER),
        (XINPUT_GAMEPAD_RIGHT_SHOULDER, RIGHT_SHOULDER),
        (XINPUT_GAMEPAD_LEFT_THUMB, LEFT_STICK),
        (XINPUT_GAMEPAD_RIGHT_THUMB, RIGHT_STICK),
    ];

    /// Raw values, no dead zone: titles apply their own.
    pub(super) fn convert(pad: &XINPUT_GAMEPAD) -> PadState {
        let stick = |v: i16| (f32::from(v) / 32767.0).max(-1.0);
        PadState {
            buttons: BUTTONS
                .iter()
                .filter(|(host, _)| pad.wButtons & host != 0)
                .fold(0, |all, (_, bit)| all | bit),
            left_trigger: f32::from(pad.bLeftTrigger) / 255.0,
            right_trigger: f32::from(pad.bRightTrigger) / 255.0,
            left_x: stick(pad.sThumbLX),
            left_y: stick(pad.sThumbLY),
            right_x: stick(pad.sThumbRX),
            right_y: stick(pad.sThumbRY),
        }
    }

    pub(super) struct Host {
        empty_until: [Option<Instant>; PADS],
        /// None with `VICTORY_MOUSE=0`.
        mouse: Option<Mouse>,
    }

    impl Host {
        pub(super) fn new(mouse: bool) -> Host {
            Host {
                empty_until: Default::default(),
                mouse: mouse.then(Mouse::default),
            }
        }
    }

    impl Source for Host {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            if self.empty_until[slot].is_some_and(|until| Instant::now() < until) {
                return None;
            }
            let mut state = XINPUT_STATE::default();
            // SAFETY: room for the state.
            if unsafe { XInputGetState(slot as u32, &mut state) } != 0 {
                self.empty_until[slot] = Some(Instant::now() + EMPTY_SLOT_RECHECK);
                return None;
            }
            self.empty_until[slot] = None;
            #[allow(unused_mut)]
            let mut pad = convert(&state.Gamepad);
            // The scripted keys (`VICTORY_KEYS`) also press the first
            // pad's buttons, by the stand-in pad's table: with a real pad
            // connected the stand-in is not there, and a title that listens
            // to the pad (the title's start screen) saw nothing.
            #[cfg(feature = "diagnostics")]
            if slot == 0 {
                for vk in scripted() {
                    if let Some(&(_, button)) = super::FALLBACK_BUTTONS.iter().find(|(key, _)| *key == vk) {
                        pad.buttons |= button;
                    }
                }
            }
            Some(pad)
        }

        fn keys(&mut self) -> Vec<KeyState> {
            #[cfg(feature = "diagnostics")]
            let mut down = scripted();
            #[cfg(not(feature = "diagnostics"))]
            let mut down = Vec::new();
            if focused() {
                // Left/right modifier keys only, as devices report them; no
                // mouse buttons (below 0x08).
                let held: Vec<u8> = (0x08..=0xFE_u8)
                    .filter(|vk| !(0x10..=0x12).contains(vk) && !down.contains(vk))
                    // SAFETY: plain queries.
                    .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
                    .collect();
                down.extend(held);
            }
            down.into_iter()
                .map(|vk| KeyState {
                    // SAFETY: plain query.
                    scan_code: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) },
                    virtual_key: vk,
                    // shortcut: no characters (code point 0);
                    // the title reads scan codes and virtual keys only.
                    ..Default::default()
                })
                .collect()
        }

        fn mouse(&mut self) -> MouseInput {
            self.mouse
                .as_mut()
                .map_or_else(MouseInput::default, |mouse| mouse.poll(&mut HostPointer))
        }

        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            let speeds = XINPUT_VIBRATION {
                wLeftMotorSpeed: low,
                wRightMotorSpeed: high,
            };
            // SAFETY: a valid vibration; an empty slot just fails.
            unsafe { XInputSetState(slot as u32, &speeds) };
        }
    }

    /// Debugging aid: `VICTORY_KEYS=<seconds>:<key>,...` holds each key for
    /// 0.2 s that long after input starts (`<from>-<to>:<key>`: all that
    /// time), with or without focus. A key is a
    /// letter, a digit, or `Enter`, `Esc`, `Tab`, `Back`, `Up`, `Down`,
    /// `Left`, `Right` (the stand-in pad's table maps them to buttons).
    #[cfg(feature = "diagnostics")]
    fn scripted() -> Vec<u8> {
        static SCRIPT: LazyLock<(Instant, Vec<(Duration, Duration, u8)>)> = LazyLock::new(|| {
            let list = std::env::var("VICTORY_KEYS").unwrap_or_default();
            let keys = list.split(',').filter_map(|entry| {
                let (at, key) = entry.split_once(':')?;
                let vk = match key.trim() {
                    "Enter" => 0x0D,
                    "Esc" => 0x1B,
                    "Tab" => 0x09,
                    "Back" => 0x08,
                    "Up" => 0x26,
                    "Down" => 0x28,
                    "Left" => 0x25,
                    "Right" => 0x27,
                    k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
                        k.as_bytes()[0].to_ascii_uppercase()
                    }
                    _ => return None,
                };
                let time = |t: &str| Duration::try_from_secs_f64(t.trim().parse().ok()?).ok();
                let (from, to) = match at.split_once('-') {
                    Some((from, to)) => (time(from)?, time(to)?),
                    None => (time(at)?, time(at)? + Duration::from_millis(200)),
                };
                Some((from, to, vk))
            });
            (Instant::now(), keys.collect())
        });
        let (start, keys) = &*SCRIPT;
        let now = start.elapsed();
        keys.iter()
            .filter(|&&(from, to, _)| (from..to).contains(&now))
            .map(|&(_, _, vk)| vk)
            .collect()
    }

    /// The foreground window when it is one of ours (the title's, our
    /// fallback or a dialog).
    fn foreground() -> Option<HWND> {
        let mut process = 0;
        // SAFETY: plain queries; a null window gives process 0.
        unsafe {
            let window = GetForegroundWindow();
            GetWindowThreadProcessId(window, &mut process);
            (process == GetCurrentProcessId()).then_some(window)
        }
    }

    fn focused() -> bool {
        foreground().is_some()
    }

    fn held(key: VIRTUAL_KEY) -> bool {
        // SAFETY: a plain query.
        unsafe { GetAsyncKeyState(i32::from(key)) < 0 }
    }

    /// The host's cursor. Under Wine the clip ([`Mouse`]) is also what makes
    /// the pointer's motion relative: while the cursor is clipped, Wine grabs
    /// the pointer and moves the cursor by the device's own motion, so an X
    /// server that cannot warp the host pointer (Xwayland) still gives
    /// motion, and the desktop pointer stays in the window.
    pub(super) struct HostPointer;

    impl Pointer for HostPointer {
        fn see(&mut self) -> Seen {
            see()
        }

        fn set_clip(&mut self, clip: Option<Rect>) -> bool {
            let host = clip.map(|c| RECT {
                left: c.left,
                top: c.top,
                right: c.right,
                bottom: c.bottom,
            });
            let to = host.as_ref().map_or(std::ptr::null(), |r| r as *const RECT);
            // SAFETY: a valid rectangle, or null to free the cursor.
            unsafe { ClipCursor(to) != 0 }
        }

        fn clip(&mut self) -> Option<Rect> {
            let mut clip = RECT::default();
            // SAFETY: room for the rectangle.
            (unsafe { GetClipCursor(&mut clip) } != 0).then_some(rect(clip))
        }

        fn move_to(&mut self, (x, y): (i32, i32)) {
            // SAFETY: a plain call; the point is inside our clip.
            unsafe { SetCursorPos(x, y) };
        }

        fn focus_lost(&mut self) -> bool {
            FOCUS_LOST.swap(false, Ordering::Relaxed)
        }
    }

    fn rect(r: RECT) -> Rect {
        Rect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }

    /// The mouse as [`Capture`] sees it: nothing unless the foreground
    /// window is a title window, so a dialog of ours never captures it.
    fn see() -> Seen {
        let Some(window) = foreground().filter(|&w| crate::modules::lifecycle::is_title_window(w))
        else {
            return Seen::default();
        };
        let mut info = WINDOWINFO {
            cbSize: size_of::<WINDOWINFO>() as u32,
            ..Default::default()
        };
        let mut cursor = POINT::default();
        // SAFETY: plain queries into our own structures.
        let (shown, known, found) = unsafe {
            (
                IsIconic(window) == 0,
                GetWindowInfo(window, &mut info) != 0,
                GetCursorPos(&mut cursor) != 0,
            )
        };
        let client = info.rcClient;
        if !shown || !known || !found || client.right <= client.left || client.bottom <= client.top
        {
            return Seen::default();
        }
        let buttons = [
            (VK_LBUTTON, MOUSE_LEFT),
            (VK_RBUTTON, MOUSE_RIGHT),
            (VK_MBUTTON, MOUSE_MIDDLE),
            (VK_XBUTTON1, MOUSE_BACK),
            (VK_XBUTTON2, MOUSE_FORWARD),
        ];
        Seen {
            window: Some(rect(client)),
            cursor: (cursor.x, cursor.y),
            buttons: buttons
                .iter()
                .filter(|(key, _)| held(*key))
                .fold(0, |all, (_, bit)| all | bit),
            escape: held(VK_ESCAPE),
        }
    }

    #[test]
    fn host_pads_convert_to_console_pads() {
        let pad = XINPUT_GAMEPAD {
            wButtons: XINPUT_GAMEPAD_A | XINPUT_GAMEPAD_START | XINPUT_GAMEPAD_DPAD_LEFT,
            bLeftTrigger: 255,
            sThumbLX: -32768,
            sThumbLY: 32767,
            ..Default::default()
        };
        let state = convert(&pad);
        assert_eq!(state.buttons, A | MENU | LEFT);
        assert_eq!(
            (
                state.left_trigger,
                state.left_x,
                state.left_y,
                state.right_x
            ),
            (1.0, -1.0, 1.0, 0.0)
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::HleResolver;
    use crate::HostPolicy;
    use victory_loader::Provenance;
    use victory_pe::ImportSymbol;

    #[derive(Default)]
    struct FakeHost {
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        /// Buttons stay; motion is taken by one poll.
        mouse: MouseInput,
        rumble: Vec<(usize, u16, u16)>,
    }

    struct Fake(Arc<Mutex<FakeHost>>);

    impl Source for Fake {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            self.0.lock().unwrap().pads[slot]
        }
        fn keys(&mut self) -> Vec<KeyState> {
            self.0.lock().unwrap().keys.clone()
        }
        fn mouse(&mut self) -> MouseInput {
            let mouse = &mut self.0.lock().unwrap().mouse;
            MouseInput {
                motion: std::mem::take(&mut mouse.motion),
                ..*mouse
            }
        }
        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            self.0.lock().unwrap().rumble.push((slot, low, high));
        }
    }

    fn fake() -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        fake_with(false)
    }

    fn fake_with(one_pad: bool) -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        let host = Arc::new(Mutex::new(FakeHost::default()));
        let hub = Hub::new(
            Box::new(Fake(Arc::clone(&host))),
            one_pad,
            LookSettings::default(),
        );
        hub.poll();
        (hub, host)
    }

    fn key(virtual_key: u8) -> KeyState {
        KeyState {
            scan_code: u32::from(virtual_key) + 0x100,
            virtual_key,
            ..Default::default()
        }
    }

    fn slot<F: Copy, T>(object: *const T, offset: usize) -> F {
        // SAFETY: our objects start with their table, which has the slot.
        let f = unsafe { *(*(object as *const *const *const ())).add(offset / 8) };
        // SAFETY: the caller names the slot's signature.
        unsafe { std::mem::transmute_copy(&f) }
    }

    /// Device callbacks seen: device index, current and previous status.
    type Log = Mutex<Vec<(usize, u32, u32)>>;

    extern "system" fn record(
        _token: u64,
        context: *mut c_void,
        device: *const Device,
        _timestamp: u64,
        current: u32,
        previous: u32,
    ) {
        // SAFETY: the test passes its log and one of the hub's devices.
        let (log, index) = unsafe { (&*(context as *const Log), (*device).index) };
        log.lock().unwrap().push((index, current, previous));
    }

    fn register(hub: &Hub, kind: u32, enumeration: u32, log: &Log) -> u64 {
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let mut token = 0;
        let context = log as *const Log as *mut c_void;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let hr = register(
            &hub.object,
            std::ptr::null(),
            kind,
            1,
            enumeration,
            context,
            Some(record),
            &mut token,
        );
        assert_eq!(hr, S_OK);
        assert_ne!(token, 0);
        token
    }

    fn result(hr: HRESULT, out: *const c_void) -> Result<*const Reading, HRESULT> {
        if hr == S_OK {
            Ok(out.cast())
        } else {
            assert!(out.is_null());
            Err(hr)
        }
    }

    fn current(hub: &Hub, kind: u32, device: usize) -> Result<*const Reading, HRESULT> {
        type Current = extern "system" fn(This, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let hr = slot::<Current, _>(&hub.object, 0x20)(
            &hub.object,
            kind,
            &hub.devices[device],
            &mut out,
        );
        result(hr, out)
    }

    fn walk(hub: &Hub, offset: usize, from: *const Reading) -> Result<*const Reading, HRESULT> {
        type Walk = extern "system" fn(This, *const Reading, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let device = reading(from).device;
        let hr =
            slot::<Walk, _>(&hub.object, offset)(&hub.object, from, GAMEPAD_KIND, device, &mut out);
        result(hr, out)
    }

    fn pad(reading: *const Reading) -> PadState {
        let mut state = PadState::default();
        let get =
            slot::<extern "system" fn(*const Reading, *mut PadState) -> bool, _>(reading, 0xb0);
        assert!(get(reading, &mut state));
        state
    }

    fn stamp(reading: *const Reading) -> (u64, u64) {
        let time = slot::<extern "system" fn(*const Reading) -> u64, _>(reading, 0x28);
        let sequence = slot::<extern "system" fn(*const Reading, u32) -> u64, _>(reading, 0x20);
        (time(reading), sequence(reading, GAMEPAD_KIND))
    }

    fn release(reading: *const Reading) {
        slot::<extern "system" fn(*const Reading) -> u32, _>(reading, 0x10)(reading);
    }

    #[test]
    fn ordinal_one_resolves_to_create() {
        let r = HleResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("GameInput.dll", &ImportSymbol::Ordinal(1)),
            Some(Provenance::Hle)
        );
        assert_eq!(r.classify("GameInput.dll", &ImportSymbol::Ordinal(2)), None);
    }

    #[test]
    fn pads_connect_report_and_disconnect() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        // Blocking enumeration ran before registration returned: with no
        // host pad the keyboard's stand-in is the only one.
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        host.lock().unwrap().pads[1] = Some(PadState {
            buttons: A,
            left_x: 0.5,
            ..Default::default()
        });
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[1..],
            [(1, STATUS_CONNECTED, 0), (FALLBACK, 0, STATUS_CONNECTED)]
        );

        let first = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(first).buttons, A);
        assert_eq!(pad(first).left_x, 0.5);
        // No change, no new reading.
        hub.poll();
        let again = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(again, first);
        release(again);

        std::thread::sleep(Duration::from_millis(2));
        host.lock().unwrap().pads[1].as_mut().unwrap().buttons = B;
        hub.poll();
        let second = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(second).buttons, B);
        let (t1, s1) = stamp(first);
        let (t2, s2) = stamp(second);
        assert!(t2 > t1 && s2 == s1 + 1);
        assert!(t2 <= now());

        // The title's edge check: previous of current, and back again.
        assert_eq!(walk(hub, 0x30, second), Ok(first));
        assert_eq!(walk(hub, 0x28, first), Ok(second));
        assert_eq!(walk(hub, 0x28, second), Err(READING_NOT_FOUND));
        assert_eq!(walk(hub, 0x30, first), Err(READING_NOT_FOUND));
        for r in [first, first, second, second] {
            release(r);
        }

        host.lock().unwrap().pads[1] = None;
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[3..],
            [(1, 0, STATUS_CONNECTED), (FALLBACK, STATUS_CONNECTED, 0)]
        );
        assert_eq!(current(hub, GAMEPAD_KIND, 1), Err(DEVICE_DISCONNECTED));
        // The stand-in is back, at rest.
        let rest = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(pad(rest), PadState::default());
        release(rest);
    }

    #[test]
    fn keys_feed_the_keyboard_and_the_stand_in_pad() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, KEYBOARD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(KEYBOARD, STATUS_CONNECTED, 0)]);

        let keys = vec![key(b'W'), key(0x0D), key(b'L'), key(b'C')];
        host.lock().unwrap().keys = keys.clone();
        hub.poll();
        let r = current(hub, KEYBOARD_KIND, KEYBOARD).unwrap();
        let count = slot::<extern "system" fn(*const Reading) -> u32, _>(r, 0x70);
        let state =
            slot::<extern "system" fn(*const Reading, u32, *mut KeyState) -> u32, _>(r, 0x78);
        assert_eq!(count(r), 4);
        let mut got = [KeyState::default(); 8];
        assert_eq!(state(r, 8, got.as_mut_ptr()), 4);
        assert_eq!(got[..4], keys[..]);
        release(r);

        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(
            pad(r),
            PadState {
                buttons: A,
                left_y: 1.0,
                right_x: 1.0,
                right_trigger: 1.0,
                ..Default::default()
            }
        );
        release(r);
        // A keyboard reading is no gamepad reading.
        assert_eq!(current(hub, GAMEPAD_KIND, KEYBOARD), Err(READING_NOT_FOUND));
    }

    #[test]
    fn a_host_pad_feeds_the_one_pad_which_never_goes() {
        let (hub, host) = fake_with(true);
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);
        let pad = |hub: &Hub| match &hub.state().slots[FALLBACK].history.back().unwrap().value {
            Value::Pad(p) => *p,
            Value::Keys(_) => unreachable!(),
        };
        host.lock().unwrap().keys = vec![key(b'D')];
        hub.poll();
        assert_eq!(pad(hub).left_x, 1.0);
        // A pad is switched on, in any slot: its input arrives, beside the
        // keyboard's, and no device comes or goes.
        host.lock().unwrap().pads[2] = Some(PadState { buttons: A, left_y: -0.5, ..Default::default() });
        hub.poll();
        let p = pad(hub);
        assert_eq!((p.buttons, p.left_x, p.left_y), (A, 1.0, -0.5));
        let device: *const Device = &hub.devices[FALLBACK];
        set_rumble(device, &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(host.lock().unwrap().rumble, [(2, u16::MAX, 0)]);
        // And off again.
        host.lock().unwrap().pads[2] = None;
        host.lock().unwrap().keys.clear();
        hub.poll();
        assert_eq!(pad(hub), PadState::default());
        assert_eq!(log.lock().unwrap().len(), 1);
        assert!(!hub.state().slots[2].connected);
    }

    #[test]
    fn the_mouse_feeds_the_one_pad_beside_the_keyboard_and_host_pads() {
        let (hub, host) = fake_with(true);
        // As the title reads it: the current gamepad reading, any device.
        let pad = |hub: &Hub| {
            type Current = extern "system" fn(This, u32, *const Device, Out) -> HRESULT;
            let mut out = std::ptr::null();
            let hr = slot::<Current, _>(&hub.object, 0x20)(
                &hub.object,
                GAMEPAD_KIND,
                std::ptr::null(),
                &mut out,
            );
            let r = result(hr, out).unwrap();
            let state = pad(r);
            release(r);
            state
        };
        // A host pad is connected: keys and the mouse still count.
        host.lock().unwrap().pads[0] = Some(PadState {
            left_x: -0.5,
            ..Default::default()
        });
        host.lock().unwrap().keys = vec![key(b'W')];
        host.lock().unwrap().mouse = MouseInput {
            motion: (40, -10),
            buttons: MOUSE_LEFT | MOUSE_RIGHT | MOUSE_MIDDLE,
            captured: true,
        };
        hub.poll();
        let p = pad(hub);
        assert_eq!((p.left_x, p.left_y), (-0.5, 1.0));
        assert_eq!(
            (p.left_trigger, p.right_trigger, p.buttons),
            (1.0, 1.0, RIGHT_STICK)
        );
        assert!(p.right_x > 0.265 && p.right_y > 0.0 && p.right_x > p.right_y);
        // The mouse stops: the stick comes back to rest, exactly (each
        // poll counts as 1 ms at least, so 400 are enough).
        let still = MouseInput {
            captured: true,
            ..Default::default()
        };
        host.lock().unwrap().mouse = still;
        for _ in 0..400 {
            hub.poll();
        }
        let p = pad(hub);
        assert_eq!((p.right_x, p.right_y, p.right_trigger), (0.0, 0.0, 0.0));

        // Released while turning fast: the stick is at rest in that poll,
        // and the speed is not carried into the next capture.
        host.lock().unwrap().mouse = MouseInput {
            motion: (400, 0),
            ..still
        };
        hub.poll();
        assert_eq!(pad(hub).right_x, 1.0);
        host.lock().unwrap().mouse = MouseInput::default();
        hub.poll();
        assert_eq!((pad(hub).right_x, pad(hub).right_y), (0.0, 0.0));
        host.lock().unwrap().mouse = still;
        hub.poll();
        assert_eq!(pad(hub).right_x, 0.0);
    }

    #[test]
    fn with_a_pad_of_its_own_per_host_pad_the_mouse_goes_with_the_stand_in() {
        let (hub, host) = fake();
        host.lock().unwrap().mouse = MouseInput {
            buttons: MOUSE_LEFT,
            captured: true,
            ..Default::default()
        };
        hub.poll();
        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(pad(r).right_trigger, 1.0);
        release(r);
        host.lock().unwrap().pads[1] = Some(PadState::default());
        hub.poll();
        assert_eq!(
            current(hub, GAMEPAD_KIND, FALLBACK),
            Err(DEVICE_DISCONNECTED)
        );
        let r = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(r).right_trigger, 0.0);
        release(r);
    }

    const WINDOW: Rect = Rect {
        left: 100,
        top: 50,
        right: 900,
        bottom: 650,
    };

    fn seen(cursor: (i32, i32), buttons: u8) -> Seen {
        Seen {
            window: Some(WINDOW),
            cursor,
            buttons,
            escape: false,
        }
    }

    #[test]
    fn a_click_in_the_window_captures_the_mouse_and_esc_or_focus_releases_it() {
        let center = WINDOW.center();
        let mut capture = Capture::default();
        // Moving and right-clicking does nothing; a left click outside the
        // client area (the title bar) neither.
        assert_eq!(
            capture.step(seen((300, 300), MOUSE_RIGHT)),
            MouseInput::default()
        );
        assert_eq!(capture.step(seen((300, 300), 0)), MouseInput::default());
        assert_eq!(
            capture.step(seen((300, 20), MOUSE_LEFT)),
            MouseInput::default()
        );
        assert_eq!(capture.step(seen((300, 20), 0)), MouseInput::default());
        assert!(capture.window.is_none());
        // No click without the foreground: the host reports no buttons.
        assert_eq!(capture.step(Seen::default()), MouseInput::default());
        assert!(capture.window.is_none());

        // A left click inside captures; it is no button press, and the
        // first poll has no motion (the cursor goes to the centre).
        let captured = MouseInput {
            captured: true,
            ..Default::default()
        };
        assert_eq!(capture.step(seen((300, 300), MOUSE_LEFT)), captured);
        assert_eq!(capture.window, Some(WINDOW));
        let at = |dx: i32, dy: i32| (center.0 + dx, center.1 + dy);
        assert_eq!(capture.step(seen(at(5, -3), MOUSE_LEFT)).buttons, 0);
        let input = capture.step(seen(at(7, 2), 0));
        assert_eq!(
            input,
            MouseInput {
                motion: (7, 2),
                ..captured
            }
        );
        // Buttons count from now on.
        assert_eq!(
            capture.step(seen(center, MOUSE_LEFT | MOUSE_BACK)).buttons,
            MOUSE_LEFT | MOUSE_BACK
        );

        // Esc releases it.
        let mut esc = seen(at(1, 1), 0);
        esc.escape = true;
        assert_eq!(capture.step(esc), MouseInput::default());
        assert!(capture.window.is_none());
        assert_eq!(
            capture.step(seen(at(9, 9), MOUSE_RIGHT)),
            MouseInput::default()
        );

        // Captured again, then the window loses the foreground.
        capture.step(seen((300, 300), 0));
        capture.step(seen((300, 300), MOUSE_LEFT));
        assert!(capture.window.is_some());
        assert_eq!(capture.step(Seen::default()), MouseInput::default());
        assert!(capture.window.is_none());
        // Back in the foreground the mouse stays free until a click.
        assert_eq!(capture.step(seen(at(20, 0), 0)), MouseInput::default());
        assert!(capture.window.is_none());
    }

    #[test]
    fn a_moved_window_gives_no_motion_once() {
        let mut capture = Capture::default();
        capture.step(seen((300, 300), MOUSE_LEFT));
        let moved = Rect {
            left: WINDOW.left + 50,
            right: WINDOW.right + 50,
            ..WINDOW
        };
        let input = capture.step(Seen {
            window: Some(moved),
            cursor: WINDOW.center(),
            buttons: 0,
            escape: false,
        });
        assert_eq!(input.motion, (0, 0));
        assert_eq!(capture.window, Some(moved));
    }

    /// The system's cursor clip as [`Mouse`] sees it.
    struct FakePointer {
        seen: Seen,
        /// The clip in effect; `SCREEN` for none.
        clip: Rect,
        /// `set_clip` fails.
        refuse: bool,
        moves: Vec<(i32, i32)>,
        /// Taken by the next poll.
        focus_lost: bool,
    }

    const SCREEN: Rect = Rect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };

    impl FakePointer {
        fn new() -> FakePointer {
            FakePointer {
                seen: Seen::default(),
                clip: SCREEN,
                refuse: false,
                moves: Vec::new(),
                focus_lost: false,
            }
        }
    }

    impl Pointer for FakePointer {
        fn see(&mut self) -> Seen {
            self.seen
        }
        fn set_clip(&mut self, clip: Option<Rect>) -> bool {
            if self.refuse {
                return false;
            }
            self.clip = clip.unwrap_or(SCREEN);
            true
        }
        fn clip(&mut self) -> Option<Rect> {
            Some(self.clip)
        }
        fn move_to(&mut self, at: (i32, i32)) {
            self.moves.push(at);
        }
        fn focus_lost(&mut self) -> bool {
            std::mem::take(&mut self.focus_lost)
        }
    }

    /// `WINDOW`'s client area a pixel inside, as `Mouse` clips it.
    const CLIPPED: Rect = Rect {
        left: 101,
        top: 51,
        right: 899,
        bottom: 649,
    };

    /// A mouse captured by a click in `WINDOW`.
    fn captured_mouse(pointer: &mut FakePointer) -> Mouse {
        let mut mouse = Mouse::default();
        pointer.seen = seen((300, 300), MOUSE_LEFT);
        assert!(mouse.poll(pointer).captured);
        pointer.seen.buttons = 0;
        assert_eq!(pointer.clip, CLIPPED);
        mouse
    }

    #[test]
    fn the_captured_mouse_is_clipped_and_centred_and_esc_takes_off_only_our_clip() {
        let mut pointer = FakePointer::new();
        let mut mouse = captured_mouse(&mut pointer);
        assert_eq!(pointer.moves, [WINDOW.center()]);
        let (x, y) = WINDOW.center();
        pointer.seen.cursor = (x + 4, y - 2);
        let input = mouse.poll(&mut pointer);
        assert_eq!((input.motion, input.captured), ((4, -2), true));
        assert_eq!(pointer.clip, CLIPPED);
        // At the centre already: not moved again.
        pointer.seen.cursor = (x, y);
        mouse.poll(&mut pointer);
        assert_eq!(pointer.moves.len(), 2);

        pointer.seen.escape = true;
        assert_eq!(mouse.poll(&mut pointer), MouseInput::default());
        assert_eq!(pointer.clip, SCREEN);
    }

    #[test]
    fn a_refused_clip_captures_nothing_and_no_clip_is_taken_for_ours() {
        let mut pointer = FakePointer::new();
        let theirs = Rect {
            left: 10,
            top: 10,
            right: 20,
            bottom: 20,
        };
        pointer.clip = theirs;
        pointer.refuse = true;
        let mut mouse = Mouse::default();
        pointer.seen = seen((300, 300), MOUSE_LEFT);
        assert_eq!(mouse.poll(&mut pointer), MouseInput::default());
        assert!(mouse.capture.window.is_none() && mouse.clip.is_none());
        assert!(pointer.moves.is_empty());
        // Nothing is held and nothing is taken off later, though the clip
        // could be freed now.
        pointer.refuse = false;
        pointer.seen = seen((300, 300), 0);
        mouse.poll(&mut pointer);
        assert_eq!(pointer.clip, theirs);
        // A click after the system allows clipping again captures.
        pointer.seen.buttons = MOUSE_LEFT;
        assert!(mouse.poll(&mut pointer).captured);
        assert_eq!(pointer.clip, CLIPPED);
    }

    #[test]
    fn a_clip_taken_off_or_replaced_ends_the_capture_and_is_not_set_again() {
        let theirs = Rect {
            left: 0,
            top: 0,
            right: 640,
            bottom: 480,
        };
        // Taken off (the system, on a foreground change it reports late), or
        // replaced by another program's.
        for after in [SCREEN, theirs] {
            let mut pointer = FakePointer::new();
            let mut mouse = captured_mouse(&mut pointer);
            pointer.clip = after;
            pointer.seen.cursor = (200, 190);
            let moves = pointer.moves.len();
            assert_eq!(mouse.poll(&mut pointer), MouseInput::default());
            assert_eq!(pointer.clip, after);
            assert_eq!(pointer.moves.len(), moves);
            // Still the foreground window, the button up: it stays released.
            mouse.poll(&mut pointer);
            assert_eq!(pointer.clip, after);
            // A new click captures again.
            pointer.seen.buttons = MOUSE_LEFT;
            assert!(mouse.poll(&mut pointer).captured);
            assert_eq!(pointer.clip, CLIPPED);
        }
    }

    #[test]
    fn lost_activation_ends_the_capture_though_the_window_and_clip_stay() {
        // Wine with a window manager, an X11 window outside Wine taking the
        // keyboard: the title's window is still the foreground window, our
        // clip is still in place, and the cursor is dragged away.
        let mut pointer = FakePointer::new();
        let mut mouse = captured_mouse(&mut pointer);
        let moves = pointer.moves.len();
        pointer.focus_lost = true;
        pointer.seen.cursor = (200, 190);
        assert_eq!(mouse.poll(&mut pointer), MouseInput::default());
        assert!(mouse.capture.window.is_none());
        assert_eq!(pointer.clip, SCREEN);
        assert_eq!(pointer.moves.len(), moves);
        // The window still the foreground window and no new click: it stays
        // released.
        mouse.poll(&mut pointer);
        assert_eq!(pointer.clip, SCREEN);
        // A loss told while released is not held against the next click.
        pointer.focus_lost = true;
        mouse.poll(&mut pointer);
        pointer.seen.buttons = MOUSE_LEFT;
        assert!(mouse.poll(&mut pointer).captured);
        assert_eq!(pointer.clip, CLIPPED);
    }

    #[test]
    fn a_click_held_through_lost_activation_does_not_capture_again() {
        let mut pointer = FakePointer::new();
        let mut mouse = captured_mouse(&mut pointer);
        pointer.seen.buttons = MOUSE_LEFT;
        assert!(mouse.poll(&mut pointer).captured);
        pointer.focus_lost = true;
        assert_eq!(mouse.poll(&mut pointer), MouseInput::default());
        // Still held: no new click.
        assert_eq!(mouse.poll(&mut pointer), MouseInput::default());
        assert_eq!(pointer.clip, SCREEN);
        pointer.seen.buttons = 0;
        mouse.poll(&mut pointer);
        pointer.seen.buttons = MOUSE_LEFT;
        assert!(mouse.poll(&mut pointer).captured);
    }

    #[test]
    fn the_foreground_lost_frees_our_clip_and_a_moved_window_is_clipped_anew() {
        let mut pointer = FakePointer::new();
        let mut mouse = captured_mouse(&mut pointer);
        // The window moves (the system may drop the old clip for it).
        let moved = Rect {
            left: WINDOW.left + 50,
            right: WINDOW.right + 50,
            ..WINDOW
        };
        pointer.seen.window = Some(moved);
        pointer.clip = SCREEN;
        assert!(mouse.poll(&mut pointer).captured);
        assert_eq!(
            pointer.clip,
            Rect {
                left: CLIPPED.left + 50,
                right: CLIPPED.right + 50,
                ..CLIPPED
            }
        );
        // The window is no longer the foreground window: our clip is freed.
        pointer.seen = Seen::default();
        assert_eq!(mouse.poll(&mut pointer), MouseInput::default());
        assert_eq!(pointer.clip, SCREEN);
    }

    #[test]
    fn mouse_look_follows_the_mouse_speed_and_keeps_its_distance() {
        let poll = POLL.as_micros() as u64;
        let settings = LookSettings {
            speed: 1.0,
            dead_zone: 0.0,
        };
        let mut look = Look::new(settings);
        // A steady speed settles at its deflection: 20 px in 8 ms is
        // 2500 px/s, all the way; half of it, half way.
        let mut stick = (0.0, 0.0);
        for i in 1..=100 {
            stick = look.step((10, -10), i * poll);
        }
        let half = 1250.0 / FULL_TILT;
        assert!(
            (stick.0 - half).abs() < 0.01 && (stick.1 - half).abs() < 0.01,
            "{stick:?}"
        );

        // Motion arriving in bursts (once a title frame) turns the camera
        // as far as the same motion spread evenly.
        let turn = |bursts: bool| {
            let mut look = Look::new(settings);
            let mut sum = 0.0;
            for i in 1..=400_u64 {
                let motion = if i > 200 {
                    0
                } else if bursts {
                    if i % 4 == 0 { 8 } else { 0 }
                } else {
                    2
                };
                sum += look.step((motion, 0), i * poll).0;
            }
            sum
        };
        let (even, bursts) = (turn(false), turn(true));
        assert!((even - bursts).abs() / even < 0.02, "{even} {bursts}");

        // A dead zone: the slightest motion clears it.
        let mut look = Look::new(LookSettings::default());
        let (x, y) = look.step((0, 1), poll);
        assert!(x == 0.0 && (y + 0.265).abs() < 0.01, "{y}");
        let (x, _) = Look::new(LookSettings::default()).step((1000, 0), poll);
        assert_eq!(x, 1.0);
    }

    #[test]
    fn mouse_settings_parse_with_defaults_for_bad_values() {
        let default = LookSettings::default();
        assert_eq!(LookSettings::parse(None, None), default);
        assert_eq!(
            LookSettings::parse(Some(" 2.5 "), Some("0.1")),
            LookSettings {
                speed: 2.5,
                dead_zone: 0.1
            }
        );
        for (speed, dead_zone) in [("0", "-0.1"), ("-1", "1"), ("fast", "inf"), ("NaN", "")] {
            assert_eq!(LookSettings::parse(Some(speed), Some(dead_zone)), default);
        }
    }

    #[test]
    fn async_enumeration_waits_for_the_poll_and_unregistering_stops_callbacks() {
        let (hub, host) = fake();
        let log = Log::default();
        let token = register(hub, GAMEPAD_KIND, ASYNC_ENUMERATION, &log);
        assert!(log.lock().unwrap().is_empty());
        hub.poll();
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(unregister(&hub.object, token, 5000));
        host.lock().unwrap().pads[0] = Some(PadState::default());
        hub.poll();
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[test]
    fn unregistering_waits_for_a_running_callback() {
        use std::sync::atomic::AtomicBool;
        static INSIDE: AtomicBool = AtomicBool::new(false);
        static LEAVE: AtomicBool = AtomicBool::new(false);
        static FROM_INSIDE: AtomicU32 = AtomicU32::new(0);
        extern "system" fn slow(
            token: u64,
            context: *mut c_void,
            _: *const Device,
            _: u64,
            _: u32,
            _: u32,
        ) {
            // From inside its own callback, unregistering fails at once.
            let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(context, 0x68);
            if !unregister(context.cast(), token, 0) {
                FROM_INSIDE.fetch_add(1, Ordering::SeqCst);
            }
            INSIDE.store(true, Ordering::SeqCst);
            while !LEAVE.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let (hub, host) = fake();
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let mut token = 0;
        let context = (&raw const hub.object).cast_mut().cast();
        assert_eq!(
            register(&hub.object, std::ptr::null(), GAMEPAD_KIND, 1, 0, context, Some(slow), &mut token),
            S_OK
        );
        host.lock().unwrap().pads[1] = Some(PadState::default());
        let poller = std::thread::spawn(|| hub.poll());
        while !INSIDE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(!unregister(&hub.object, token, 5000), "times out while it runs");
        let leave = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            LEAVE.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        assert!(unregister(&hub.object, token, u64::MAX), "waits for it");
        assert!(start.elapsed() >= Duration::from_millis(40));
        assert_eq!(FROM_INSIDE.load(Ordering::SeqCst), 1);
        poller.join().unwrap();
        leave.join().unwrap();
    }

    #[test]
    fn devices_are_found_by_id_and_rumble_reaches_the_host() {
        let (hub, host) = fake();
        host.lock().unwrap().pads[2] = Some(PadState::default());
        hub.poll();
        let device = &hub.devices[2] as *const Device;
        let info = slot::<extern "system" fn(*const Device) -> *const u8, _>(device, 0x18);
        // SAFETY: the info has the id at +0x20.
        let id = unsafe { *(info(device).add(0x20) as *const [u8; 32]) };
        assert_ne!(id, [0; 32]);

        type Find = extern "system" fn(This, *const [u8; 32], Out) -> HRESULT;
        let find = slot::<Find, _>(&hub.object, 0x80);
        let mut out = std::ptr::null();
        assert_eq!(find(&hub.object, &id, &mut out), S_OK);
        assert_eq!(out, device.cast());
        assert_eq!(find(&hub.object, &[1; 32], &mut out), DEVICE_NOT_FOUND);
        assert!(out.is_null());

        type Rumble = extern "system" fn(*const Device, *const [f32; 4]);
        let rumble = slot::<Rumble, _>(device, 0x50);
        rumble(device, &[0.5, 1.0, 1.0, 1.0]);
        rumble(&hub.devices[FALLBACK], &[1.0; 4]);
        rumble(device, std::ptr::null());
        assert_eq!(host.lock().unwrap().rumble, [(2, 32767, 65535), (2, 0, 0)]);
    }
}
