use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::stream::StreamExt;
use gpui::*;
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};

/// Headless layout harness (G24): decode a command buffer through the real
/// decoder, render it in a gpui `TestAppContext` window (no GPU, no display),
/// and read element geometry back via `debug_bounds`. Compiled for unit tests
/// and for the `test-support` feature (benches / integration tests); the
/// staticlib build has neither and stays free of gpui's `test-support`.
#[cfg(any(test, feature = "test-support"))]
pub mod headless;

mod abi_constants;
use abi_constants::{
    ABI_VERSION, ALIGN_CENTER, ALIGN_DEFAULT, ALIGN_END, ALIGN_START, ALIGN_STRETCH,
    BUFFER_VERSION, CURSOR_ARROW, CURSOR_COL_RESIZE, CURSOR_CROSSHAIR, CURSOR_EW_RESIZE,
    CURSOR_GRAB, CURSOR_GRABBING, CURSOR_NONE, CURSOR_NOT_ALLOWED, CURSOR_NS_RESIZE,
    CURSOR_POINTER, CURSOR_ROW_RESIZE, CURSOR_TEXT, EVENT_ASYNC, EVENT_CLICK,
    EVENT_INPUT_CHANGED, EVENT_INPUT_SUBMIT, EVENT_KEY, EVENT_NAMED_KEY, EVENT_SCROLL, EVENT_TEXT,
    JUSTIFY_CENTER, JUSTIFY_DEFAULT, JUSTIFY_END,
    JUSTIFY_SPACE_AROUND, JUSTIFY_SPACE_BETWEEN, JUSTIFY_START, KEY_BACKSPACE, KEY_DELETE,
    KEY_DOWN, KEY_END, KEY_ENTER, KEY_ESCAPE, KEY_HOME, KEY_LEFT, KEY_PAGEUP, KEY_PAGEDOWN,
    KEY_RIGHT, KEY_TAB, KEY_UP, MOD_ALT, MOD_CTRL, MOD_FUNCTION, MOD_PLATFORM, MOD_SHIFT,
    OP_ADD_CHILD, OP_DIV, OP_SET_ALIGN, OP_SET_BG, OP_SET_BG_COLOR, OP_SET_BORDER,
    OP_SET_CENTER, OP_SET_CURSOR, OP_SET_FLEX, OP_SET_FLEX_ITEM, OP_SET_FOCUSABLE,
    OP_SET_FONT_FAMILY, OP_SET_FONT_WEIGHT, OP_SET_GAP, OP_SET_INSET, OP_SET_KEY,
    OP_SET_LINE_HEIGHT, OP_SET_MARGIN, OP_SET_MAX_SIZE, OP_SET_MIN_SIZE, OP_SET_ON_CLICK,
    OP_SET_OPACITY, OP_SET_OVERFLOW, OP_SET_PADDING, OP_SET_PADDING_SIDES, OP_SET_POSITION,
    OP_SET_ROOT, OP_SET_ROUNDED, OP_SET_SCROLL_ID, OP_SET_SHADOW, OP_SET_SIZE, OP_SET_TAB_INDEX,
    OP_SET_TAB_STOP,
    OP_SET_TEXT_ALIGN, OP_SET_TEXT_COLOR, OP_SET_TEXT_SIZE, OP_SET_WHITESPACE, OP_TEXT,
    OP_TEXT_INPUT, OP_TEXT_RUN, OVERFLOW_HIDDEN, OVERFLOW_SCROLL, OVERFLOW_VISIBLE,
    POSITION_ABSOLUTE, POSITION_RELATIVE, RUN_STYLE_BACKGROUND, RUN_STYLE_COLOR,
    RUN_STYLE_ITALIC, RUN_STYLE_STRIKETHROUGH, RUN_STYLE_UNDERLINE, RUN_STYLE_WEIGHT,
    TEXT_ALIGN_CENTER, TEXT_ALIGN_DEFAULT, TEXT_ALIGN_JUSTIFY, TEXT_ALIGN_LEFT,
    TEXT_ALIGN_RIGHT, WHITESPACE_DEFAULT, WHITESPACE_NORMAL, WHITESPACE_NOWRAP, WHITESPACE_PRE,
    WHITESPACE_PRE_WRAP,
};

// Reference the version as a build-time sanity anchor until runtime FFI negotiation exists.
const _: () = assert!(ABI_VERSION > 0);

/// Committed trees, one slot per view id. `render` reads the slot for its own
/// view; a successful `gpui_build_tree` swaps a freshly built tree into it.
/// `None` = no tree committed yet (the view renders empty).
static VIEWS: Mutex<Vec<Option<UiNode>>> = Mutex::new(Vec::new());

/// Serializes every test that mutates the process-global `VIEWS`. The unit
/// tests (`mod tests`), the headless golden tests (`headless_tests`, via
/// `headless::layout_bounds`), and the fuzz tests (`fuzz_tests`) all commit
/// into `VIEWS`; without one shared lock they would run concurrently and
/// clobber each other's trees (e.g. `mod tests`'s `clear_state()` wiping a
/// slot mid-render). One lock, held for the duration of each test, keeps them
/// mutually exclusive.
#[cfg(any(test, feature = "test-support"))]
static TEST_VIEWS_MUTEX: Mutex<()> = Mutex::new(());

/// Operation completed successfully.
pub const GPUI_STATUS_OK: i32 = 0;
/// A handle was negative, out of range, duplicated, or could not be allocated.
pub const GPUI_STATUS_INVALID_HANDLE: i32 = -1;
/// The handle refers to the wrong kind of node for the requested operation.
pub const GPUI_STATUS_WRONG_NODE_KIND: i32 = -2;
/// The node was already moved into another node by `gpui_add_child`.
pub const GPUI_STATUS_NODE_ABSENT: i32 = -3;
/// An internal panic was caught before it could cross the C boundary.
pub const GPUI_STATUS_INTERNAL_PANIC: i32 = -4;
/// The command buffer header magic or version did not match.
pub const GPUI_STATUS_BAD_BUFFER_VERSION: i32 = -5;
/// The command buffer ended mid-field, or carried a truncated/oversized payload.
pub const GPUI_STATUS_TRUNCATED_BUFFER: i32 = -6;
/// The command buffer named an opcode this build does not recognize.
pub const GPUI_STATUS_UNKNOWN_OPCODE: i32 = -7;
/// `gpui_build_tree` finished without an `OP_SET_ROOT` designating a root.
pub const GPUI_STATUS_NO_ROOT: i32 = -8;
/// Two or more nodes in the committed tree carry the same explicit key.
pub const GPUI_STATUS_DUPLICATE_KEY: i32 = -9;
/// `gpui_update_text` found no node carrying the requested explicit key in the
/// committed tree for the view (the view may have no tree, or the key is absent
/// / belongs to a text node). Callers treat this as "fall back to a full
/// `gpui_build_tree` rebuild".
pub const GPUI_STATUS_KEY_NOT_FOUND: i32 = -10;
/// The async injection queue is full (back-pressure): the producer should
/// retry later, coalesce, or drop — the library never blocks, discards, or
/// merges on its own (RFC 0002 §3.2).
pub const GPUI_STATUS_QUEUE_FULL: i32 = -11;
/// The async injection payload exceeds the per-entry size limit.
pub const GPUI_STATUS_PAYLOAD_TOO_LARGE: i32 = -12;
/// `gpui_input_set_text` was rejected because the input is mid-IME-composition
/// (a marked range is active). Overwriting the buffer would destroy the
/// composition the user sees; retry after the composition commits (RFC 0003
/// §3.5).
pub const GPUI_STATUS_BUSY_COMPOSING: i32 = -13;
/// A command buffer carried a non-finite `f32` operand (NaN or ±infinity) for a
/// geometry field. Rejected at decode time so the value never reaches taffy:
/// measured behavior is that `f32::INFINITY` lays out to infinite bounds and an
/// infinite gap makes a sibling's width `NaN`, which then propagates silently
/// through paint and hit-testing (issue #75).
pub const GPUI_STATUS_INVALID_FLOAT: i32 = -14;
/// The committed tree nests deeper than [`MAX_TREE_DEPTH`]. The three functions
/// that walk a tree (`render_node`, `collect_text_contents`, `update_keyed_text`)
/// are recursive; `stacker` grows the stack under them so a legitimate deep tree
/// still renders, but an unbounded one would grow until the allocator gives up.
/// Rejected before commit so the depth is capped once, at the boundary, rather
/// than separately in each walker (issue #74).
pub const GPUI_STATUS_DEPTH_EXCEEDED: i32 = -15;
/// An `OP_TEXT_RUN` record is semantically invalid for the text node it
/// targets: out of bounds, not on a `char` boundary, overlapping/unsorted
/// against the previous run, or carrying unknown style flag bits. Rejection is
/// per-buffer (the tree is not committed) because gpui's run machinery panics
/// on ranges like these — `StyledText::compute_runs` subtracts range starts
/// and `with_runs` asserts the runs tile the text — so a lenient decoder would
/// trade a diagnosable status for a paint-time abort (issue #91).
pub const GPUI_STATUS_INVALID_TEXT_RUN: i32 = -16;


// Rust -> MoonBit callback. MoonBit native does not emit a stable C export
// symbol for an executable build, so we bind directly to the compiled MoonBit
// function's mangled symbol. Rather than hard-code that (fragile) name, the
// `extern` block below is generated by build.rs from `mb_symbol.txt`, which
// `build.sh` fills by extracting the real `dispatch_entry` symbol from MoonBit's
// build output — so renames / toolchain mangling changes are tracked
// automatically. The callback is invoked on the main thread, inside the
// (MoonBit-initiated) GPUI event loop — safe under MoonBit's reference-counted
// runtime.
//
// Versioned event envelope (abi_version 4): the five i32 slots carry
//   (abi_version, event_kind, view, data_a, data_b)
// Slot 0 is always ABI_VERSION so MoonBit can reject a stale Rust binary at
// runtime. Slot 1 selects the event kind. Slot 2 is the view id (index into
// VIEWS, from FfiView.view) and routes the rebuild target. Slots 3–4 are
// kind-dependent:
//   EVENT_CLICK: data_a = click_id, data_b = 0
//   EVENT_KEY:   data_a = codepoint (single-char key), data_b = modifier bits
//   EVENT_TEXT:  data_a = token (index into EVENT_QUEUE), data_b = byte length
//   EVENT_SCROLL: data_a = scroll_id (OP_SET_SCROLL_ID), data_b = 0
// For EVENT_TEXT the UTF-8 payload lives in a Rust-owned queue; MoonBit copies
// it synchronously via `gpui_event_copy_text` before returning from dispatch.
// EVENT_SCROLL is notify-only (RFC 0003's notify-then-pull contract): the
// offset itself is read via `gpui_scroll_copy_state`, so a coalesced or missed
// event can never leave MoonBit acting on stale numbers.
//
// Generates: `unsafe extern "C" { #[link_name = "_M0FP…15dispatch__entry"] fn mb_dispatch(version: i32, kind: i32, view: i32, data_a: i32, data_b: i32) -> i32; }`
include!(concat!(env!("OUT_DIR"), "/mb_extern.rs"));

/// Rust-owned event payload queue. Text events store their UTF-8 bytes here;
/// the callback passes a token (index) and byte length so MoonBit can copy
/// the payload via `gpui_event_copy_text`. Entries are valid only during the
/// synchronous dispatch call — MoonBit must copy before returning, and every
/// dispatch site clears the queue immediately after `mb_dispatch` returns
/// (#70), so it holds at most one entry at a time and can never grow with the
/// number of events delivered.
static EVENT_QUEUE: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

// --- Async event injection (RFC 0002) --------------------------------------
//
// External native code pushes opaque payloads from any thread via
// `gpui_post_event`; a foreground drain pump (installed by `run_window`)
// moves them onto the main thread as `EVENT_ASYNC` dispatches. The queue is
// bounded on both axes (entry count and per-entry size) and the drain pops
// ownership, so unbounded growth is structurally impossible — the lesson of
// #70 ("a queue with no release contract always leaks").

/// Maximum number of queued injection entries (RFC 0002 §6-1). A full queue
/// fails `gpui_post_event` with `GPUI_STATUS_QUEUE_FULL` instead of blocking.
pub const INJECT_QUEUE_MAX_ENTRIES: usize = 1024;

/// Maximum payload size of a single injection entry, in bytes (RFC 0002
/// §6-1). Larger payloads fail with `GPUI_STATUS_PAYLOAD_TOO_LARGE`.
pub const INJECT_PAYLOAD_MAX_BYTES: usize = 1024 * 1024;

/// One queued injection: the destination view id and the copied payload.
struct InjectEntry {
    view: i32,
    payload: Vec<u8>,
}

/// The process-wide injection queue plus its wake channel. `entries` is a
/// single FIFO across all producers (Mutex acquisition order); `wake_tx`
/// carries only `()` — the data lives in the queue, and the drain pump empties
/// it on every wake, so coalesced wakes lose nothing.
///
/// `wake_tx` is behind a `Mutex` because it is swapped each time a window
/// starts: the receiver lives with that window's drain pump, so a sender left
/// over from a closed window would wake nothing. A post that arrives before
/// any window installs a disconnected sender (its receiver is dropped), so
/// the send is a harmless no-op and the entry simply waits in the queue for
/// the first window's startup drain.
struct InjectQueue {
    entries: Mutex<VecDeque<InjectEntry>>,
    wake_tx: Mutex<UnboundedSender<()>>,
}

static INJECT: OnceLock<InjectQueue> = OnceLock::new();

/// Serializes every test that touches the process-global injection queue
/// (`INJECT`): the `gpui_post_event` unit tests (`mod tests`) and the
/// headless drain-pump tests (`async_inject_tests`) post into and drain the
/// same queue, so one shared lock keeps them from interleaving.
#[cfg(test)]
static INJECT_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Ensure the injection queue exists and attach a fresh wake channel, handing
/// the receiver to the caller (which spawns the drain pump with it). Called on
/// the main thread at window startup; swapping the sender is what re-arms wake
/// delivery for this window's pump.
fn install_inject_queue() -> UnboundedReceiver<()> {
    let (wake_tx, wake_rx) = unbounded::<()>();
    match INJECT.get() {
        Some(queue) => {
            *queue.wake_tx.lock().unwrap_or_else(|e| e.into_inner()) = wake_tx;
        }
        None => {
            let _ = INJECT.set(InjectQueue {
                entries: Mutex::new(VecDeque::new()),
                wake_tx: Mutex::new(wake_tx),
            });
        }
    }
    wake_rx
}

/// Swap in a disconnected wake sender so the current drain pump's receiver
/// yields `None` and it exits. Test-only: gives async-injection tests a clean
/// teardown so a pump never survives into the next test (the injection queue
/// and recorder are process globals).
#[cfg(any(test, feature = "test-support"))]
pub fn stop_drain_pump() {
    if let Some(queue) = INJECT.get() {
        let (wake_tx, wake_rx) = unbounded::<()>();
        drop(wake_rx);
        *queue.wake_tx.lock().unwrap_or_else(|e| e.into_inner()) = wake_tx;
    }
}

/// Push an event from any thread into the injection queue (RFC 0002 §3.1).
/// Non-blocking: the payload is copied under the queue lock and the call
/// returns immediately. The payload is opaque bytes — the library never
/// interprets it; framing is a contract between the producer and the MoonBit
/// `Event::Async` handler.
///
/// `view` is the destination view id (index into `VIEWS`); a negative value
/// fails up front, an unknown one is dropped at drain time. `ptr` is borrowed
/// only for the duration of the call and copied internally (same contract as
/// every other FFI here). `len` may be 0: an entry can carry "something
/// happened" with no payload.
///
/// Returns `GPUI_STATUS_OK`, `GPUI_STATUS_INVALID_HANDLE` (negative view, or
/// a null pointer with a nonzero length), `GPUI_STATUS_PAYLOAD_TOO_LARGE`, or
/// `GPUI_STATUS_QUEUE_FULL`. Posts made before any window starts are queued
/// and delivered by the first drain after startup.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_post_event(view: i32, ptr: *const u8, len: i32) -> i32 {
    ffi_export("gpui_post_event", || {
        if view < 0 {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        if len < 0 {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        let len = len as usize;
        if len > INJECT_PAYLOAD_MAX_BYTES {
            return GPUI_STATUS_PAYLOAD_TOO_LARGE;
        }
        if ptr.is_null() && len != 0 {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        // SAFETY: `ptr` points to at least `len` readable bytes for the
        // duration of this call (the standard FFI borrow contract); `len` is
        // validated above. A null pointer is only reachable here with len 0.
        let payload = if len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
        };
        let queue = INJECT.get_or_init(|| {
            // A post that arrives before any window: install the queue with a
            // disconnected sender (its receiver is dropped immediately), so
            // the wake below is a no-op and the entry waits for the first
            // window's startup drain.
            let (wake_tx, wake_rx) = unbounded::<()>();
            drop(wake_rx);
            InjectQueue {
                entries: Mutex::new(VecDeque::new()),
                wake_tx: Mutex::new(wake_tx),
            }
        });
        {
            let mut entries = queue.entries.lock().unwrap_or_else(|e| e.into_inner());
            if entries.len() >= INJECT_QUEUE_MAX_ENTRIES {
                return GPUI_STATUS_QUEUE_FULL;
            }
            entries.push_back(InjectEntry { view, payload });
        }
        // Wake the drain pump. A closed channel (no window running) is fine:
        // the entry stays queued for the next window's startup drain.
        let _ = queue
            .wake_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unbounded_send(());
        GPUI_STATUS_OK
    })
}

/// Pop the oldest queued injection entry, if any. The queue lock is held only
/// for the pop; the caller owns the payload afterwards.
fn pop_injected() -> Option<InjectEntry> {
    INJECT
        .get()?
        .entries
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pop_front()
}

/// Main-thread registry of open views, keyed by view id (RFC 0002 §3.4). The
/// drain pump has no entity context of its own, so it routes `cx.notify()`
/// through the `WeakEntity` registered here when the view opened. Main-thread
/// only: `WeakEntity` is not `Send`, so this cannot live in a `Mutex`-guarded
/// global (same reason as `ScrollHandle`; `architecture.md` §3).
#[derive(Default)]
struct ViewRegistry(HashMap<i32, WeakEntity<FfiView>>);

impl Global for ViewRegistry {}

/// Register an open view so the drain pump can notify it. Called on the main
/// thread when the `FfiView` entity is created. Takes a `WeakEntity` (not the
/// `Entity`) because the only non-test-gated way to reach the window's root
/// entity from a `WindowHandle` is `read`, which hands the entity to a closure
/// and drops it on return — the caller downgrades inside that closure.
fn register_view(cx: &mut App, view: i32, weak: WeakEntity<FfiView>) {
    cx.default_global::<ViewRegistry>().0.insert(view, weak);
}

/// Notify the view that its committed tree changed (dispatch returned 1). A
/// closed window (failed `upgrade`) is dropped silently: the dispatch already
/// ran, and there is no UI left to refresh.
fn notify_view(cx: &mut AsyncApp, view: i32) {
    let _ = cx.update(|app| notify_view_app(app, view));
}

/// `App`-level flavor of [`notify_view`], usable from any main-thread context
/// that dereferences to `App` (the text-input commit paths, RFC 0003).
fn notify_view_app(app: &mut App, view: i32) {
    // Clone the weak handle out before borrowing `app` mutably for the
    // update (the registry borrow and the entity update cannot overlap).
    let weak = app.default_global::<ViewRegistry>().0.get(&view).cloned();
    if let Some(weak) = weak {
        let _ = weak.update(app, |_, cx| cx.notify());
    }
}

/// Dispatch one injected entry as `EVENT_ASYNC` (RFC 0002 §3.4). The payload
/// rides the existing `EVENT_QUEUE` token+copy mechanism — MoonBit copies it
/// with `gpui_event_copy_text` during the synchronous dispatch — and the queue
/// is cleared immediately after dispatch returns, the same #70 contract the
/// `EVENT_TEXT` path follows. Returns the dispatch's `changed` flag.
fn dispatch_injected(view: i32, payload: Vec<u8>) -> i32 {
    let len = payload.len() as i32;
    let token = {
        let mut q = EVENT_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        q.push(payload);
        (q.len() - 1) as i32
    };
    let changed = unsafe { mb_dispatch(ABI_VERSION, EVENT_ASYNC, view, token, len) };
    EVENT_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    changed
}

/// Drain every queued injection entry, dispatching each as `EVENT_ASYNC` and
/// notifying the view when the dispatch reports a change. Runs to completion
/// on the main thread (called from the drain pump), so a wake that arrives
/// mid-drain simply schedules another pass.
///
/// An entry addressed to a view with no committed tree is dropped here rather
/// than dispatched (RFC 0002 §6-2): the producer cannot validate the view at
/// post time without a TOCTOU race, so validation happens at drain, on the
/// main thread, where `VIEWS` is cheap to read.
fn drain_injected_events(cx: &mut AsyncApp) {
    while let Some(entry) = pop_injected() {
        let view_exists = VIEWS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(entry.view as usize)
            .map_or(false, |slot| slot.is_some());
        if !view_exists {
            continue;
        }
        let changed = dispatch_injected(entry.view, entry.payload);
        if changed == 1 || take_input_dirty() == 1 {
            notify_view(cx, entry.view);
        }
    }
}

/// Spawn the foreground drain pump (RFC 0002 §3.3). It drains the queue once
/// at startup (delivering any backlog posted before the window opened), then
/// runs one full `drain_injected_events` per `()` on the wake channel; the
/// pump exits when every wake sender is dropped (the window closed).
/// `cx.spawn` schedules the future on the foreground executor, so the drain —
/// and every `mb_dispatch` it makes — runs on the main thread.
fn spawn_drain_pump(cx: &App, wake_rx: UnboundedReceiver<()>) {
    cx.spawn(async move |mut cx: &mut AsyncApp| {
        drain_injected_events(&mut cx);
        let mut wake_rx = wake_rx;
        while wake_rx.next().await.is_some() {
            drain_injected_events(&mut cx);
        }
    })
    .detach();
}

// --- Test-only dispatch recorder -------------------------------------------
//
// With the `test-dispatch-stub` feature, build.rs replaces `mb_dispatch` with
// a stub that returns 0. The async-injection tests need to observe dispatches
// (kind/view/payload) and drive the `changed` return value, so the stub
// routes through this recorder when one is installed.
#[cfg(feature = "test-dispatch-stub")]
mod dispatch_recorder {
    use std::sync::{Mutex, OnceLock};

    /// One observed dispatch. `payload` is the `EVENT_QUEUE` entry at `token`
    /// (copied synchronously, as a real MoonBit handler would).
    #[derive(Clone, PartialEq, Eq, Debug)]
    pub struct RecordedDispatch {
        pub kind: i32,
        pub view: i32,
        pub data_a: i32,
        pub data_b: i32,
        pub payload: Vec<u8>,
    }

    static RECORDER: OnceLock<Mutex<Recorder>> = OnceLock::new();

    #[derive(Default)]
    struct Recorder {
        events: Vec<RecordedDispatch>,
        changed: i32,
    }

    /// Install a fresh recorder and return a guard that removes it on drop.
    #[cfg(test)]
    pub fn install() -> RecorderGuard {
        let recorder = RECORDER.get_or_init(|| Mutex::new(Recorder::default()));
        *recorder.lock().unwrap_or_else(|e| e.into_inner()) = Recorder::default();
        RecorderGuard
    }

    #[cfg(test)]
    pub struct RecorderGuard;

    #[cfg(test)]
    impl Drop for RecorderGuard {
        fn drop(&mut self) {
            if let Some(recorder) = RECORDER.get() {
                *recorder.lock().unwrap_or_else(|e| e.into_inner()) = Recorder::default();
            }
        }
    }

    /// Make subsequent dispatches return `changed` (1 → the pump notifies the
    /// view; 0 → it does not).
    #[cfg(test)]
    pub fn set_changed(changed: i32) {
        if let Some(recorder) = RECORDER.get() {
            recorder.lock().unwrap_or_else(|e| e.into_inner()).changed = changed;
        }
    }

    /// Snapshot of every dispatch observed since `install`.
    #[cfg(test)]
    pub fn take_events() -> Vec<RecordedDispatch> {
        RECORDER
            .get()
            .map(|recorder| {
                std::mem::take(&mut recorder.lock().unwrap_or_else(|e| e.into_inner()).events)
            })
            .unwrap_or_default()
    }

    /// Called by the generated `mb_dispatch` stub.
    pub fn record(kind: i32, view: i32, data_a: i32, data_b: i32) -> i32 {
        let Some(recorder) = RECORDER.get() else {
            return 0;
        };
        let mut recorder = recorder.lock().unwrap_or_else(|e| e.into_inner());
        // Only EVENT_TEXT / EVENT_ASYNC carry a payload in EVENT_QUEUE (data_a
        // is the token); for click/key events data_a is a click_id/codepoint, so
        // indexing the queue with it would capture an unrelated entry.
        let payload = if kind == crate::EVENT_TEXT || kind == crate::EVENT_ASYNC {
            crate::EVENT_QUEUE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(data_a as usize)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        recorder.events.push(RecordedDispatch {
            kind,
            view,
            data_a,
            data_b,
            payload,
        });
        recorder.changed
    }
}

#[cfg(feature = "test-dispatch-stub")]
pub use dispatch_recorder::RecordedDispatch;
#[cfg(all(test, feature = "test-dispatch-stub"))]
pub use dispatch_recorder::RecorderGuard;

#[cfg(all(test, feature = "test-dispatch-stub"))]
fn install_dispatch_recorder() -> dispatch_recorder::RecorderGuard {
    dispatch_recorder::install()
}

#[cfg(all(test, feature = "test-dispatch-stub"))]
fn set_dispatch_changed(changed: i32) {
    dispatch_recorder::set_changed(changed)
}

#[cfg(all(test, feature = "test-dispatch-stub"))]
fn take_recorded_dispatches() -> Vec<RecordedDispatch> {
    dispatch_recorder::take_events()
}

/// Copy the text payload for a pending EVENT_TEXT dispatch.
///
/// `token` is the index passed in `data_a`; `buf` must point to at least `len`
/// writable bytes (the `data_b` value). Returns the number of bytes written,
/// or a negative GPUI_STATUS_* on error. The payload is valid only during the
/// dispatch call that provided the token.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_event_copy_text(token: i32, buf: *mut u8, len: i32) -> i32 {
    ffi_export("gpui_event_copy_text", || {
        if token < 0 || buf.is_null() || len < 0 {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        let guard = EVENT_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(payload) = guard.get(token as usize) else {
            return GPUI_STATUS_INVALID_HANDLE;
        };
        let copy_len = (len as usize).min(payload.len());
        unsafe {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), buf, copy_len);
        }
        copy_len as i32
    })
}

/// Stack headroom required before descending one more level of a committed
/// tree, and the size of the segment allocated once that headroom is gone.
///
/// The red zone is far larger than the few kilobytes typical for this helper
/// because the frames are far larger: `render_node` measured at roughly 70 KB
/// of stack per nesting level in a debug build, so a 2 MiB thread overflows
/// between depth 24 and 32 and an 8 MiB one between 112 and 128 (issue #74).
/// A stack overflow aborts the process — `catch_unwind` cannot convert it into
/// `GPUI_STATUS_INTERNAL_PANIC` — so growing here is the difference between a
/// diagnosable error and a dead app.
const STACK_RED_ZONE: usize = 512 * 1024;
const STACK_GROW_BY: usize = 4 * 1024 * 1024;

/// Collect text node contents in DFS pre-order from a committed tree.
fn collect_text_contents(node: &UiNode, out: &mut Vec<u8>) {
    stacker::maybe_grow(STACK_RED_ZONE, STACK_GROW_BY, || match node {
        UiNode::Div { children, .. } => {
            for child in children {
                collect_text_contents(child, out);
            }
        }
        UiNode::Text { content, .. } => {
            let bytes = content.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        // The editable content lives in the per-view TextInputModel, not the
        // committed tree; read it via gpui_input_copy_text instead.
        UiNode::TextInput { .. } => {}
    })
}

/// Debug read-back: dump every text node's content from the committed tree for
/// `view` into `buf` as a sequence of `len u32 LE + utf8[len]` records (DFS
/// pre-order). Returns the total number of bytes written, or a negative
/// GPUI_STATUS_* on error. Used by the headless round-trip test (issue #34)
/// to verify MoonBit→C→Rust text fidelity without a GUI.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_debug_dump_text(view: i32, buf: *mut u8, len: i32) -> i32 {
    ffi_export("gpui_debug_dump_text", || {
        if view < 0 || buf.is_null() || len < 0 {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        let guard = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(Some(root)) = guard.get(view as usize) else {
            return GPUI_STATUS_INVALID_HANDLE;
        };
        let mut payload = Vec::new();
        collect_text_contents(root, &mut payload);
        let copy_len = (len as usize).min(payload.len());
        unsafe {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), buf, copy_len);
        }
        copy_len as i32
    })
}

/// Cross-boundary ABI probe: echo `value` back unchanged.
///
/// The whole bridge assumes MoonBit's native `Int` is ABI-compatible with
/// Rust's `i32` (callback envelope, command-buffer operands, status codes).
/// MoonBit's `main.mbt` type annotation anchors that at `moon check` time,
/// but nothing verifies the actual register/stack width across the boundary.
/// The headless round-trip test (issue #54, G23) sends boundary values
/// (`i32::MAX`, `i32::MIN`, 0, -1) through this probe on every build; any
/// width or sign-extension mismatch fails the build instead of corrupting
/// silently at runtime.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_abi_probe(value: i32) -> i32 {
    ffi_export("gpui_abi_probe", || value)
}

/// A single box shadow decoded from `OP_SET_SHADOW`. Offsets, blur, and spread
/// are pixel values; color is RGBA (0–255). `render_node` maps it onto a gpui
/// `BoxShadow` (offset/blur_radius/spread_radius + `Hsla` color).
#[derive(Clone, PartialEq, Debug)]
struct Shadow {
    x: f32,
    y: f32,
    blur: f32,
    spread: f32,
    color: (u8, u8, u8, u8),
}

/// One styled run decoded from `OP_TEXT_RUN` (issue #91). `start`/`len` are
/// UTF-8 byte offsets into the owning text node's content, validated at decode
/// time (in bounds, on `char` boundaries, sorted, non-overlapping) so gpui's
/// panicking run machinery can never see a bad range. Style fields are applied
/// only when their `RUN_STYLE_*` bit is set in `flags`; the rest of the run
/// inherits the text node's base style.
#[derive(Clone, PartialEq, Debug)]
struct TextRunSpec {
    start: usize,
    len: usize,
    flags: i32,
    color: (u8, u8, u8, u8),
    weight: i32,
    background: (u8, u8, u8, u8),
}

#[derive(Clone)]
enum UiNode {
    Div {
        width: f32,
        height: f32,
        bg: Option<(u8, u8, u8)>,
        flex: bool,
        flex_col: bool,
        center: bool,
        gap: f32,
        rounded: f32,
        padding: f32,
        border_width: f32,
        border_color: Option<(u8, u8, u8)>,
        // --- G7 core layout/style + G9 color (issue #51) -----------------
        /// Background with alpha (G9). Takes precedence over `bg` when both set.
        bg_color: Option<(u8, u8, u8, u8)>,
        /// Per-side margin in px (top, right, bottom, left).
        margin: Option<(f32, f32, f32, f32)>,
        /// Minimum (width, height) in px; a negative component means auto.
        min_size: Option<(f32, f32)>,
        /// Maximum (width, height) in px; a negative component means auto.
        max_size: Option<(f32, f32)>,
        /// Flex item params: (grow, shrink, basis_px); basis < 0 means auto.
        flex_item: Option<(f32, f32, f32)>,
        /// (align_items, justify_content) as ABI enum ids; 0 = default (unset).
        align: Option<(i32, i32)>,
        /// (overflow_x, overflow_y) as ABI enum ids.
        overflow: Option<(i32, i32)>,
        /// Opacity 0.0–1.0.
        opacity: Option<f32>,
        /// Box shadow.
        shadow: Option<Shadow>,
        /// Cursor style as an ABI enum id.
        cursor: Option<i32>,
        /// Position mode as an ABI enum id (0 relative, 1 absolute).
        position: Option<i32>,
        /// Per-side inset in px (top, right, bottom, left); negative = auto.
        inset: Option<(f32, f32, f32, f32)>,
        /// Per-side padding in px (top, right, bottom, left). Takes precedence
        /// over the uniform `padding` when both are set.
        padding_sides: Option<(f32, f32, f32, f32)>,
        // --- G8 typography (issue #51) -----------------------------------
        /// Font size in px for descendant text (inherited via `Style.text`).
        text_size: Option<f32>,
        /// Text color RGBA (0–255) for descendant text.
        text_color: Option<(u8, u8, u8, u8)>,
        /// Font weight 100–900 (clamped at decode time).
        font_weight: Option<i32>,
        /// Line height in px; `None` keeps gpui's default (the golden ratio).
        line_height: Option<f32>,
        /// Text alignment as an ABI enum id; 0 = default (unset).
        text_align: Option<i32>,
        /// Whitespace/wrap handling as an ABI enum id; 0 = default (unset).
        whitespace: Option<i32>,
        /// Font family name for descendant text.
        font_family: Option<String>,
        on_click: Option<i32>,
        // --- Keyboard navigation / a11y (issue #52) ----------------------
        /// Focusable flag (`OP_SET_FOCUSABLE`): nonzero makes the div a
        /// focusable element (gpui `.focusable()`). Requires element identity,
        /// which `render_node` synthesizes when no key/click id is present.
        focusable: Option<bool>,
        /// Tab order index (`OP_SET_TAB_INDEX`): sets gpui `.tab_index()`,
        /// which also marks the element focusable and a tab stop.
        tab_index: Option<isize>,
        /// Tab stop flag (`OP_SET_TAB_STOP`): nonzero keeps the element
        /// reachable via Tab, zero removes it from keyboard navigation while
        /// leaving it in tab-index order (gpui `.tab_stop()`).
        tab_stop: Option<bool>,
        /// Explicit stable identity, independent of click routing. When set,
        /// `render_node` uses it as the GPUI `ElementId`; duplicate keys within
        /// a committed tree are rejected at `commit_tree`.
        key: Option<String>,
        // --- Scroll position feedback (issue #89) ------------------------
        /// Feedback subscription id (`OP_SET_SCROLL_ID`). When set on a
        /// scrollable div, every settled offset change dispatches
        /// `EVENT_SCROLL` with this id and the current state is readable via
        /// the `gpui_scroll_copy_state` pull ABI. Position retention still
        /// requires `key` — without one the handle is fresh each rebuild, so
        /// the reported offset resets with it.
        scroll_id: Option<i32>,
        children: Vec<UiNode>,
    },
    Text {
        content: String,
        color: (u8, u8, u8),
        size: f32,
        /// Styled runs (`OP_TEXT_RUN`, issue #91), in start order. Empty for
        /// the common single-style case, which renders through the plain
        /// `div().child(content)` path unchanged.
        runs: Vec<TextRunSpec>,
    },
    /// Editable text input (RFC 0003, issue #88). A leaf: the committed tree
    /// carries only the widget's identity and placeholder — the editable
    /// content, selection, and IME marked range live in the per-view
    /// `TextInputModel` entity (the Rust side is the source of truth), which
    /// survives rebuilds exactly like `ScrollHandle`s.
    TextInput {
        input_id: i32,
        placeholder: String,
    },
}


fn report_panic(context: &str, payload: &(dyn Any + Send)) {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    eprintln!("gpui-sys: panic in {context}: {message}");
}

fn ffi_export<F>(name: &str, f: F) -> i32
where
    F: FnOnce() -> i32,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(payload) => {
            report_panic(name, payload.as_ref());
            GPUI_STATUS_INTERNAL_PANIC
        }
    }
}

fn div_mut(nodes: &mut [Option<UiNode>], handle: i32) -> Result<&mut UiNode, i32> {
    if handle < 0 {
        return Err(GPUI_STATUS_INVALID_HANDLE);
    }
    match nodes.get_mut(handle as usize) {
        None => Err(GPUI_STATUS_INVALID_HANDLE),
        Some(None) => Err(GPUI_STATUS_NODE_ABSENT),
        Some(Some(node @ UiNode::Div { .. })) => Ok(node),
        Some(Some(UiNode::Text { .. } | UiNode::TextInput { .. })) => {
            Err(GPUI_STATUS_WRONG_NODE_KIND)
        }
    }
}

fn push_node(nodes: &mut Vec<Option<UiNode>>, node: UiNode) -> i32 {
    let Ok(id) = i32::try_from(nodes.len()) else {
        return GPUI_STATUS_INVALID_HANDLE;
    };
    nodes.push(Some(node));
    id
}

// --- Command buffer (issue #5) ---------------------------------------------
//
// MoonBit builds the whole tree as one length-delimited command buffer and
// submits it with a single `gpui_build_tree` call, replacing the former
// property-per-call FFI surface. The buffer is a flat opcode stream: a fixed
// header, then a sequence of `[opcode u8][operands]` records. Node creation
// pushes a handle onto an internal stack; setters apply to the top of the
// stack; `OP_ADD_CHILD` pops child then parent and re-pushes the parent;
// `OP_SET_ROOT` pops the root. All multi-byte integers are little-endian.
//
// Wire layout (little-endian):
//   header:  "GPUI" (4 bytes) | BUFFER_VERSION (u32)
//   OP_DIV            u8
//   OP_TEXT           u8 | len u32 | utf8[len] | r u8 | g u8 | b u8 | size f32
//   OP_TEXT_RUN       u8 | start u32 | len u32 | flags u8 | r u8 g u8 b u8 a u8 | weight i32 | br u8 bg u8 bb u8 ba u8
//                     (issue #91: appends one styled run to the text node on
//                     top of the stack; start/len are UTF-8 byte offsets into
//                     its content, flags are RUN_STYLE_* bits, unset fields
//                     still occupy their zero-filled slot)
//   OP_SET_SIZE       u8 | w f32 | h f32
//   OP_SET_BG         u8 | r u8 | g u8 | b u8
//   OP_SET_FLEX       u8 | col u8
//   OP_SET_CENTER     u8
//   OP_SET_GAP        u8 | gap f32
//   OP_SET_ROUNDED    u8 | radius f32
//   OP_SET_ON_CLICK   u8 | click_id i32
//   OP_SET_KEY        u8 | len u32 | utf8[len]
//   OP_SET_PADDING    u8 | padding f32
//   OP_SET_BORDER     u8 | width f32 | r u8 | g u8 | b u8
//   OP_SET_BG_COLOR   u8 | r u8 | g u8 | b u8 | a u8          (G9: alpha)
//   OP_SET_MARGIN     u8 | top i32 | right i32 | bottom i32 | left i32   (px)
//   OP_SET_MIN_SIZE   u8 | w i32 | h i32                    (px; -1 = auto)
//   OP_SET_MAX_SIZE   u8 | w i32 | h i32                    (px; -1 = auto)
//   OP_SET_FLEX_ITEM  u8 | grow i32 | shrink i32 | basis i32 (grow/shrink ×1000; basis px, -1 = auto)
//   OP_SET_ALIGN      u8 | align_items i32 | justify_content i32  (ALIGN_*/JUSTIFY_* ids)
//   OP_SET_OVERFLOW   u8 | x i32 | y i32                    (OVERFLOW_* ids)
//   OP_SET_OPACITY    u8 | x1000 i32                        (0–1000 → 0.0–1.0)
//   OP_SET_SHADOW     u8 | x i32 | y i32 | blur i32 | spread i32 | r u8 | g u8 | b u8 | a u8  (px + RGBA)
//   OP_SET_CURSOR     u8 | kind i32                         (CURSOR_* ids)
//   OP_SET_POSITION   u8 | mode i32                         (POSITION_* ids)
//   OP_SET_INSET      u8 | top i32 | right i32 | bottom i32 | left i32   (px; -1 = auto)
//   OP_SET_PADDING_SIDES u8 | top i32 | right i32 | bottom i32 | left i32   (px; overrides uniform padding)
//   OP_SET_TEXT_SIZE  u8 | size i32                         (px; G8 typography)
//   OP_SET_TEXT_COLOR u8 | r u8 | g u8 | b u8 | a u8        (G8: RGBA text color)
//   OP_SET_FONT_WEIGHT u8 | weight i32                      (100–900; clamped)
//   OP_SET_LINE_HEIGHT u8 | px_x1000 i32                    (px×1000; negative = unset)
//   OP_SET_TEXT_ALIGN u8 | id i32                           (TEXT_ALIGN_* ids)
//   OP_SET_WHITESPACE u8 | id i32                           (WHITESPACE_* ids)
//   OP_SET_FONT_FAMILY u8 | len u32 | utf8[len]             (font family name)
//   OP_SET_FOCUSABLE  u8 | mode i32                         (0 = not focusable, nonzero = focusable)
//   OP_SET_TAB_INDEX  u8 | index i32                        (tab order; also marks focusable + tab stop)
//   OP_SET_TAB_STOP   u8 | mode i32                         (0 = skip in Tab nav, nonzero = tab stop)
//   OP_SET_SCROLL_ID  u8 | scroll_id i32                    (scroll feedback subscription, issue #89)
//   OP_ADD_CHILD      u8            (pops child, then parent; re-pushes parent)
//   OP_SET_ROOT       u8            (pops the root)
//
// Opcodes and BUFFER_VERSION are generated from abi.toml on both sides, so a
// drift fails the cross-boundary constant check rather than corrupting at
// runtime. New opcodes are backward-compatible additions (issue #42): an old
// Rust binary rejects them with `UNKNOWN_OPCODE` rather than misdecoding, so
// `BUFFER_VERSION` is bumped only when an existing opcode's meaning changes.

const BUFFER_MAGIC: &[u8; 4] = b"GPUI";

/// Upper bound on the magnitude of any `f32` geometry operand, in pixels.
///
/// Chosen empirically (issue #75): taffy accumulates sizes/gaps/padding without
/// saturating, so values near `f32::MAX` overflow to `inf` during layout and
/// behave exactly like an infinite input. A megapixel is ~500× the largest
/// realistic display dimension, so clamping here cannot affect a sane tree while
/// keeping every intermediate sum comfortably finite.
const MAX_LAYOUT_PX: f32 = 1.0e6;

/// Maximum nesting depth of a committed tree, counting the root as level 1.
///
/// Picked from measurement, not from taste (issue #74). Rendering a nested
/// chain in a debug build on a 2 MiB thread stack overflows:
///
/// * between depth 24 and 32 with plain recursion, and
/// * between depth 256 and 384 once [`STACK_RED_ZONE`] growth is in place.
///
/// The second cliff is not ours: `stacker` covers the three walkers in this
/// crate, but gpui and taffy recurse over the element tree on their own during
/// layout, and the nested elements drop recursively too. That ceiling can move
/// with a gpui upgrade and cannot be raised from here.
///
/// 64 sits under that remaining cliff with room for a main thread smaller than
/// the test harness's — Windows defaults to 1 MiB, half of what the measurement
/// above used — while still being several times deeper than any real UI (the
/// Counter demo nests under ten). A tree deeper than this is a bug in the
/// caller, and a status code says so where a stack overflow would only abort.
const MAX_TREE_DEPTH: u32 = 64;

/// A cursor over the command buffer with little-endian readers. Every reader
/// returns `None` on truncation so the parser reports `TRUNCATED_BUFFER`
/// instead of panicking.
struct BufferReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BufferReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_u8(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(byte)
    }

    fn read_u32(&mut self) -> Option<u32> {
        let bytes = self.data.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_i32(&mut self) -> Option<i32> {
        self.read_u32().map(|v| v as i32)
    }

    fn read_f32(&mut self) -> Option<f32> {
        self.read_u32().map(f32::from_bits)
    }

    /// Read an `f32` operand that ends up as geometry.
    ///
    /// Two guards, both measured rather than assumed (issue #75):
    ///
    /// * Non-finite values are rejected with `GPUI_STATUS_INVALID_FLOAT`. They
    ///   do not panic taffy — they lay out to infinite bounds, and an infinite
    ///   gap yields a `NaN` sibling width — so nothing catches them downstream.
    /// * Finite values are clamped to ±[`MAX_LAYOUT_PX`]. `f32::MAX` measured
    ///   identically to `f32::INFINITY`, because taffy's accumulation overflows
    ///   long before the type does; an `is_finite` check alone would not close
    ///   the hole.
    ///
    /// Rejection is per-buffer: the caller returns the status and the tree is
    /// not committed, matching how truncation and unknown opcodes behave.
    fn read_layout_f32(&mut self) -> Result<f32, i32> {
        match self.read_f32() {
            None => Err(GPUI_STATUS_TRUNCATED_BUFFER),
            Some(v) if !v.is_finite() => Err(GPUI_STATUS_INVALID_FLOAT),
            Some(v) => Ok(v.clamp(-MAX_LAYOUT_PX, MAX_LAYOUT_PX)),
        }
    }

    /// Borrow `len` bytes without copying; advances the cursor.
    fn read_bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        let slice = self.data.get(self.pos..self.pos + len)?;
        self.pos += len;
        Some(slice)
    }

    fn read_string(&mut self) -> Option<String> {
        let len = self.read_u32()? as usize;
        let bytes = self.read_bytes(len)?;
        Some(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Apply `f` to the div on top of the stack. Fails with `INVALID_HANDLE` if the
/// stack is empty, `NODE_ABSENT` if the top was already moved, `WRONG_NODE_KIND`
/// if the top is a text node.
fn with_top_div<F>(stack: &[i32], nodes: &mut [Option<UiNode>], f: F) -> i32
where
    F: FnOnce(&mut UiNode) -> i32,
{
    let Some(&handle) = stack.last() else {
        return GPUI_STATUS_INVALID_HANDLE;
    };
    match div_mut(nodes, handle) {
        Ok(node) => f(node),
        Err(status) => status,
    }
}

/// Build and commit a tree for `view` from one command buffer. On any failure
/// the staging state is discarded and the previously committed tree is left
/// untouched.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_build_tree(view: i32, ptr: *const u8, len: i32) -> i32 {
    ffi_export("gpui_build_tree", || {
        if view < 0 {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        if ptr.is_null() || len < 0 {
            return GPUI_STATUS_TRUNCATED_BUFFER;
        }
        let data = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
        build_tree_from_buffer(view as usize, data)
    })
}

fn build_tree_from_buffer(view: usize, data: &[u8]) -> i32 {
    let mut reader = BufferReader::new(data);

    // Header: magic + version.
    match reader.read_bytes(BUFFER_MAGIC.len()) {
        Some(m) if m == BUFFER_MAGIC => {}
        _ => return GPUI_STATUS_BAD_BUFFER_VERSION,
    }
    match reader.read_u32() {
        Some(v) if v == BUFFER_VERSION as u32 => {}
        _ => return GPUI_STATUS_BAD_BUFFER_VERSION,
    }

    let mut nodes: Vec<Option<UiNode>> = Vec::new();
    let mut stack: Vec<i32> = Vec::new();
    let mut root: Option<usize> = None;

    loop {
        let Some(opcode) = reader.read_u8() else {
            break; // clean end of buffer
        };
        let status = match opcode as i32 {
            OP_DIV => {
                let id = push_node(
                    &mut nodes,
                    UiNode::Div {
                        width: 0.0,
                        height: 0.0,
                        bg: None,
                        flex: false,
                        flex_col: false,
                        center: false,
                        gap: 0.0,
                        rounded: 0.0,
                        padding: 0.0,
                        border_width: 0.0,
                        border_color: None,
                        bg_color: None,
                        margin: None,
                        min_size: None,
                        max_size: None,
                        flex_item: None,
                        align: None,
                        overflow: None,
                        opacity: None,
                        shadow: None,
                        cursor: None,
                        position: None,
                        inset: None,
                        padding_sides: None,
                        text_size: None,
                        text_color: None,
                        font_weight: None,
                        line_height: None,
                        text_align: None,
                        whitespace: None,
                        font_family: None,
                        on_click: None,
                        focusable: None,
                        tab_index: None,
                        tab_stop: None,
                        key: None,
                        scroll_id: None,
                        children: Vec::new(),
                    },
                );
                if id < 0 {
                    id
                } else {
                    stack.push(id);
                    GPUI_STATUS_OK
                }
            }
            OP_TEXT => {
                let (Some(content), Some(r), Some(g), Some(b)) = (
                    reader.read_string(),
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                ) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                let size = match reader.read_layout_f32() {
                    Ok(v) => v,
                    Err(status) => return status,
                };
                let id = push_node(
                    &mut nodes,
                    UiNode::Text {
                        content,
                        color: (r, g, b),
                        size,
                        runs: Vec::new(),
                    },
                );
                if id < 0 {
                    id
                } else {
                    stack.push(id);
                    GPUI_STATUS_OK
                }
            }
            OP_TEXT_RUN => {
                let (Some(start), Some(len), Some(flags)) =
                    (reader.read_u32(), reader.read_u32(), reader.read_u8())
                else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                let (Some(r), Some(g), Some(b), Some(a)) = (
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                ) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                let Some(weight) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                let (Some(br), Some(bg), Some(bb), Some(ba)) = (
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                ) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                // Reject unknown flag bits the way an unknown opcode is
                // rejected: an old binary must not half-apply a run whose
                // meaning it does not fully know.
                const KNOWN_RUN_FLAGS: i32 = RUN_STYLE_COLOR
                    | RUN_STYLE_WEIGHT
                    | RUN_STYLE_ITALIC
                    | RUN_STYLE_UNDERLINE
                    | RUN_STYLE_STRIKETHROUGH
                    | RUN_STYLE_BACKGROUND;
                if flags as i32 & !KNOWN_RUN_FLAGS != 0 {
                    return GPUI_STATUS_INVALID_TEXT_RUN;
                }
                let Some(&handle) = stack.last() else {
                    return GPUI_STATUS_INVALID_HANDLE;
                };
                let node = match nodes.get_mut(handle as usize) {
                    None => return GPUI_STATUS_INVALID_HANDLE,
                    Some(None) => return GPUI_STATUS_NODE_ABSENT,
                    Some(Some(node)) => node,
                };
                let UiNode::Text { content, runs, .. } = node else {
                    return GPUI_STATUS_WRONG_NODE_KIND;
                };
                // Semantic validation happens here, against the owning text
                // node, so gpui's panicking run machinery (`compute_runs` /
                // `with_runs`) can never see a bad range. Sorted and
                // non-overlapping falls out of "start >= previous end".
                let (start, len) = (start as usize, len as usize);
                let Some(end) = start.checked_add(len) else {
                    return GPUI_STATUS_INVALID_TEXT_RUN;
                };
                if end > content.len()
                    || !content.is_char_boundary(start)
                    || !content.is_char_boundary(end)
                    || runs.last().is_some_and(|prev| start < prev.start + prev.len)
                {
                    return GPUI_STATUS_INVALID_TEXT_RUN;
                }
                runs.push(TextRunSpec {
                    start,
                    len,
                    flags: flags as i32,
                    color: (r, g, b, a),
                    weight,
                    background: (br, bg, bb, ba),
                });
                GPUI_STATUS_OK
            }
            OP_TEXT_INPUT => {
                let (Some(input_id), Some(placeholder)) =
                    (reader.read_i32(), reader.read_string())
                else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                let id = push_node(
                    &mut nodes,
                    UiNode::TextInput {
                        input_id,
                        placeholder,
                    },
                );
                if id < 0 {
                    id
                } else {
                    stack.push(id);
                    GPUI_STATUS_OK
                }
            }
            OP_SET_SIZE => {
                let (w, h) = match (reader.read_layout_f32(), reader.read_layout_f32()) {
                    (Ok(w), Ok(h)) => (w, h),
                    (Err(status), _) | (Ok(_), Err(status)) => return status,
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { width, height, .. } => {
                        *width = w;
                        *height = h;
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_BG => {
                let (Some(r), Some(g), Some(b)) =
                    (reader.read_u8(), reader.read_u8(), reader.read_u8())
                else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { bg, .. } => {
                        *bg = Some((r, g, b));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_FLEX => {
                let Some(col) = reader.read_u8() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { flex, flex_col, .. } => {
                        *flex = true;
                        *flex_col = col != 0;
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_CENTER => with_top_div(&stack, &mut nodes, |node| match node {
                UiNode::Div { center, .. } => {
                    *center = true;
                    GPUI_STATUS_OK
                }
                _ => unreachable!("with_top_div guarantees a div"),
            }),
            OP_SET_GAP => {
                let gap = match reader.read_layout_f32() {
                    Ok(v) => v,
                    Err(status) => return status,
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { gap: value, .. } => {
                        *value = gap;
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_ROUNDED => {
                let radius = match reader.read_layout_f32() {
                    Ok(v) => v,
                    Err(status) => return status,
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { rounded, .. } => {
                        *rounded = radius;
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_ON_CLICK => {
                let Some(click_id) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { on_click, .. } => {
                        *on_click = Some(click_id);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_KEY => {
                let Some(key) = reader.read_string() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { key: slot, .. } => {
                        *slot = Some(key);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_PADDING => {
                let padding = match reader.read_layout_f32() {
                    Ok(v) => v,
                    Err(status) => return status,
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { padding: value, .. } => {
                        *value = padding;
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_BORDER => {
                let width = match reader.read_layout_f32() {
                    Ok(v) => v,
                    Err(status) => return status,
                };
                let (Some(r), Some(g), Some(b)) =
                    (reader.read_u8(), reader.read_u8(), reader.read_u8())
                else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div {
                        border_width,
                        border_color,
                        ..
                    } => {
                        *border_width = width;
                        *border_color = Some((r, g, b));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_BG_COLOR => {
                let (Some(r), Some(g), Some(b), Some(a)) = (
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                ) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { bg_color, .. } => {
                        *bg_color = Some((r, g, b, a));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_MARGIN => {
                let (Some(top), Some(right), Some(bottom), Some(left)) = (
                    reader.read_i32(),
                    reader.read_i32(),
                    reader.read_i32(),
                    reader.read_i32(),
                ) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { margin, .. } => {
                        *margin = Some((top as f32, right as f32, bottom as f32, left as f32));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_MIN_SIZE => {
                let (Some(w), Some(h)) = (reader.read_i32(), reader.read_i32()) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { min_size, .. } => {
                        *min_size = Some((w as f32, h as f32));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_MAX_SIZE => {
                let (Some(w), Some(h)) = (reader.read_i32(), reader.read_i32()) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { max_size, .. } => {
                        *max_size = Some((w as f32, h as f32));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_FLEX_ITEM => {
                let (Some(grow), Some(shrink), Some(basis)) = (
                    reader.read_i32(),
                    reader.read_i32(),
                    reader.read_i32(),
                ) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { flex_item, .. } => {
                        *flex_item = Some((
                            grow as f32 / 1000.0,
                            shrink as f32 / 1000.0,
                            basis as f32,
                        ));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_ALIGN => {
                let (Some(align_items), Some(justify_content)) =
                    (reader.read_i32(), reader.read_i32())
                else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { align, .. } => {
                        *align = Some((align_items, justify_content));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_OVERFLOW => {
                let (Some(x), Some(y)) = (reader.read_i32(), reader.read_i32()) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { overflow, .. } => {
                        *overflow = Some((x, y));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_OPACITY => {
                let Some(x1000) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { opacity, .. } => {
                        *opacity = Some(x1000 as f32 / 1000.0);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_SHADOW => {
                let (
                    Some(x),
                    Some(y),
                    Some(blur),
                    Some(spread),
                    Some(r),
                    Some(g),
                    Some(b),
                    Some(a),
                ) = (
                    reader.read_i32(),
                    reader.read_i32(),
                    reader.read_i32(),
                    reader.read_i32(),
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                ) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { shadow, .. } => {
                        *shadow = Some(Shadow {
                            x: x as f32,
                            y: y as f32,
                            blur: blur as f32,
                            spread: spread as f32,
                            color: (r, g, b, a),
                        });
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_CURSOR => {
                let Some(kind) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { cursor, .. } => {
                        *cursor = Some(kind);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_POSITION => {
                let Some(mode) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { position, .. } => {
                        *position = Some(mode);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_INSET => {
                let (Some(top), Some(right), Some(bottom), Some(left)) = (
                    reader.read_i32(),
                    reader.read_i32(),
                    reader.read_i32(),
                    reader.read_i32(),
                ) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { inset, .. } => {
                        *inset = Some((top as f32, right as f32, bottom as f32, left as f32));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_PADDING_SIDES => {
                let (Some(top), Some(right), Some(bottom), Some(left)) = (
                    reader.read_i32(),
                    reader.read_i32(),
                    reader.read_i32(),
                    reader.read_i32(),
                ) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { padding_sides, .. } => {
                        *padding_sides =
                            Some((top as f32, right as f32, bottom as f32, left as f32));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_TEXT_SIZE => {
                let Some(size) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { text_size, .. } => {
                        *text_size = Some(size as f32);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_TEXT_COLOR => {
                let (Some(r), Some(g), Some(b), Some(a)) = (
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                    reader.read_u8(),
                ) else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { text_color, .. } => {
                        *text_color = Some((r, g, b, a));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_FONT_WEIGHT => {
                let Some(weight) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { font_weight, .. } => {
                        // gpui's FontWeight is a free f32, but the CSS-style
                        // 100–900 range is the documented contract; clamp
                        // out-of-range operands rather than reject them.
                        *font_weight = Some(weight.clamp(100, 900));
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_LINE_HEIGHT => {
                let Some(px_x1000) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { line_height, .. } => {
                        // Negative = unset (restores gpui's default line
                        // height); the px×1000 fixed-point matches the
                        // opacity/flex milliunit convention.
                        *line_height = if px_x1000 < 0 {
                            None
                        } else {
                            Some(px_x1000 as f32 / 1000.0)
                        };
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_TEXT_ALIGN => {
                let Some(id) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { text_align, .. } => {
                        *text_align = Some(id);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_WHITESPACE => {
                let Some(id) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { whitespace, .. } => {
                        *whitespace = Some(id);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_FONT_FAMILY => {
                let Some(family) = reader.read_string() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { font_family, .. } => {
                        *font_family = Some(family);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            // --- Keyboard navigation / a11y (issue #52) -----------------
            OP_SET_FOCUSABLE => {
                let Some(mode) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { focusable, .. } => {
                        *focusable = Some(mode != 0);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_TAB_INDEX => {
                let Some(index) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { tab_index, .. } => {
                        *tab_index = Some(index as isize);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_TAB_STOP => {
                let Some(mode) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { tab_stop, .. } => {
                        *tab_stop = Some(mode != 0);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_SET_SCROLL_ID => {
                let Some(sid) = reader.read_i32() else {
                    return GPUI_STATUS_TRUNCATED_BUFFER;
                };
                with_top_div(&stack, &mut nodes, |node| match node {
                    UiNode::Div { scroll_id, .. } => {
                        *scroll_id = Some(sid);
                        GPUI_STATUS_OK
                    }
                    _ => unreachable!("with_top_div guarantees a div"),
                })
            }
            OP_ADD_CHILD => {
                let (Some(child), Some(parent)) = (stack.pop(), stack.pop()) else {
                    return GPUI_STATUS_INVALID_HANDLE;
                };
                if parent < 0 || child < 0 || parent == child {
                    return GPUI_STATUS_INVALID_HANDLE;
                }
                let parent_index = parent as usize;
                let child_index = child as usize;
                if parent_index >= nodes.len() || child_index >= nodes.len() {
                    return GPUI_STATUS_INVALID_HANDLE;
                }
                match &nodes[parent_index] {
                    None => return GPUI_STATUS_NODE_ABSENT,
                    Some(UiNode::Text { .. } | UiNode::TextInput { .. }) => {
                        return GPUI_STATUS_WRONG_NODE_KIND;
                    }
                    Some(UiNode::Div { .. }) => {}
                }
                if nodes[child_index].is_none() {
                    return GPUI_STATUS_NODE_ABSENT;
                }
                let child_node = nodes[child_index]
                    .take()
                    .expect("child presence was validated");
                let Some(UiNode::Div { children, .. }) = nodes[parent_index].as_mut() else {
                    unreachable!("parent kind was validated");
                };
                children.push(child_node);
                stack.push(parent);
                GPUI_STATUS_OK
            }
            OP_SET_ROOT => {
                let Some(handle) = stack.pop() else {
                    return GPUI_STATUS_INVALID_HANDLE;
                };
                if handle < 0 {
                    return GPUI_STATUS_INVALID_HANDLE;
                }
                match nodes.get(handle as usize) {
                    None => GPUI_STATUS_INVALID_HANDLE,
                    Some(None) => GPUI_STATUS_NODE_ABSENT,
                    Some(Some(_)) => {
                        root = Some(handle as usize);
                        GPUI_STATUS_OK
                    }
                }
            }
            _ => return GPUI_STATUS_UNKNOWN_OPCODE,
        };
        if status != GPUI_STATUS_OK {
            return status;
        }
    }

    // Commit: validate root + duplicate keys, then swap into VIEWS.
    let Some(root_index) = root else {
        return GPUI_STATUS_NO_ROOT;
    };
    if nodes[root_index].is_none() {
        return GPUI_STATUS_NODE_ABSENT;
    }
    let live_scroll_ids;
    {
        // One iterative pass validates both invariants that span the whole
        // tree: unique keys, and a bounded nesting depth. Depth is carried on
        // the walk stack rather than derived from the decoder's stack, because
        // the two are not the same number — a subtree can be built shallow and
        // then nested under a fresh parent, so only the committed shape says
        // how deep the walkers will actually recurse (issue #74). The same
        // pass collects the tree's scroll feedback ids so the mirror can drop
        // entries a rebuild removed (issue #89).
        let mut seen = std::collections::HashSet::new();
        let mut scroll_ids = std::collections::HashSet::new();
        let root_ref = nodes[root_index].as_ref().expect("root present");
        let mut walk: Vec<(&UiNode, u32)> = vec![(root_ref, 1)];
        while let Some((node, depth)) = walk.pop() {
            if depth > MAX_TREE_DEPTH {
                return GPUI_STATUS_DEPTH_EXCEEDED;
            }
            let UiNode::Div {
                key,
                children,
                scroll_id,
                ..
            } = node
            else {
                continue;
            };
            if let Some(key) = key {
                if !seen.insert(key.as_str()) {
                    return GPUI_STATUS_DUPLICATE_KEY;
                }
            }
            if let Some(sid) = scroll_id {
                scroll_ids.insert(*sid);
            }
            walk.extend(children.iter().map(|child| (child, depth + 1)));
        }
        live_scroll_ids = scroll_ids;
    }
    let root_node = nodes[root_index].take().expect("root presence was validated");
    let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
    if view >= views.len() {
        views.resize(view + 1, None);
    }
    views[view] = Some(root_node);
    drop(views);
    scroll_mirror_prune(view as i32, &live_scroll_ids);
    GPUI_STATUS_OK
}

/// Recursively locate the div carrying `key` and overwrite the `content` of its
/// first `UiNode::Text` child in place. Returns `true` on a successful update.
///
/// A keyed div whose first child is a text node is the canonical "labelled
/// value" shape (the Counter's count card: a keyed div wrapping one text node).
/// Only that first text child is touched — sibling text nodes and the rest of
/// the subtree are left untouched, so an incremental update is a single string
/// assignment rather than a rebuild. A keyed div with no text child, or a key
/// that resolves to a text node, yields no update (the caller falls back to a
/// full rebuild).
fn update_keyed_text(node: &mut UiNode, key: &str, text: &str) -> bool {
    stacker::maybe_grow(STACK_RED_ZONE, STACK_GROW_BY, || {
        let UiNode::Div {
            key: node_key,
            children,
            ..
        } = node
        else {
            return false;
        };
        if node_key.as_deref() == Some(key) {
            if let Some(UiNode::Text { content, .. }) = children.first_mut() {
                *content = text.to_string();
                return true;
            }
            return false;
        }
        children
            .iter_mut()
            .any(|child| update_keyed_text(child, key, text))
    })
}

/// Update the text of a keyed node in the committed tree for `view` in place,
/// without rebuilding the tree (issue #10: measurement-justified incremental
/// update).
///
/// `key_ptr`/`key_len` and `text_ptr`/`text_len` are UTF-8 byte slices (no NUL
/// terminator; the explicit lengths carry the size, matching how `OP_SET_KEY`
/// and `OP_TEXT` carry their strings). The function walks the retained
/// `VIEWS[view]` tree for the div whose `OP_SET_KEY` value equals `key` and
/// overwrites its first text child's content. The re-render still flows through
/// the existing dispatch→notify path: `dispatch` returns 1, Rust calls
/// `cx.notify()`, and `render_node` reads the now-updated `VIEWS[view]`.
///
/// Returns `GPUI_STATUS_OK` on success. Returns `GPUI_STATUS_KEY_NOT_FOUND` when
/// the view has no committed tree or no keyed text node matches — the caller
/// (MoonBit) then falls back to a full `gpui_build_tree`. `GPUI_STATUS_INVALID_HANDLE`
/// for a negative view, `GPUI_STATUS_TRUNCATED_BUFFER` for a null/negative
/// pointer or length.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_update_text(
    view: i32,
    key_ptr: *const u8,
    key_len: i32,
    text_ptr: *const u8,
    text_len: i32,
) -> i32 {
    ffi_export("gpui_update_text", || {
        if view < 0 {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        if key_ptr.is_null() || key_len < 0 || text_ptr.is_null() || text_len < 0 {
            return GPUI_STATUS_TRUNCATED_BUFFER;
        }
        let key_bytes = unsafe { std::slice::from_raw_parts(key_ptr, key_len as usize) };
        let text_bytes = unsafe { std::slice::from_raw_parts(text_ptr, text_len as usize) };
        let (Ok(key), Ok(text)) = (std::str::from_utf8(key_bytes), std::str::from_utf8(text_bytes))
        else {
            return GPUI_STATUS_TRUNCATED_BUFFER;
        };
        let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(Some(root)) = views.get_mut(view as usize) else {
            return GPUI_STATUS_KEY_NOT_FOUND;
        };
        if update_keyed_text(root, key, text) {
            GPUI_STATUS_OK
        } else {
            GPUI_STATUS_KEY_NOT_FOUND
        }
    })
}

/// Open a window rendering the committed tree for `view` (index into
/// `VIEWS`) and block in the GPUI event loop. A negative `view` fails with
/// `GPUI_STATUS_INVALID_HANDLE` before any GPUI startup.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_run_window(view: i32, width: f32, height: f32) -> i32 {
    ffi_export("gpui_run_window", || {
        if view < 0 {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        run_window_with_fallback(view as usize, width, height, false)
    })
}

fn run_window(view: usize, width: f32, height: f32, benchmark: bool) {
    Application::new().run(move |cx: &mut App| {
        // Attach a fresh wake channel and start the drain pump before the
        // window opens, so events posted from other threads — including any
        // backlog queued before startup — are drained as soon as the loop
        // runs (RFC 0002 §3.3).
        spawn_drain_pump(cx, install_inject_queue());
        let view_id = view as i32;
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        // Focus the view at construction (with `window` available), the
                        // same way GPUI's own examples do — focusing during `render`
                        // does not reliably make the element the OS first responder,
                        // so key events never arrive.
                        let focus = cx.focus_handle();
                        window.focus(&focus);
                        FfiView {
                            focus,
                            view,
                            scroll_handles: Rc::new(RefCell::new(HashMap::new())),
                            inputs: Rc::new(RefCell::new(HashMap::new())),
                        }
                    })
                },
            )
            .unwrap();
        // Route drain-pump notifications to this view (RFC 0002 §3.4). The
        // root entity is only reachable through `read_window` (the `root`
        // accessor is test-support-gated), so downgrade inside the closure and
        // register the weak handle.
        let mut benchmark_entity = None;
        if let Ok(weak) = cx.read_window(&window, |entity, _| entity.downgrade()) {
            if benchmark {
                benchmark_entity = Some(weak.clone());
            }
            register_view(cx, view_id, weak);
        }
        // Arm the frame-paced benchmark loop after the window exists so the
        // first `on_next_frame` tick observes the first presented frame.
        if let Some(entity) = benchmark_entity {
            let _ = window.update(cx, |_, window, _| {
                schedule_benchmark_frame(window, view_id, &entity);
                // The initial tree is committed before the native window is
                // opened. Explicitly invalidate the window here so the first
                // benchmark frame is queued immediately instead of waiting
                // for a later event-loop mutation.
                window.refresh();
            });
        }
        cx.activate(true);
    });
}

fn run_window_with_fallback(view: usize, width: f32, height: f32, benchmark: bool) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| run_window(view, width, height, benchmark))) {
        Ok(()) => GPUI_STATUS_OK,
        Err(first_panic) => {
            #[cfg(target_os = "linux")]
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                report_panic("gpui_run_window (Wayland attempt)", first_panic.as_ref());
                eprintln!(
                    "gpui-sys: Wayland startup failed; unsetting WAYLAND_DISPLAY and retrying with X11"
                );
                // SAFETY: window startup is single-threaded and happens before GPUI
                // creates worker threads that could concurrently read the environment.
                unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
                return match catch_unwind(AssertUnwindSafe(|| {
                    run_window(view, width, height, benchmark)
                })) {
                    Ok(()) => GPUI_STATUS_OK,
                    Err(second_panic) => {
                        report_panic("gpui_run_window (X11 retry)", second_panic.as_ref());
                        GPUI_STATUS_INTERNAL_PANIC
                    }
                };
            }

            report_panic("gpui_run_window", first_panic.as_ref());
            GPUI_STATUS_INTERNAL_PANIC
        }
    }
}

// ---------------------------------------------------------------------------
// Real-window UI benchmark (ui-frame scope)
//
// Frame-paced benchmark loop for the cross-editor comparison harness,
// mirroring the retired Rust editor adapter: every `on_next_frame` tick
// records the interval since the previous frame, drives ONE action through
// the real input/scroll path, and quits once the target action count is
// reached. Scenario codes:
//   0 (open):   sample the first presented frame as first_interactive and quit.
//   1 (input):  append one ASCII char to every retained input model via the
//               same commit path real typing uses (apply + mirror +
//               EVENT_INPUT_CHANGED), so MoonBit rebuilds and recommits the
//               tree before the next frame renders it.
//   2 (scroll): add +/- `stride` px to every retained scroll handle — the
//               native wheel equivalent; paint clamps the offset and the
//               EVENT_SCROLL announcement fires as it would for a user.
// Frames are the vsync-paced GPUI frames, so sample intervals are real frame
// durations, not action pacing. The JSON report is printed to stdout just
// before quitting, because on macOS quitting terminates the process without
// unwinding `gpui_run_window_benchmark`.

struct WindowBenchmark {
    scenario: i32,
    target: usize,
    stride: f32,
    document_load_ms: f64,
    started: std::time::Instant,
    previous_frame: Option<std::time::Instant>,
    action_started: Option<std::time::Instant>,
    pending_work_ms: f64,
    paint_work_ms: Option<f64>,
    first_interactive_ms: f64,
    work_samples: Vec<f64>,
    dispatch_work_samples: Vec<f64>,
    samples: Vec<f64>,
    latencies: Vec<f64>,
    action_timestamps_epoch_ms: Vec<f64>,
    action_window_end_epoch_ms: Option<f64>,
    completed: usize,
    warming_up: bool,
    action_index: usize,
}

static WINDOW_BENCHMARK: Mutex<Option<WindowBenchmark>> = Mutex::new(None);

unsafe extern "C" {
    fn md_editor_benchmark_signpost_event(action_id: i32);
}

fn benchmark_milliseconds(started: std::time::Instant, finished: std::time::Instant) -> f64 {
    finished.duration_since(started).as_secs_f64() * 1000.0
}

fn benchmark_epoch_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_run_window_benchmark(
    view: i32,
    width: f32,
    height: f32,
    scenario: i32,
    target: i32,
    stride: f32,
    document_load_ms: f64,
) -> i32 {
    ffi_export("gpui_run_window_benchmark", || {
        if view < 0 || !(0..=2).contains(&scenario) || target <= 0 {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        *WINDOW_BENCHMARK.lock().unwrap_or_else(|e| e.into_inner()) = Some(WindowBenchmark {
            scenario,
            target: target as usize,
            stride,
            document_load_ms,
            started: std::time::Instant::now(),
            previous_frame: None,
            action_started: None,
            pending_work_ms: 0.0,
            paint_work_ms: None,
            first_interactive_ms: 0.0,
            work_samples: Vec::new(),
            dispatch_work_samples: Vec::new(),
            samples: Vec::new(),
            latencies: Vec::new(),
            action_timestamps_epoch_ms: Vec::new(),
            action_window_end_epoch_ms: None,
            completed: 0,
            warming_up: scenario != 0,
            action_index: 0,
        });
        run_window_with_fallback(view as usize, width, height, true)
    })
}

fn schedule_benchmark_frame(window: &mut Window, view: i32, entity: &WeakEntity<FfiView>) {
    let entity = entity.clone();
    window.on_next_frame(move |window, cx| {
        benchmark_frame_tick(window, cx, view, &entity);
    });
}

fn benchmark_drive_action(
    inputs: Rc<RefCell<HashMap<i32, Entity<TextInputModel>>>>,
    scroll_handles: Rc<RefCell<HashMap<String, ScrollHandle>>>,
    window: &mut Window,
    cx: &mut App,
    scenario: i32,
    index: usize,
    stride: f32,
    view: i32,
) {
    match scenario {
        // Append one char through the same commit path real typing uses:
        // apply to the model, refresh the mirror, emit EVENT_INPUT_CHANGED so
        // MoonBit pulls the text, rebuilds and recommits the tree.
        // Apps that never commit a text input (keyboard-driven editors that
        // receive typed characters as EVENT_TEXT) have no retained model; for
        // those, synthesize the same EVENT_TEXT payload the AppKit
        // `on_key_down` handler pushes, so the injected action travels the
        // identical dispatch/rebuild path as a real keystroke.
        1 => {
            let models: Vec<Entity<TextInputModel>> =
                inputs.borrow().values().cloned().collect();
            if models.is_empty() {
                let bytes = char::from(b'a' + (index % 26) as u8)
                    .to_string()
                    .into_bytes();
                let len = bytes.len() as i32;
                let token = {
                    let mut q = EVENT_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                    q.push(bytes);
                    (q.len() - 1) as i32
                };
                unsafe { mb_dispatch(ABI_VERSION, EVENT_TEXT, view, token, len) };
                // #70: the payload is only valid during the synchronous
                // dispatch; drop it so the queue cannot grow with actions.
                EVENT_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clear();
                return;
            }
            for model in models {
                model.update(cx, |m, cx| {
                    let ch = char::from(b'a' + (index % 26) as u8);
                    let start = m.content.len();
                    input_apply_replace(
                        &mut m.content,
                        &mut m.selected_range,
                        &mut m.marked_range,
                        start..start,
                        &ch.to_string(),
                    );
                    m.sync_mirror();
                    m.emit_changed(window, cx);
                });
            }
        }
        // Native wheel equivalent: shift every retained scroll handle (the
        // document-scroll div is the only one in this app). Alternating
        // directions match the wheel deltas the other adapters send; paint
        // clamps the offset to the content bounds exactly as for a user.
        2 => {
            for handle in scroll_handles.borrow_mut().values_mut() {
                let mut offset = handle.offset();
                offset.y += px(if index % 2 == 0 { -stride } else { stride });
                handle.set_offset(offset);
            }
        }
        _ => {}
    }
}

fn benchmark_frame_tick(window: &mut Window, cx: &mut App, view: i32, entity: &WeakEntity<FfiView>) {
    let now = std::time::Instant::now();
    let mut report = None;
    let mut skip_action = false;
    {
        let mut guard = WINDOW_BENCHMARK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(st) = guard.as_mut() else { return };
        if st.previous_frame.is_none() {
            st.first_interactive_ms = benchmark_milliseconds(st.started, now);
            // on_next_frame can run before the first render pass. Arm a
            // second callback for open so the first work sample is taken only
            // after request_layout/prepaint/paint has completed.
            st.previous_frame = Some(now);
            st.action_started = Some(now);
        } else if st.warming_up {
            st.warming_up = false;
            // Let the warm-up repaint settle before the first measured action.
            // This prevents the first action from inheriting the initial
            // window/layout scheduling burst while keeping warmup_action_count
            // at one for the protocol.
            skip_action = true;
        } else if st.scenario == 0 {
            // Open has no action or pacing interval. The first callback above
            // establishes the startup timestamp; this callback observes the
            // completed first paint and reports its measured work.
            if let Some(work) = st.paint_work_ms.take() {
                st.work_samples.push(work);
            } else {
                // A missing probe is a diagnostic failure, never a measured
                // zero. Keep the protocol shape while surfacing it as n/a via
                // the explicit unavailable marker in the JSON report.
                st.work_samples.push(f64::NAN);
            }
            report = Some(benchmark_report_json_v2(&st));
        } else {
            let previous = st.previous_frame.expect("previous frame");
            st.samples.push(benchmark_milliseconds(previous, now));
            st.work_samples.push(st.paint_work_ms.take().unwrap_or(st.pending_work_ms));
            st.dispatch_work_samples.push(st.pending_work_ms);
            let action = st.action_started.expect("action start");
            st.latencies.push(benchmark_milliseconds(action, now));
            st.completed += 1;
            if st.completed >= st.target {
                st.action_window_end_epoch_ms = Some(benchmark_epoch_ms());
                report = Some(benchmark_report_json_v2(&st));
            }
        }
        if report.is_none() {
            st.previous_frame = Some(now);
            st.action_started = Some(std::time::Instant::now());
            if skip_action {
                drop(guard);
                schedule_benchmark_frame(window, view, entity);
                window.refresh();
                return;
            }
            if !st.warming_up && st.scenario != 0 {
                unsafe { md_editor_benchmark_signpost_event(st.action_index as i32) };
                st.action_timestamps_epoch_ms.push(benchmark_epoch_ms());
            }
            let (scenario, index, stride) = (st.scenario, st.action_index, st.stride);
            st.action_index += 1;
            drop(guard);
            // Clone the shared handle maps out of the view state so actions
            // run OUTSIDE a `FfiView` update: the input commit path notifies
            // MoonBit synchronously, and the notification handler updates the
            // view entity again (GPUI forbids re-entrant updates).
            if let Some(view_entity) = entity.upgrade() {
                let view = view_entity.read(cx);
                let work_started = std::time::Instant::now();
                benchmark_drive_action(
                    view.inputs.clone(),
                    view.scroll_handles.clone(),
                    window,
                    cx,
                    scenario,
                    index,
                    stride,
                    view.view as i32,
                );
                // Input changes need MoonBit to rebuild the formatted tree;
                // native scroll offsets are already owned by GPUI and should
                // remain a compositor/layout-only update. Rebuilding all
                // blocks here makes the stress fixture quadratic in practice.
                let pending_work_ms = benchmark_milliseconds(work_started, std::time::Instant::now());
                if let Some(state) = WINDOW_BENCHMARK.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    state.pending_work_ms = pending_work_ms;
                }
                if scenario != 2 && scenario != 0 {
                    let _ = view_entity.update(cx, |_, cx| cx.notify());
                }
            }
            // `on_next_frame` callbacks only run when GPUI schedules another
            // draw. ScrollHandle changes can settle without invalidating the
            // window (notably on large documents), so request the next frame
            // explicitly before arming the callback.
            schedule_benchmark_frame(window, view, entity);
            window.refresh();
            return;
        }
        *guard = None;
    }
    // The report must reach stdout before quitting: on macOS `quit`
    // terminates the process through [NSApp terminate:], so
    // `Application::run` never unwinds back into MoonBit.
    {
        use std::io::Write;
        let report_text = report.as_deref().unwrap_or("");
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{report_text}");
        let _ = out.flush();
    }
    if std::env::var("UI_BENCHMARK_SYSTEM_PRESENT").ok().as_deref() == Some("1") {
        let tail_ms = std::env::var("UI_BENCHMARK_TRACE_TAIL_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(15_000)
            .min(120_000);
        std::thread::sleep(std::time::Duration::from_millis(tail_ms));
    }
    cx.quit();
}

fn benchmark_report_json_v2(st: &WindowBenchmark) -> String {
    let mut work = st.work_samples.clone();
    let mut intervals = st.samples.clone();
    let mut latencies = st.latencies.clone();
    work.sort_by(f64::total_cmp);
    intervals.sort_by(f64::total_cmp);
    latencies.sort_by(f64::total_cmp);
    let measured = |values: &[f64]| values.iter().copied().filter(|value| value.is_finite()).collect::<Vec<_>>();
    let average = |values: &[f64]| {
        let values = measured(values);
        if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 }
    };
    let at = |values: &[f64], ratio: f64| {
        let values = measured(values);
        if values.is_empty() { 0.0 } else { values[((values.len().saturating_sub(1)) as f64 * ratio).round() as usize] }
    };
    let scenario = match st.scenario { 0 => "open", 1 => "input", _ => "scroll" };
    let encode = |values: &[f64]| values.iter().map(|value| {
        if value.is_finite() { value.to_string() } else { "null".to_string() }
    }).collect::<Vec<_>>().join(",");
    let optional = |values: &[f64], value: f64| {
        if measured(values).is_empty() { "null".to_string() } else { value.to_string() }
    };
    let strict_trace = std::env::var("UI_BENCHMARK_SYSTEM_PRESENT").ok().as_deref() == Some("1");
    // The harness treats the adapter-emitted `adapter` field as authoritative,
    // so a second app built on this benchmark loop (e.g. gpui2) overrides the
    // default name instead of colliding with the original `gpui` rows.
    let adapter_name = std::env::var("UI_BENCHMARK_ADAPTER_NAME")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "gpui".to_string());
    let input_samples = if st.scenario == 1 { encode(&st.latencies) } else { String::new() };
    let input_mean = if st.scenario == 1 { average(&st.latencies) } else { 0.0 };
    let action_timestamps = encode(&st.action_timestamps_epoch_ms);
    let action_window_start = st.action_timestamps_epoch_ms.first()
        .map(f64::to_string)
        .unwrap_or_else(|| "null".to_string());
    let action_window_end = st.action_window_end_epoch_ms
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string());
    format!(
        "{{\"adapter\":\"{adapter}\",\"measurement_scope\":\"ui-frame\",\"timing_source\":\"gpui-request-layout-prepaint-paint-and-on_next_frame\",\"latency_source\":\"action-to-next-frame-callback\",\"scenario\":\"{scenario}\",\"frame_work_samples_ms\":[{work}],\"dispatch_work_samples_ms\":[{dispatch_work}],\"frame_interval_samples_ms\":[{intervals}],\"input_to_visible_samples_ms\":[{input_samples}],\"offscreen_samples_ms\":[{nulls}],\"readback_samples_ms\":[{nulls}],\"offscreen_readback_samples_ms\":[{nulls}],\"frame_work_ms\":{work_mean},\"frame_interval_ms\":{interval_mean},\"input_to_visible_ms\":{input_mean},\"offscreen_ms\":null,\"readback_ms\":null,\"offscreen_readback_ms\":null,\"frame_work_p95_ms\":{work_p95},\"frame_interval_p95_ms\":{interval_p95},\"input_to_visible_p95_ms\":{input_p95},\"dropped_display_frames\":null,\"action_count\":{actions},\"frame_sample_count\":{frames},\"warmup_action_count\":{warmups},\"action_timestamps_epoch_ms\":[{action_timestamps}],\"action_window_start_epoch_ms\":{action_window_start},\"action_window_end_epoch_ms\":{action_window_end},\"first_interactive_ms\":{first_interactive},\"document_load_ms\":{document_load},\"window_mode\":\"native-window\",\"work_scope\":\"gpui-request-layout-prepaint-paint\",\"display_timestamp_source\":\"{display_timestamp_source}\",\"viewport\":{{\"width\":1280,\"height\":800}},\"font\":\"system-ui 16px\",\"line_height\":1.55,\"overscan\":3,\"virtual_row_height\":66}}",
        adapter = adapter_name,
        work = encode(&st.work_samples),
        dispatch_work = encode(&st.dispatch_work_samples),
        intervals = encode(&st.samples),
        input_samples = input_samples,
        nulls = (0..st.work_samples.len()).map(|_| "null").collect::<Vec<_>>().join(","),
        work_mean = optional(&st.work_samples, average(&st.work_samples)),
        interval_mean = if st.samples.is_empty() { "null".to_string() } else { average(&st.samples).to_string() },
        input_mean = if st.scenario == 1 { input_mean.to_string() } else { "null".to_string() },
        work_p95 = optional(&st.work_samples, at(&work, 0.95)),
        interval_p95 = if intervals.is_empty() { "null".to_string() } else { at(&intervals, 0.95).to_string() },
        input_p95 = if st.scenario == 1 { at(&latencies, 0.95).to_string() } else { "null".to_string() },
        actions = st.target,
        frames = st.samples.len(),
        warmups = if st.scenario == 0 { 0 } else { 1 },
        action_timestamps = action_timestamps,
        action_window_start = action_window_start,
        action_window_end = action_window_end,
        first_interactive = st.first_interactive_ms,
        document_load = st.document_load_ms,
        display_timestamp_source = if strict_trace { "macos-compositor-trace" } else { "gpui-on_next_frame-callback" },
    )
}

// ---------------------------------------------------------------------------
// Text input widget + IME (RFC 0003, issue #88)
//
// The Rust side owns the editable state (content / selection / marked range):
// the platform IME queries `EntityInputHandler` synchronously with string
// returns, and no such MoonBit->Rust reply channel exists across the single
// 5xi32 `mb_dispatch` envelope (RFC 0003 §2). MoonBit is notified of commits
// via EVENT_INPUT_CHANGED / EVENT_INPUT_SUBMIT (push, no payload) and reads or
// writes the buffer explicitly through `gpui_input_text_len` /
// `gpui_input_copy_text` / `gpui_input_set_text` (pull).
//
// The pull ABI cannot reach the `TextInputModel` entity (reading an Entity
// needs an `App` context the C export does not have), so every commit point
// also updates `INPUT_MIRROR`, a Mutex-guarded (view, input_id) -> text/state
// mirror the exports read. `gpui_input_set_text` writes the mirror and queues
// the change; the widget applies queued writes to the entity during its next
// prepaint (which has the context), and `take_input_dirty()` tells the
// dispatch sites a redraw is needed even when MoonBit's handler reported no
// signal change.

/// Mirrored state for the pull ABI, updated at every commit point on the main
/// thread. `composing` gates `gpui_input_set_text` (BUSY_COMPOSING).
#[derive(Default, Clone)]
struct InputMirrorEntry {
    text: String,
    composing: bool,
}

static INPUT_MIRROR: Mutex<Option<HashMap<(i32, i32), InputMirrorEntry>>> = Mutex::new(None);

/// Pending `gpui_input_set_text` writes, applied to the entity at the widget's
/// next prepaint (the export has no `App` context; prepaint does).
static INPUT_SET_TEXT_QUEUE: Mutex<Vec<(i32, i32, String)>> = Mutex::new(Vec::new());

/// Set when a queued set_text needs a redraw that MoonBit's `changed` flag
/// alone would not trigger. Dispatch sites fold `take_input_dirty()` into
/// their notify decision.
static INPUT_DIRTY: Mutex<bool> = Mutex::new(false);

/// Serializes every test that touches the process-global input statics above
/// (`INPUT_MIRROR` / `INPUT_SET_TEXT_QUEUE` / `INPUT_DIRTY`): the state-machine
/// tests (`text_input_tests`, which reset the whole mirror to `None`) and the
/// headless interaction tests (`headless_tests`, which render a real widget and
/// then read its mirrored text back through the pull ABI).
///
/// Lock order when a test needs more than one of the process-global test locks:
/// `INJECT_TEST_LOCK` → `INPUT_TEST_LOCK` → `TEST_VIEWS_MUTEX`. Always take
/// them in that order; `headless::layout_bounds` / `with_rendered_tree` take
/// `TEST_VIEWS_MUTEX` internally, so it must be last.
#[cfg(test)]
static INPUT_TEST_LOCK: Mutex<()> = Mutex::new(());

fn mirror_update(view: i32, input_id: i32, text: &str, composing: bool) {
    let mut guard = INPUT_MIRROR.lock().unwrap_or_else(|e| e.into_inner());
    guard.get_or_insert_with(HashMap::new).insert(
        (view, input_id),
        InputMirrorEntry {
            text: text.to_string(),
            composing,
        },
    );
}

fn mirror_get(view: i32, input_id: i32) -> Option<InputMirrorEntry> {
    INPUT_MIRROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&(view, input_id)).cloned())
}

fn take_input_dirty() -> i32 {
    let mut guard = INPUT_DIRTY.lock().unwrap_or_else(|e| e.into_inner());
    if std::mem::take(&mut *guard) { 1 } else { 0 }
}

// --- Scroll position feedback (issue #89) -----------------------------------
//
// gpui owns scroll state: its wheel handler mutates the tracked `ScrollHandle`
// from a paint-registered listener and prepaint clamps the offset to the
// content bounds, so there is no Rust-side commit point to intercept — the
// settled value only exists after a draw. The `ScrollFeedback` wrapper element
// therefore observes the offset on every paint of the subscribed div, mirrors
// it for the pull ABI (the C exports have no `App` context — the same
// constraint that motivates `INPUT_MIRROR`), and defers a payload-free
// `EVENT_SCROLL` dispatch when the offset differs from the last one announced.
// MoonBit reads the numbers via `gpui_scroll_copy_state`, following RFC 0003's
// notify-then-pull contract: a coalesced or dropped event can never leave the
// consumer acting on stale data, because the pull always returns the current
// state.

/// Mirrored scroll state for the pull ABI, refreshed on every paint of the
/// subscribed div. All values are f32 px in gpui's scroll-space convention:
/// offsets are ≤ 0 (content scrolled down/right makes them more negative),
/// `max` is the positive scrollable extent, `viewport` is the container's
/// laid-out size.
#[derive(Default, Clone, Copy, PartialEq)]
struct ScrollMirrorEntry {
    offset: (f32, f32),
    max: (f32, f32),
    viewport: (f32, f32),
}

static SCROLL_MIRROR: Mutex<Option<HashMap<(i32, i32), ScrollMirrorEntry>>> = Mutex::new(None);

/// Last offset per (view, scroll_id) whose change was announced. Kept apart
/// from `SCROLL_MIRROR` because the two answer different questions: the mirror
/// is "what is the state now" (refreshed every paint), this is "what did
/// MoonBit last hear" (edge detection). The first observation seeds the entry
/// without dispatching — nothing has scrolled yet, and the initial position is
/// always pullable.
static SCROLL_SENT: Mutex<Option<HashMap<(i32, i32), (f32, f32)>>> = Mutex::new(None);

fn scroll_mirror_update(view: i32, scroll_id: i32, entry: ScrollMirrorEntry) {
    let mut guard = SCROLL_MIRROR.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .insert((view, scroll_id), entry);
}

fn scroll_mirror_get(view: i32, scroll_id: i32) -> Option<ScrollMirrorEntry> {
    SCROLL_MIRROR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&(view, scroll_id)).copied())
}

/// Drop mirror and edge-detection state for ids a rebuild removed from
/// `view`'s tree, so a stale pair cannot serve pulls forever. Called from the
/// commit path with the freshly collected id set.
fn scroll_mirror_prune(view: i32, live: &std::collections::HashSet<i32>) {
    let mut guard = SCROLL_MIRROR.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = guard.as_mut() {
        m.retain(|&(v, id), _| v != view || live.contains(&id));
    }
    drop(guard);
    let mut sent = SCROLL_SENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = sent.as_mut() {
        m.retain(|&(v, id), _| v != view || live.contains(&id));
    }
}

/// UTF-16 offset -> UTF-8 byte offset in `s` (clamped to the string's end).
/// The `EntityInputHandler` contract speaks UTF-16; the model stores UTF-8.
fn offset_from_utf16(s: &str, utf16_offset: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for ch in s.chars() {
        if utf16_count >= utf16_offset {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }
    utf8_offset
}

/// UTF-8 byte offset -> UTF-16 offset in `s` (clamped to the string's end).
fn offset_to_utf16(s: &str, utf8_offset: usize) -> usize {
    let mut utf16_offset = 0;
    let mut utf8_count = 0;
    for ch in s.chars() {
        if utf8_count >= utf8_offset {
            break;
        }
        utf8_count += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }
    utf16_offset
}

fn range_from_utf16(s: &str, range: &std::ops::Range<usize>) -> std::ops::Range<usize> {
    offset_from_utf16(s, range.start)..offset_from_utf16(s, range.end)
}

fn range_to_utf16(s: &str, range: &std::ops::Range<usize>) -> std::ops::Range<usize> {
    offset_to_utf16(s, range.start)..offset_to_utf16(s, range.end)
}

/// Pure state transition for `replace_text_in_range` (a commit: typed text,
/// IME confirmation, paste, backspace/delete). Split out from the entity so
/// the boundary arithmetic is unit-testable without a gpui context.
/// All offsets are UTF-8 bytes; `range_utf16` is converted by the caller.
fn input_apply_replace(
    content: &mut String,
    selected: &mut std::ops::Range<usize>,
    marked: &mut Option<std::ops::Range<usize>>,
    range: std::ops::Range<usize>,
    new_text: &str,
) {
    let mut next = String::with_capacity(content.len() + new_text.len());
    next.push_str(&content[..range.start]);
    next.push_str(new_text);
    next.push_str(&content[range.end..]);
    *content = next;
    let caret = range.start + new_text.len();
    *selected = caret..caret;
    *marked = None;
}

/// Pure state transition for `replace_and_mark_text_in_range` (IME preedit
/// update). The marked range tracks the freshly inserted text; the selection
/// lands inside it (or at its end when the IME gives no explicit selection).
fn input_apply_replace_and_mark(
    content: &mut String,
    selected: &mut std::ops::Range<usize>,
    marked: &mut Option<std::ops::Range<usize>>,
    range: std::ops::Range<usize>,
    new_text: &str,
    new_selected_in_text: Option<std::ops::Range<usize>>,
) {
    let mut next = String::with_capacity(content.len() + new_text.len());
    next.push_str(&content[..range.start]);
    next.push_str(new_text);
    next.push_str(&content[range.end..]);
    *content = next;
    *marked = if new_text.is_empty() {
        None
    } else {
        Some(range.start..range.start + new_text.len())
    };
    *selected = match new_selected_in_text {
        Some(sel) => range.start + sel.start..range.start + sel.end,
        None => {
            let caret = range.start + new_text.len();
            caret..caret
        }
    };
}

/// The retained, per-widget editable state (RFC 0003 §3.2). Lives in
/// `FfiView.inputs` keyed by `input_id`, created on first render and surviving
/// rebuilds. Main-thread only (same reasoning as `ScrollHandle`).
pub struct TextInputModel {
    view: i32,
    input_id: i32,
    content: String,
    placeholder: String,
    /// Selection in UTF-8 byte offsets; empty range = caret.
    selected_range: std::ops::Range<usize>,
    /// IME preedit span in UTF-8 byte offsets. While `Some`, the composition
    /// is drawn underlined and `gpui_input_set_text` is rejected.
    marked_range: Option<std::ops::Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    focus: FocusHandle,
}

impl TextInputModel {
    fn caret(&self) -> usize {
        self.selected_range.end
    }

    /// Previous char boundary (backspace / left-arrow granularity).
    fn previous_boundary(&self, offset: usize) -> usize {
        self.content[..offset]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Next char boundary (delete / right-arrow granularity).
    fn next_boundary(&self, offset: usize) -> usize {
        self.content[offset..]
            .chars()
            .next()
            .map(|c| offset + c.len_utf8())
            .unwrap_or(self.content.len())
    }

    fn sync_mirror(&self) {
        mirror_update(
            self.view,
            self.input_id,
            &self.content,
            self.marked_range.is_some(),
        );
    }

    /// Commit-path change notification to MoonBit (RFC 0003 §3.4): push, no
    /// payload — the handler pulls via `gpui_input_copy_text` if it cares.
    /// When the handler reports a state change (or queued a set_text), the
    /// owning `FfiView` is notified so the committed tree re-renders; the
    /// window is refreshed regardless because the widget's own visual state
    /// (text/caret) changed.
    fn emit_changed(&self, window: &mut Window, app: &mut App) {
        let changed = unsafe {
            mb_dispatch(ABI_VERSION, EVENT_INPUT_CHANGED, self.view, self.input_id, 0)
        };
        if changed == 1 || take_input_dirty() == 1 {
            notify_view_app(app, self.view);
        }
        window.refresh();
    }

    /// Apply a queued `gpui_input_set_text` write (called from prepaint, which
    /// has the context the C export lacks).
    fn apply_set_text(&mut self, text: String) {
        self.content = text;
        let end = self.content.len();
        self.selected_range = end..end;
        self.marked_range = None;
        self.sync_mirror();
    }

    /// Editing keys the widget consumes while focused. Returns true when the
    /// key was handled (the root container's dispatch suppression means these
    /// never reach MoonBit anyway; see `FfiView::render`).
    fn handle_editing_key(&mut self, key: &str, window: &mut Window, app: &mut App) -> bool {
        match key {
            "backspace" => {
                if self.selected_range.is_empty() {
                    let start = self.previous_boundary(self.caret());
                    self.selected_range = start..self.caret();
                }
                if !self.selected_range.is_empty() {
                    let range = self.selected_range.clone();
                    input_apply_replace(
                        &mut self.content,
                        &mut self.selected_range,
                        &mut self.marked_range,
                        range,
                        "",
                    );
                    self.sync_mirror();
                    self.emit_changed(window, app);
                }
                true
            }
            "delete" => {
                if self.selected_range.is_empty() {
                    let end = self.next_boundary(self.caret());
                    self.selected_range = self.caret()..end;
                }
                if !self.selected_range.is_empty() {
                    let range = self.selected_range.clone();
                    input_apply_replace(
                        &mut self.content,
                        &mut self.selected_range,
                        &mut self.marked_range,
                        range,
                        "",
                    );
                    self.sync_mirror();
                    self.emit_changed(window, app);
                }
                true
            }
            "left" => {
                let caret = if self.selected_range.is_empty() {
                    self.previous_boundary(self.caret())
                } else {
                    self.selected_range.start
                };
                self.selected_range = caret..caret;
                window.refresh();
                true
            }
            "right" => {
                let caret = if self.selected_range.is_empty() {
                    self.next_boundary(self.caret())
                } else {
                    self.selected_range.end
                };
                self.selected_range = caret..caret;
                window.refresh();
                true
            }
            "home" => {
                self.selected_range = 0..0;
                window.refresh();
                true
            }
            "end" => {
                let end = self.content.len();
                self.selected_range = end..end;
                window.refresh();
                true
            }
            "enter" => {
                // Single-line input: Enter submits instead of inserting a
                // newline (RFC 0003 §3.4).
                let changed = unsafe {
                    mb_dispatch(ABI_VERSION, EVENT_INPUT_SUBMIT, self.view, self.input_id, 0)
                };
                if changed == 1 || take_input_dirty() == 1 {
                    notify_view_app(app, self.view);
                    window.refresh();
                }
                true
            }
            _ => false,
        }
    }
}

impl EntityInputHandler for TextInputModel {
    fn text_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = range_from_utf16(&self.content, &range_utf16);
        adjusted_range.replace(range_to_utf16(&self.content, &range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: range_to_utf16(&self.content, &self.selected_range),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| range_to_utf16(&self.content, range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
        self.sync_mirror();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| range_from_utf16(&self.content, r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        input_apply_replace(
            &mut self.content,
            &mut self.selected_range,
            &mut self.marked_range,
            range,
            new_text,
        );
        self.sync_mirror();
        self.emit_changed(window, cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<std::ops::Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| range_from_utf16(&self.content, r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        // The IME's selection is relative to `new_text`; convert against it.
        let new_selected = new_selected_range_utf16
            .as_ref()
            .map(|r| range_from_utf16(new_text, r));
        input_apply_replace_and_mark(
            &mut self.content,
            &mut self.selected_range,
            &mut self.marked_range,
            range,
            new_text,
            new_selected,
        );
        // Preedit stays Rust-internal (RFC 0003 §3.3): update the mirror and
        // redraw, but do NOT notify MoonBit until the composition commits.
        self.sync_mirror();
        window.refresh();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = range_from_utf16(&self.content, &range_utf16);
        Some(Bounds::from_corners(
            point(
                element_bounds.left() + layout.x_for_index(range.start),
                element_bounds.top(),
            ),
            point(
                element_bounds.left() + layout.x_for_index(range.end),
                element_bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds.as_ref()?;
        let layout = self.last_layout.as_ref()?;
        let utf8_index = layout.index_for_x(point.x - bounds.left())?;
        Some(offset_to_utf16(&self.content, utf8_index))
    }
}

/// Custom element that shapes and paints one text-input line: committed text,
/// preedit underline run, selection highlight, caret, and the
/// `Window::handle_input` registration (the paint-time hook that connects the
/// focused widget to the platform IME). Adapted from gpui's `examples/input.rs`.
struct TextInputElement {
    input: Entity<TextInputModel>,
}

struct TextInputPrepaint {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextInputElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextInputElement {
    type RequestLayoutState = ();
    type PrepaintState = TextInputPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // Apply queued gpui_input_set_text writes first: the C export has no
        // App context, so the widget itself is the application point.
        let (view, input_id) = {
            let m = self.input.read(cx);
            (m.view, m.input_id)
        };
        let pending: Vec<String> = {
            let mut q = INPUT_SET_TEXT_QUEUE
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut taken = Vec::new();
            q.retain(|(v, i, text)| {
                if *v == view && *i == input_id {
                    taken.push(text.clone());
                    false
                } else {
                    true
                }
            });
            taken
        };
        if !pending.is_empty() {
            self.input.update(cx, |m, _| {
                for text in pending {
                    m.apply_set_text(text);
                }
            });
        }

        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let marked_range = input.marked_range.clone();
        let cursor_offset = input.caret();
        let style = window.text_style();

        let (display_text, text_color): (SharedString, Hsla) = if content.is_empty() {
            (input.placeholder.clone().into(), hsla(0., 0., 0.5, 0.6))
        } else {
            (content.clone().into(), style.color)
        };

        let base_run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // Three runs: committed / preedit (underlined) / committed. The
        // preedit underline is the visual contract that tells the user which
        // span the IME still owns (RFC 0003 §3.3).
        let runs = match (&marked_range, content.is_empty()) {
            (Some(marked), false) => vec![
                TextRun {
                    len: marked.start,
                    ..base_run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(base_run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..base_run.clone()
                },
                TextRun {
                    len: display_text.len() - marked.end,
                    ..base_run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect(),
            _ => vec![base_run],
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let show_marks = !content.is_empty();
        let cursor_x = if show_marks {
            line.x_for_index(cursor_offset)
        } else {
            px(0.)
        };
        let (selection, cursor) = if selected_range.is_empty() || !show_marks {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_x, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    rgb(0x3B82F6),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    rgba(0x3B82F640),
                )),
                None,
            )
        };
        TextInputPrepaint {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        // The paint-time registration that routes platform IME queries to this
        // widget while it holds focus (`a11y-ime.md` §2.2, window.rs:3400).
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.input.clone()), cx);
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let line = prepaint.line.take().unwrap();
        line.paint(bounds.origin, window.line_height(), window, cx)
            .unwrap();
        if focus.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }
        self.input.update(cx, |m, _| {
            m.last_layout = Some(line);
            m.last_bounds = Some(bounds);
        });
    }
}

/// UTF-8 byte length of the committed content of `(view, input_id)`, for
/// buffer sizing before `gpui_input_copy_text`. Main-thread contract, same as
/// every other export called from inside `dispatch`.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_input_text_len(view: i32, input_id: i32) -> i32 {
    ffi_export("gpui_input_text_len", || match mirror_get(view, input_id) {
        Some(entry) => entry.text.len() as i32,
        None => GPUI_STATUS_INVALID_HANDLE,
    })
}

/// Copy the committed content of `(view, input_id)` into `buf` (up to `len`
/// bytes). Returns bytes written, or a negative status. Same contract as
/// `gpui_event_copy_text`.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_input_copy_text(view: i32, input_id: i32, buf: *mut u8, len: i32) -> i32 {
    ffi_export("gpui_input_copy_text", || {
        if buf.is_null() || len < 0 {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        let Some(entry) = mirror_get(view, input_id) else {
            return GPUI_STATUS_INVALID_HANDLE;
        };
        let bytes = entry.text.as_bytes();
        let copy_len = (len as usize).min(bytes.len());
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, copy_len);
        }
        copy_len as i32
    })
}

/// Replace the committed content of `(view, input_id)`; the caret moves to the
/// end. Rejected with `GPUI_STATUS_BUSY_COMPOSING` while an IME composition is
/// active (the marked text belongs to the IME, not the app). The write lands
/// in the mirror immediately (subsequent reads see it) and is applied to the
/// widget at its next prepaint.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_input_set_text(view: i32, input_id: i32, ptr: *const u8, len: i32) -> i32 {
    ffi_export("gpui_input_set_text", || {
        if len < 0 || (ptr.is_null() && len != 0) {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        let Some(entry) = mirror_get(view, input_id) else {
            return GPUI_STATUS_INVALID_HANDLE;
        };
        if entry.composing {
            return GPUI_STATUS_BUSY_COMPOSING;
        }
        let text = if len == 0 {
            String::new()
        } else {
            // SAFETY: `ptr` points to `len` readable bytes for the duration of
            // this call (the standard FFI borrow contract).
            String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len as usize) })
                .into_owned()
        };
        mirror_update(view, input_id, &text, false);
        INPUT_SET_TEXT_QUEUE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((view, input_id, text));
        *INPUT_DIRTY.lock().unwrap_or_else(|e| e.into_inner()) = true;
        GPUI_STATUS_OK
    })
}

/// Number of bytes `gpui_scroll_copy_state` writes: six little-endian f32
/// values (offset_x, offset_y, max_x, max_y, viewport_w, viewport_h).
pub const SCROLL_STATE_BYTES: usize = 24;

/// Copy the mirrored scroll state of `(view, scroll_id)` into `buf` (issue
/// #89). Writes [`SCROLL_STATE_BYTES`] bytes: offset_x, offset_y, max_x,
/// max_y, viewport_w, viewport_h as little-endian f32. Offsets follow gpui's
/// scroll-space convention (≤ 0, more negative as content scrolls down/right);
/// max is the positive scrollable extent; viewport is the container's
/// laid-out size.
///
/// Returns bytes written, `GPUI_STATUS_KEY_NOT_FOUND` when the pair has never
/// painted (or a rebuild removed it), or `GPUI_STATUS_INVALID_HANDLE` for a
/// negative view or a null/short buffer. Main-thread contract, same as the
/// other pull exports: call it during a dispatch.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_scroll_copy_state(view: i32, scroll_id: i32, buf: *mut u8, len: i32) -> i32 {
    ffi_export("gpui_scroll_copy_state", || {
        if view < 0 || buf.is_null() || len < 0 || (len as usize) < SCROLL_STATE_BYTES {
            return GPUI_STATUS_INVALID_HANDLE;
        }
        let Some(entry) = scroll_mirror_get(view, scroll_id) else {
            return GPUI_STATUS_KEY_NOT_FOUND;
        };
        let values = [
            entry.offset.0,
            entry.offset.1,
            entry.max.0,
            entry.max.1,
            entry.viewport.0,
            entry.viewport.1,
        ];
        let mut bytes = [0u8; SCROLL_STATE_BYTES];
        for (chunk, v) in bytes.chunks_exact_mut(4).zip(values) {
            chunk.copy_from_slice(&v.to_le_bytes());
        }
        // SAFETY: `buf` points to at least `len` >= SCROLL_STATE_BYTES
        // writable bytes for the duration of this call (the standard FFI
        // borrow contract).
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, SCROLL_STATE_BYTES);
        }
        SCROLL_STATE_BYTES as i32
    })
}

// --- App-level IME bridge + clipboard ---------------------------------------
//
// Additive C surface (backward-compatible per issue #42: no existing opcode or
// envelope shape changes). Two gaps hit apps that draw and edit text entirely
// themselves (rich_text + app-level key handling) and therefore never commit an
// OP_TEXT_INPUT widget:
//
// 1. IME. The mac window forwards every keyDown to NSInputContext after the
//    app declines, but the NSTextInputClient queries behind it all land on
//    `window.input_handler` — which is None for such apps. Composition cannot
//    report marked text, commits are dropped, and the raw pinyin/kana letters
//    were already dispatched as EVENT_TEXT, so CJK input degrades to latin
//    letter insertion. `ImeBridge` is a window-level `InputHandler` registered
//    on every FfiView paint: it tracks the marked range (so the mac window's
//    `is_composing` routes in-composition keystrokes to the IME) and forwards
//    committed text to MoonBit as EVENT_TEXT — the same payload a typed
//    keystroke carries, so app-side text handling is unchanged. A real
//    OP_TEXT_INPUT widget re-registers during its own paint and overrides the
//    bridge for that window (last registration wins), preserving RFC 0003.
//
// (Clipboard is handled moon-side: adapter/native-stub talks to NSPasteboard
// directly and synchronously — the same shape as moonbitlang/x/fs's stubs —
// so no App-context bridge is needed here.)

static IME_MARKED: Mutex<Option<HashMap<i32, Option<std::ops::Range<usize>>>>> = Mutex::new(None);

fn ime_marked_get(view: i32) -> Option<std::ops::Range<usize>> {
    IME_MARKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&view))
        .cloned()
        .flatten()
}

fn ime_marked_set(view: i32, marked: Option<Range<usize>>) {
    IME_MARKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(view, marked);
}

/// Push `text` as an EVENT_TEXT payload and dispatch it synchronously (the
/// shared commit path of typed keystrokes, IME commits, and clipboard reads).
/// Returns the dispatch's `changed` flag.
fn dispatch_event_text(view: i32, text: &str) -> i32 {
    let bytes = text.as_bytes();
    let token = {
        let mut q = EVENT_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        q.push(bytes.to_vec());
        (q.len() - 1) as i32
    };
    let changed = unsafe { mb_dispatch(ABI_VERSION, EVENT_TEXT, view, token, bytes.len() as i32) };
    // #70: the payload is only valid during the synchronous dispatch call; drop
    // it on return so the queue cannot accumulate one entry per event.
    EVENT_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    changed
}

/// Probe geometry table: window-space rect (x, y, w, h) per `OP_SET_KEY` the
/// app tags with `caret` or `probe:*`, captured by [`ProbeBoundsProbe`] during
/// prepaint. The app draws its own content, so gpui-sys cannot know where
/// things are; the app claims geometry via keys: "caret" feeds the IME
/// candidate anchor (`ImeBridge::bounds_for_range`), "probe:*" feeds the app's
/// own hit-testing (drag selection maps mouse coords to token rects via the
/// `gpui_probe_rect` pull export). A key "probe:clear" — the root's first
/// child — wipes the table at the start of every frame, so rects can never go
/// stale across frames/scrolls.
static PROBE_BOUNDS: Mutex<Option<HashMap<String, [f32; 4]>>> = Mutex::new(None);

/// Transparent wrapper (same shape as `TextGlyphInset`): records the wrapped
/// div's prepaint bounds — already resolved absolute window-space coords,
/// including taffy's placement of `Position::Absolute` — then paints the child
/// untouched. Layout is fully delegated to the child's own layout node, so the
/// wrapper is invisible to flex/absolute placement and hit-testing.
struct ProbeBoundsProbe {
    child: AnyElement,
    key: String,
}

impl IntoElement for ProbeBoundsProbe {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ProbeBoundsProbe {
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
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Transparent: the child's own layout node is ours.
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let rect = [
            f32::from(bounds.origin.x),
            f32::from(bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        ];
        {
            // Lock is released before descending: probe divs nest (a caret
            // overlay lives inside a token div, both keyed), and std Mutex is
            // not reentrant.
            let mut guard = PROBE_BOUNDS.lock().unwrap_or_else(|e| e.into_inner());
            let table = guard.get_or_insert_with(HashMap::new);
            if self.key == "probe:clear" {
                // First probe of the frame (root's first child): fresh table.
                table.clear();
            }
            table.insert(self.key.clone(), rect);
        }
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

/// Pull ABI: read a probe rect by key. `buf`/`len` carry the ASCII key bytes
/// (no NUL required); on a hit writes rounded integer `(x, y, w, h)` as four
/// little-endian i32s into `out` (16 bytes, caller pre-zeroed) and returns 0,
/// else -1. Integer pixels are enough for the app's hit-testing; the app keeps
/// sub-pixel precision in its own model.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_probe_rect(buf: *const u8, len: i32, out: *mut u8) -> i32 {
    ffi_export("gpui_probe_rect", || {
        if buf.is_null() || out.is_null() || len <= 0 {
            return -1;
        }
        let bytes = unsafe { std::slice::from_raw_parts(buf, len as usize) };
        let Ok(key) = std::str::from_utf8(bytes) else {
            return -1;
        };
        let guard = PROBE_BOUNDS.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref().and_then(|table| table.get(key)) {
            Some(rect) => {
                unsafe {
                    for (i, v) in rect.iter().enumerate() {
                        let bytes = (v.round() as i32).to_le_bytes();
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.add(i * 4), 4);
                    }
                }
                0
            }
            None => -1,
        }
    })
}

/// EVENT_ASYNC payload tag for IME preedit updates (app-private contract):
/// one `0xEE` byte followed by the UTF-8 composing text; an empty text clears.
const IME_PREEDIT_TAG: u8 = 0xEE;

/// EVENT_ASYNC payload tag for mouse input (app-private contract): `0xEF`,
/// a phase byte (0 = left-button down, 1 = dragged move, 2 = left-button up),
/// then x and y as little-endian i32 window coords. The app hit-tests its own
/// layout with these (drag selection).
const MOUSE_TAG: u8 = 0xEF;

fn ime_preedit_payload(text: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(text.len() + 1);
    payload.push(IME_PREEDIT_TAG);
    payload.extend_from_slice(text.as_bytes());
    payload
}

/// Window-level IME target for apps without a text-input widget. Stateless
/// beyond the per-view marked range (kept in IME_MARKED so re-registering on
/// every paint cannot reset an in-flight composition).
struct ImeBridge {
    view: i32,
}

impl InputHandler for ImeBridge {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        // The bridge owns no text; an empty selection at 0 tells the IME that
        // replacements start there. While marked text exists the IME manages
        // its own replacement range and does not consult this.
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        ime_marked_get(self.view)
    }

    fn text_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        _cx: &mut App,
    ) {
        ime_marked_set(self.view, None);
        if text.is_empty() {
            return;
        }
        // Clear the app-side preedit strip first (a commit replaces the marked
        // span), then deliver the committed text. The rebuild those dispatches
        // perform swaps the committed tree but schedules no frame — request
        // one, or the commit only becomes visible on the next unrelated frame.
        dispatch_injected(self.view, ime_preedit_payload(""));
        dispatch_event_text(self.view, text);
        window.refresh();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        _cx: &mut App,
    ) {
        // Composition update. The span is tracked so `is_composing` (mac
        // window) keeps routing keystrokes to the IME until commit/unmark, and
        // the composing text is forwarded as an EVENT_ASYNC preedit payload so
        // the app can show what the IME owns (the candidate window alone is
        // not enough feedback for mixed-input IMEs).
        let marked = if new_text.is_empty() {
            None
        } else {
            Some(0..new_text.encode_utf16().count())
        };
        ime_marked_set(self.view, marked);
        dispatch_injected(self.view, ime_preedit_payload(new_text));
        window.refresh();
    }

    fn unmark_text(&mut self, window: &mut Window, _cx: &mut App) {
        ime_marked_set(self.view, None);
        dispatch_injected(self.view, ime_preedit_payload(""));
        window.refresh();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        // Anchor the candidate window at the app's painted caret: the rect of
        // the div keyed "caret", captured by `ProbeBoundsProbe` on the last
        // paint. Without one (caret never painted) fall back to the viewport
        // lower-left — no better anchor exists at bridge level.
        if let Some([x, y, w, h]) = PROBE_BOUNDS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|m| m.get("caret"))
            .copied()
        {
            return Some(Bounds::new(point(px(x), px(y)), size(px(w), px(h))));
        }
        let vp = window.viewport_size();
        Some(Bounds::new(
            point(px(0.), vp.height - px(40.)),
            size(px(1.), px(1.)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardInputSource() -> *const std::ffi::c_void;
    fn TISGetInputSourceProperty(
        source: *const std::ffi::c_void,
        property_key: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    static kTISPropertyInputSourceType: *const std::ffi::c_void;
    static kTISTypeKeyboardInputMode: *const std::ffi::c_void;
    static kTISTypeKeyboardInputMethodWithoutModes: *const std::ffi::c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFEqual(cf1: *const std::ffi::c_void, cf2: *const std::ffi::c_void) -> u8;
    fn CFRelease(cf: *const std::ffi::c_void);
}

/// Whether the current keyboard input source composes text — an input method
/// (Pinyin, Kotoeri, Sogou, WeType, ...) or an input mode — rather than a
/// plain layout (ABC, U.S., ...). Per-keystroke query;
/// TISCopyCurrentKeyboardInputSource is a refcount copy.
///
/// The test compares `kTISPropertyInputSourceType` against the type constants
/// by identity (`CFEqual`). A vendor prefix on `kTISPropertyInputSourceID`
/// ("com.apple.inputmethod.") misses third-party IMEs whose bundle ids are
/// not com.apple.* (Sogou, WeType, ...); their printable keystrokes then
/// reach BOTH the app (typed_text → EVENT_TEXT) and the IME (commit →
/// EVENT_TEXT), double-inserting every letter and digit.
fn ime_input_source_active() -> bool {
    unsafe {
        let source = TISCopyCurrentKeyboardInputSource();
        if source.is_null() {
            return false;
        }
        let ty = TISGetInputSourceProperty(source, kTISPropertyInputSourceType);
        let active = !ty.is_null()
            && (CFEqual(ty, kTISTypeKeyboardInputMethodWithoutModes) != 0
                || CFEqual(ty, kTISTypeKeyboardInputMode) != 0);
        CFRelease(source);
        active
    }
}

pub struct FfiView {
    focus: FocusHandle,
    /// Index into `VIEWS` whose committed tree this view renders.
    view: usize,
    /// Retained scroll handles, keyed by the div's `OP_SET_KEY` value. The tree
    /// is rebuilt from scratch on every state change, so a scroll div's
    /// position only survives the rebuild if its `ScrollHandle` lives outside
    /// the tree. `ScrollHandle` is `Rc`-based (not `Send`), so the store lives
    /// here in the per-view entity — which only ever runs on the main thread —
    /// rather than in the `Mutex`-guarded `VIEWS` global. `render_node` looks
    /// up (or inserts) the handle for keyed scroll divs; keyless scroll divs
    /// get a fresh handle per render and reset to the top on each rebuild.
    scroll_handles: Rc<RefCell<HashMap<String, ScrollHandle>>>,
    /// Retained text-input models, keyed by `input_id` (RFC 0003 §3.2). Same
    /// lifetime story as `scroll_handles`: the tree rebuilds, the models
    /// survive, and everything here is main-thread only.
    inputs: Rc<RefCell<HashMap<i32, Entity<TextInputModel>>>>,
}

impl FfiView {
    /// Forward one mouse event to MoonBit as an EVENT_ASYNC payload (see
    /// `MOUSE_TAG`): phase (0 down / 1 dragged move / 2 up) + rounded x/y
    /// window coords, little-endian i32. The adapter hit-tests its own layout
    /// to turn these into drag selection.
    fn dispatch_mouse(&self, phase: u8, position: Point<Pixels>, cx: &mut Context<Self>) {
        let x = f32::from(position.x).round() as i32;
        let y = f32::from(position.y).round() as i32;
        let mut payload = Vec::with_capacity(10);
        payload.push(MOUSE_TAG);
        payload.push(phase);
        payload.extend_from_slice(&x.to_le_bytes());
        payload.extend_from_slice(&y.to_le_bytes());
        let changed = dispatch_injected(self.view as i32, payload);
        notify_if_changed(changed, || cx.notify());
    }
}

/// Paint-phase registration of the app-level IME bridge. `Window::handle_input`
/// is a paint-time hook (it panics outside paint), so the bridge cannot be
/// registered from `render`; this transparent wrapper owns the registration
/// instead. A real OP_TEXT_INPUT widget paints later in the frame and
/// re-registers its own handler over the bridge (last registration wins), so
/// RFC 0003 behavior is untouched.
struct ImeBridgeProbe {
    child: AnyElement,
    view: i32,
    focus: FocusHandle,
}

impl IntoElement for ImeBridgeProbe {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ImeBridgeProbe {
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
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Transparent: the child's own layout node is ours.
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.handle_input(&self.focus, ImeBridge { view: self.view }, cx);
        self.child.paint(window, cx);
    }
}

impl Render for FfiView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Render the committed root for this view (swapped in by `commit_tree`).
        // Cloned out so the lock is not held while building GPUI elements.
        let root = {
            let guard = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(self.view).cloned().flatten()
        };
        let mut d = div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, win, cx| {
                let view = this.view as i32;
                // Keyboard navigation (issue #52): Tab moves focus to the next
                // tab stop, Shift+Tab to the previous one. The framework owns
                // this traversal, so Tab is consumed here and NOT forwarded to
                // MoonBit as a named key; every other key falls through to the
                // normal key/text dispatch below. `focus_next`/`focus_prev`
                // walk the tab stops the focusable divs registered at paint
                // time (see `render_node`); with no tab stops they are no-ops.
                if ev.keystroke.key == "tab" {
                    if ev.keystroke.modifiers.shift {
                        win.focus_prev();
                    } else {
                        win.focus_next();
                    }
                    return;
                }
                // Text-input suppression (RFC 0003 §3.4): while a text input
                // holds focus, keystrokes belong to the widget — typed text
                // flows through the platform input handler into
                // `replace_text_in_range`, and editing keys are consumed by
                // the widget's own listener. Forwarding them to the app-level
                // EVENT_KEY / EVENT_NAMED_KEY / EVENT_TEXT as well would
                // double-deliver every keystroke.
                let input_focused = this
                    .inputs
                    .borrow()
                    .values()
                    .any(|model| model.read(cx).focus.is_focused(win));
                if input_focused {
                    return;
                }
                // IME passthrough: when an input method is active, printable
                // keystrokes belong to the IME, not the app. Declining to
                // dispatch them (and NOT stopping propagation) lets the mac
                // window forward the native event to NSInputContext, whose
                // client is the ImeBridge registered in `render`; committed
                // text comes back as EVENT_TEXT. Cmd/Ctrl/Fn combos keep the
                // normal path — they are shortcuts, not composition input.
                // The unnamed-key marker keeps MoonBit's swallow-generation
                // counter in sync (adapter on_text), so a shortcut's swallowed
                // stray EVENT_TEXT cannot also swallow a later IME commit.
                let mods = &ev.keystroke.modifiers;
                if ime_owned_when_idle(&ev.keystroke.key, ev.keystroke.key_char.as_deref())
                    && !mods.control
                    && !mods.platform
                    && !mods.function
                    && ime_input_source_active()
                {
                    unsafe { mb_dispatch(ABI_VERSION, EVENT_NAMED_KEY, view, 0, 0) };
                    return;
                }
                let code = key_code(ev);
                let mods = mods_bits(&ev.keystroke.modifiers);
                if code != 0 {
                    let changed =
                        unsafe { mb_dispatch(ABI_VERSION, EVENT_KEY, view, code, mods) };
                    notify_if_changed(changed.max(take_input_dirty()), || cx.notify());
                } else if let Some(key_id) = named_key_id(&ev.keystroke.key) {
                    let changed =
                        unsafe { mb_dispatch(ABI_VERSION, EVENT_NAMED_KEY, view, key_id, mods) };
                    notify_if_changed(changed.max(take_input_dirty()), || cx.notify());
                }
                // Emit a text event for keys that produce typed characters
                // (including multi-char keys and IME-composed text). The
                // payload lives in EVENT_QUEUE; MoonBit copies it via
                // gpui_event_copy_text during the synchronous dispatch.
                if let Some(text) = typed_text(ev) {
                    notify_if_changed(dispatch_event_text(view, &text).max(take_input_dirty()), || {
                        cx.notify();
                    });
                }
            }))
            // Mouse input for the app's own drag selection: left-button
            // down/up and dragged moves are forwarded as EVENT_ASYNC payloads
            // (see `MOUSE_TAG`). Move events are only sent while the left
            // button is held, so idle hovering never wakes MoonBit. Clicks
            // keep flowing through the per-div on_click path untouched.
            .on_mouse_down(MouseButton::Left, cx.listener(|this, ev: &MouseDownEvent, _win, cx| {
                this.dispatch_mouse(0, ev.position, cx);
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, ev: &MouseUpEvent, _win, cx| {
                this.dispatch_mouse(2, ev.position, cx);
            }))
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _win, cx| {
                if ev.dragging() {
                    this.dispatch_mouse(1, ev.position, cx);
                }
            }));
        if let Some(node) = &root {
            if let Some(el) = render_node(
                node,
                cx,
                true,
                &self.scroll_handles,
                &self.inputs,
                self.view as i32,
                &Cell::new(0),
                &Cell::new(0),
            ) {
                d = d.child(el);
            }
        }
        let element = ImeBridgeProbe {
            child: d.into_any_element(),
            view: self.view as i32,
            focus: self.focus.clone(),
        }
        .into_any_element();
        if benchmark_window_active() {
            BenchmarkFrameProbe { child: element }.into_any_element()
        } else {
            element
        }
    }
}

fn benchmark_window_active() -> bool {
    WINDOW_BENCHMARK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

struct BenchmarkFrameProbe {
    child: AnyElement,
}

impl Element for BenchmarkFrameProbe {
    type RequestLayoutState = std::time::Instant;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let started = std::time::Instant::now();
        let layout = self.child.request_layout(window, cx);
        (layout, started)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        let elapsed = benchmark_milliseconds(*state, std::time::Instant::now());
        if let Some(benchmark) = WINDOW_BENCHMARK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            if benchmark.first_interactive_ms == 0.0 {
                // Paint is the earliest reliable point at which the initial
                // frame is usable. on_next_frame is a scheduling callback and
                // may arrive later (or before this pass), so startup must be
                // anchored to the measured paint completion instead.
                benchmark.first_interactive_ms =
                    benchmark_milliseconds(benchmark.started, std::time::Instant::now());
            }
            benchmark.paint_work_ms = Some(elapsed);
        }
    }
}

impl IntoElement for BenchmarkFrameProbe {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Codepoint of a single-character key (letters/digits/…); 0 for named or
/// multi-char keys (up/down/enter/…), which `named_key_id` maps to an ABI id.
/// Rust only translates the platform key to a scalar; MoonBit decides what it does.
fn key_code(ev: &KeyDownEvent) -> i32 {
    let mut chars = ev.keystroke.key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => c as i32,
        _ => 0,
    }
}

/// Map a GPUI named key string to its ABI id. Returns `None` for single-char
/// keys (handled by `key_code`) and unrecognized names.
fn named_key_id(key: &str) -> Option<i32> {
    match key {
        "enter" => Some(KEY_ENTER),
        "escape" => Some(KEY_ESCAPE),
        "up" => Some(KEY_UP),
        "down" => Some(KEY_DOWN),
        "left" => Some(KEY_LEFT),
        "right" => Some(KEY_RIGHT),
        "tab" => Some(KEY_TAB),
        "backspace" => Some(KEY_BACKSPACE),
        "delete" => Some(KEY_DELETE),
        "home" => Some(KEY_HOME),
        "end" => Some(KEY_END),
        "pageup" => Some(KEY_PAGEUP),
        "pagedown" => Some(KEY_PAGEDOWN),
        _ => None,
    }
}

/// Typed text for a key event: `key_char` if present (the character that would
/// be inserted, including IME-composed and multi-char keys), else the single
/// `key` character. Returns `None` for pure modifier/navigation keys.
fn typed_text(ev: &KeyDownEvent) -> Option<String> {
    if let Some(s) = &ev.keystroke.key_char {
        if !s.is_empty() {
            return Some(s.clone());
        }
    }
    let k = &ev.keystroke.key;
    if k.chars().count() == 1 && !k.chars().next().unwrap().is_control() {
        Some(k.clone())
    } else {
        None
    }
}

/// Whether a keydown must be handed to the IME even while no composition is
/// in flight (the IME-passthrough branch). Printable keys are IME-owned: with
/// a composing input source selected they must reach NSInputContext so the
/// commit comes back as EVENT_TEXT. Enter is NOT IME-owned when idle: a
/// non-composing IME answers a Return keydown with
/// `doCommandBySelector:insertNewline:`, which gpui drops
/// (`keystroke_for_do_command` is only set on the composing path), so
/// intercepting it lost the key entirely — no newline, no text. Routing it to
/// the normal `named_key_id` path restores EVENT_NAMED_KEY(KEY_ENTER). Safe
/// even while composing: the mac window routes composing keydowns to the
/// input context before this listener ever runs.
fn ime_owned_when_idle(key: &str, key_char: Option<&str>) -> bool {
    key != "enter" && key_char.is_some_and(|s| !s.is_empty())
}

/// Pack modifier flags into the `b` payload slot (bit0 ctrl, 1 alt, 2 shift,
/// 3 platform/cmd, 4 fn). Unused by the demo but kept for completeness.
fn mods_bits(m: &Modifiers) -> i32 {
    (m.control as i32) * MOD_CTRL
        | (m.alt as i32) * MOD_ALT
        | (m.shift as i32) * MOD_SHIFT
        | (m.platform as i32) * MOD_PLATFORM
        | (m.function as i32) * MOD_FUNCTION
}

fn notify_if_changed(changed: i32, notify: impl FnOnce()) {
    if changed == 1 {
        notify();
    }
}

/// A non-negative pixel value as a definite `Length`; a negative sentinel maps to
/// `Length::Auto` (the "unset/auto" encoding used by inset operands).
fn px_or_auto(v: f32) -> Length {
    if v >= 0.0 {
        px(v).into()
    } else {
        Length::Auto
    }
}

/// Map an ABI `ALIGN_*` id to a gpui `AlignItems`. `ALIGN_DEFAULT` (0) and any
fn map_align_items(id: i32) -> Option<AlignItems> {
    match id {
        ALIGN_DEFAULT => None,
        ALIGN_START => Some(AlignItems::Start),
        ALIGN_CENTER => Some(AlignItems::Center),
        ALIGN_END => Some(AlignItems::End),
        ALIGN_STRETCH => Some(AlignItems::Stretch),
        _ => None,
    }
}

/// Map an ABI `JUSTIFY_*` id to a gpui `JustifyContent` (an `AlignContent`).
/// `JUSTIFY_DEFAULT` (0) and any unknown id map to `None`.
fn map_justify_content(id: i32) -> Option<JustifyContent> {
    match id {
        JUSTIFY_DEFAULT => None,
        JUSTIFY_START => Some(JustifyContent::Start),
        JUSTIFY_CENTER => Some(JustifyContent::Center),
        JUSTIFY_END => Some(JustifyContent::End),
        JUSTIFY_SPACE_BETWEEN => Some(JustifyContent::SpaceBetween),
        JUSTIFY_SPACE_AROUND => Some(JustifyContent::SpaceAround),
        _ => None,
    }
}

/// Map an ABI `OVERFLOW_*` id to a gpui `Overflow`. Unknown ids map to `None`.
/// `Scroll` becomes real scrolling in `render_node`: any div whose overflow is
/// `Scroll` on either axis is tracked with a retained `ScrollHandle` (keyed by
/// the node's `OP_SET_KEY` value when present), so scroll position survives the
/// full tree rebuild that every state change triggers.
fn map_overflow(id: i32) -> Option<Overflow> {
    match id {
        OVERFLOW_VISIBLE => Some(Overflow::Visible),
        OVERFLOW_HIDDEN => Some(Overflow::Hidden),
        OVERFLOW_SCROLL => Some(Overflow::Scroll),
        _ => None,
    }
}

/// Map an ABI `CURSOR_*` id to a gpui `CursorStyle`. Unknown ids map to `None`.
fn map_cursor(id: i32) -> Option<CursorStyle> {
    match id {
        CURSOR_ARROW => Some(CursorStyle::Arrow),
        CURSOR_POINTER => Some(CursorStyle::PointingHand),
        CURSOR_TEXT => Some(CursorStyle::IBeam),
        CURSOR_CROSSHAIR => Some(CursorStyle::Crosshair),
        CURSOR_GRAB => Some(CursorStyle::OpenHand),
        CURSOR_GRABBING => Some(CursorStyle::ClosedHand),
        CURSOR_NOT_ALLOWED => Some(CursorStyle::OperationNotAllowed),
        CURSOR_EW_RESIZE => Some(CursorStyle::ResizeLeftRight),
        CURSOR_NS_RESIZE => Some(CursorStyle::ResizeUpDown),
        CURSOR_COL_RESIZE => Some(CursorStyle::ResizeColumn),
        CURSOR_ROW_RESIZE => Some(CursorStyle::ResizeRow),
        CURSOR_NONE => Some(CursorStyle::None),
        _ => None,
    }
}

/// Map an ABI `TEXT_ALIGN_*` id to a gpui `TextAlign`. `TEXT_ALIGN_DEFAULT` (0)
/// and unknown ids map to `None`. `TEXT_ALIGN_JUSTIFY` maps to `Left`: gpui
/// 0.2.2's `TextAlign` has no `Justify` variant, so the closest supported
/// alignment is used (see `docs/framework-gaps.md` G8).
fn map_text_align(id: i32) -> Option<TextAlign> {
    match id {
        TEXT_ALIGN_DEFAULT => None,
        TEXT_ALIGN_LEFT => Some(TextAlign::Left),
        TEXT_ALIGN_CENTER => Some(TextAlign::Center),
        TEXT_ALIGN_RIGHT => Some(TextAlign::Right),
        TEXT_ALIGN_JUSTIFY => Some(TextAlign::Left),
        _ => None,
    }
}

/// Map an ABI `WHITESPACE_*` id to a gpui `WhiteSpace`. `WHITESPACE_DEFAULT`
/// (0) and unknown ids map to `None`. `PRE`/`PRE_WRAP` map to `Nowrap`/`Normal`:
/// gpui 0.2.2's `WhiteSpace` has only `Normal` (wrap) and `Nowrap` (no wrap);
/// literal-whitespace preservation is a property of the text content itself.
fn map_whitespace(id: i32) -> Option<WhiteSpace> {
    match id {
        WHITESPACE_DEFAULT => None,
        WHITESPACE_NORMAL => Some(WhiteSpace::Normal),
        WHITESPACE_NOWRAP => Some(WhiteSpace::Nowrap),
        WHITESPACE_PRE => Some(WhiteSpace::Nowrap),
        WHITESPACE_PRE_WRAP => Some(WhiteSpace::Normal),
        _ => None,
    }
}

/// Look up (or create) the retained `ScrollHandle` for a scroll div. Keyed
/// divs (those with an `OP_SET_KEY` value) reuse the same handle across every
/// rebuild, so their scroll position persists; keyless divs get a fresh handle
/// each render and reset to the top. Handles live in the per-view store because
/// `ScrollHandle` is `Rc`-based and not `Send`, so it cannot sit in the
/// `Mutex`-guarded `VIEWS` global.
fn scroll_handle_for(
    scroll_handles: &Rc<RefCell<HashMap<String, ScrollHandle>>>,
    key: Option<&str>,
) -> ScrollHandle {
    match key {
        Some(key) => scroll_handles
            .borrow_mut()
            .entry(key.to_owned())
            .or_insert_with(ScrollHandle::new)
            .clone(),
        None => ScrollHandle::new(),
    }
}

/// Build the GPUI element for one committed node. `scroll_handles` is the
/// per-view retained-handle store (see `FfiView.scroll_handles`): scroll divs
/// look up or insert their handle here so scroll position survives the full
/// tree rebuild that every state change triggers. `keyless_scroll_id` hands
/// out per-render ids for scroll divs without an `OP_SET_KEY` (their handle is
/// ephemeral and their position resets on each rebuild). `keyless_focus_id`
/// does the same for focusable divs that have neither a key nor a click id:
/// the focus builders need element state, which needs an id, so one is
/// synthesized per render (and the focus handle resets on each rebuild).
///
/// Recursion happens through this wrapper so every level of descent gets its
/// stack headroom checked (issue #74): these frames are the ~70 KB ones that
/// make a plain thread stack overflow in the low tens of levels, and an
/// overflow aborts rather than returning a status. `MAX_TREE_DEPTH` bounds the
/// committed tree independently, so the growth here is what keeps a legitimate
/// deep-but-under-the-limit tree renderable rather than a safety net for
/// unbounded input.
fn render_node(
    node: &UiNode,
    cx: &mut Context<FfiView>,
    fill_available_space: bool,
    scroll_handles: &Rc<RefCell<HashMap<String, ScrollHandle>>>,
    inputs: &Rc<RefCell<HashMap<i32, Entity<TextInputModel>>>>,
    view_id: i32,
    keyless_scroll_id: &Cell<usize>,
    keyless_focus_id: &Cell<usize>,
) -> Option<AnyElement> {
    stacker::maybe_grow(STACK_RED_ZONE, STACK_GROW_BY, || {
        render_node_inner(
            node,
            cx,
            fill_available_space,
            scroll_handles,
            inputs,
            view_id,
            keyless_scroll_id,
            keyless_focus_id,
        )
    })
}

fn render_node_inner(
    node: &UiNode,
    cx: &mut Context<FfiView>,
    fill_available_space: bool,
    scroll_handles: &Rc<RefCell<HashMap<String, ScrollHandle>>>,
    inputs: &Rc<RefCell<HashMap<i32, Entity<TextInputModel>>>>,
    view_id: i32,
    keyless_scroll_id: &Cell<usize>,
    keyless_focus_id: &Cell<usize>,
) -> Option<AnyElement> {
    match node {
        UiNode::Div {
            width,
            height,
            bg,
            flex,
            flex_col,
            center,
            gap,
            rounded,
            padding,
            border_width,
            border_color,
            bg_color,
            margin,
            min_size,
            max_size,
            flex_item,
            align,
            overflow,
            opacity,
            shadow,
            cursor,
            position,
            inset,
            padding_sides,
            text_size,
            text_color,
            font_weight,
            line_height,
            text_align,
            whitespace,
            font_family,
            on_click,
            focusable,
            tab_index,
            tab_stop,
            key,
            scroll_id,
            children,
        } => {
            // Build children first (recursion borrows `cx`), then attach the
            // click listener (also borrows `cx`) — kept sequential to avoid an
            // aliasing borrow.
            let mut child_elements: Vec<AnyElement> = Vec::new();
            for child in children {
                if let Some(el) = render_node(
                    child,
                    cx,
                    false,
                    scroll_handles,
                    inputs,
                    view_id,
                    keyless_scroll_id,
                    keyless_focus_id,
                ) {
                    child_elements.push(el);
                }
            }
            let mut d = div();
            if fill_available_space {
                d = d.size_full();
            }
            if *width > 0.0 && *height > 0.0 {
                d = d.w(px(*width)).h(px(*height));
            }
            if let Some((r, g, b, a)) = bg_color {
                // G9: RGBA background with alpha. `rgba()` packs 0xRRGGBBAA
                // (big-endian byte order), so alpha rides in the low byte.
                d = d.bg(rgba(
                    ((*r as u32) << 24)
                        | ((*g as u32) << 16)
                        | ((*b as u32) << 8)
                        | (*a as u32),
                ));
            } else if let Some((r, g, b)) = bg {
                d = d.bg(rgb(((*r as u32) << 16) | ((*g as u32) << 8) | (*b as u32)));
            }
            if *flex {
                d = d.flex();
                if *flex_col {
                    d = d.flex_col();
                }
            }
            if *center {
                d = d.justify_center().items_center();
            }
            if *gap > 0.0 {
                d = d.gap(px(*gap));
            }
            if *rounded > 0.0 {
                d = d.rounded(px(*rounded));
            }
            if let Some((top, right, bottom, left)) = padding_sides {
                // Per-side padding (G7) overrides the uniform `padding` above.
                d.style().padding.top = Some(px(*top).into());
                d.style().padding.right = Some(px(*right).into());
                d.style().padding.bottom = Some(px(*bottom).into());
                d.style().padding.left = Some(px(*left).into());
            } else if *padding > 0.0 {
                d = d.p(px(*padding));
            }
            if *border_width > 0.0 {
                d = d.border(px(*border_width));
                if let Some((r, g, b)) = border_color {
                    d = d.border_color(rgb(
                        ((*r as u32) << 16) | ((*g as u32) << 8) | (*b as u32),
                    ));
                }
            }
            // --- G7 core layout/style (issue #51) -------------------------
            if let Some((top, right, bottom, left)) = margin {
                d.style().margin.top = Some(px(*top).into());
                d.style().margin.right = Some(px(*right).into());
                d.style().margin.bottom = Some(px(*bottom).into());
                d.style().margin.left = Some(px(*left).into());
            }
            if let Some((w, h)) = min_size {
                if *w >= 0.0 {
                    d.style().min_size.width = Some(px(*w).into());
                }
                if *h >= 0.0 {
                    d.style().min_size.height = Some(px(*h).into());
                }
            }
            if let Some((w, h)) = max_size {
                if *w >= 0.0 {
                    d.style().max_size.width = Some(px(*w).into());
                }
                if *h >= 0.0 {
                    d.style().max_size.height = Some(px(*h).into());
                }
            }
            if let Some((grow, shrink, basis)) = flex_item {
                d.style().flex_grow = Some(*grow);
                d.style().flex_shrink = Some(*shrink);
                d.style().flex_basis = Some(if *basis >= 0.0 {
                    px(*basis).into()
                } else {
                    Length::Auto
                });
            }
            if let Some((align_items, justify_content)) = align {
                if let Some(v) = map_align_items(*align_items) {
                    d.style().align_items = Some(v);
                }
                if let Some(v) = map_justify_content(*justify_content) {
                    d.style().justify_content = Some(v);
                }
            }
            // G6 scroll: `Overflow::Scroll` on either axis makes this a real
            // scroll container. The handle is retained per view (keyed by the
            // node's `OP_SET_KEY` value) so the scroll position survives the
            // full tree rebuild every state change triggers; keyless scroll
            // divs get a fresh handle and reset to the top on each rebuild.
            // The handle is applied in the identity branches below, where the
            // element has an id (`track_scroll` needs `StatefulInteractiveElement`).
            let scroll_handle = if let Some((x, y)) = overflow {
                if let Some(v) = map_overflow(*x) {
                    d.style().overflow.x = Some(v);
                }
                if let Some(v) = map_overflow(*y) {
                    d.style().overflow.y = Some(v);
                }
                if *x == OVERFLOW_SCROLL || *y == OVERFLOW_SCROLL {
                    Some(scroll_handle_for(scroll_handles, key.as_deref()))
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(op) = opacity {
                d = d.opacity(*op);
            }
            if let Some(shadow) = shadow {
                let (r, g, b, a) = shadow.color;
                let color = rgba(
                    ((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | (a as u32),
                );
                d = d.shadow(vec![BoxShadow {
                    color: color.into(),
                    offset: point(px(shadow.x), px(shadow.y)),
                    blur_radius: px(shadow.blur),
                    spread_radius: px(shadow.spread),
                }]);
            }
            if let Some(mode) = position {
                if *mode == POSITION_ABSOLUTE {
                    d.style().position = Some(Position::Absolute);
                } else if *mode == POSITION_RELATIVE {
                    d.style().position = Some(Position::Relative);
                }
            }
            if let Some((top, right, bottom, left)) = inset {
                d.style().inset.top = Some(px_or_auto(*top));
                d.style().inset.right = Some(px_or_auto(*right));
                d.style().inset.bottom = Some(px_or_auto(*bottom));
                d.style().inset.left = Some(px_or_auto(*left));
            }
            if let Some(kind) = cursor {
                // An explicit cursor on a clickable div is superseded by the
                // pointer applied in the identity/click branches below.
                if let Some(v) = map_cursor(*kind) {
                    d.style().mouse_cursor = Some(v);
                }
            }
            // --- G8 typography (issue #51) --------------------------------
            // Applied to the div's `Style.text` refinement: gpui pushes it via
            // `with_text_style` around child layout/paint (div.rs), and the
            // text element reads the folded stack (`window.text_style()`), so
            // every descendant text node inherits these values.
            if text_size.is_some()
                || text_color.is_some()
                || font_weight.is_some()
                || line_height.is_some()
                || text_align.is_some()
                || whitespace.is_some()
                || font_family.is_some()
            {
                let text = d.style().text.get_or_insert_with(Default::default);
                if let Some(size) = text_size {
                    text.font_size = Some(AbsoluteLength::Pixels(px(*size)));
                }
                if let Some((r, g, b, a)) = text_color {
                    text.color = Some(
                        rgba(
                            ((*r as u32) << 24)
                                | ((*g as u32) << 16)
                                | ((*b as u32) << 8)
                                | (*a as u32),
                        )
                        .into(),
                    );
                }
                if let Some(weight) = font_weight {
                    text.font_weight = Some(FontWeight(*weight as f32));
                }
                if let Some(lh) = line_height {
                    text.line_height = Some(px(*lh).into());
                }
                if let Some(id) = text_align {
                    if let Some(v) = map_text_align(*id) {
                        text.text_align = Some(v);
                    }
                }
                if let Some(id) = whitespace {
                    if let Some(v) = map_whitespace(*id) {
                        text.white_space = Some(v);
                    }
                }
                if let Some(family) = font_family {
                    text.font_family = Some(SharedString::from(family.clone()));
                }
            }
            d.extend(child_elements);
            // Element identity: an explicit key (set via `gpui_set_key`) is the
            // stable identity, independent of click routing. Without a key, a
            // clickable div falls back to its click id (the historical scheme).
            // A keyed div gets an id even when not clickable, so stateful
            // elements that only need identity (not click routing) are stable
            // across rebuilds. Duplicate keys are rejected at commit, so ids
            // never collide here. A scroll div always gets an id (scroll
            // tracking requires element state): keyed ones use their key,
            // keyless scroll divs get an ephemeral per-render id.
            //
            // Keyboard navigation (issue #52): `.focusable()` / `.tab_index()` /
            // `.tab_stop()` all live on `StatefulInteractiveElement`, so they
            // need an element id. A focusable div without a key or click id
            // synthesizes one below (the `keyless_focus_id` counter, mirroring
            // the keyless-scroll scheme). `tab_index`/`tab_stop` imply
            // focusability, so setting either also makes the div focusable.
            let focus_nav = focusable.unwrap_or(false)
                || tab_index.is_some()
                || tab_stop.is_some();
            // Apply the a11y focus builders to a stateful element. Order
            // matters only in that `tab_index` sets `tab_stop = true`, so an
            // explicit `tab_stop(false)` must come after to win.
            let apply_focus = |mut el: Stateful<Div>| {
                if focus_nav {
                    el = el.focusable();
                }
                if let Some(idx) = tab_index {
                    el = el.tab_index(*idx);
                }
                if let Some(stop) = tab_stop {
                    el = el.tab_stop(*stop);
                }
                el
            };
            let el = match (key.as_deref(), *on_click) {
                (Some(key), on_click) => {
                    let mut d = d.id(SharedString::from(format!("gpui_key:{key}")));
                    // G24 headless harness: expose this div's laid-out bounds to
                    // `VisualTestContext::debug_bounds` under its key. Compiles
                    // to a no-op without gpui's `test-support` feature, so the
                    // shipped staticlib pays nothing.
                    d = d.debug_selector(|| key.to_string());
                    if let Some(handle) = &scroll_handle {
                        d = d.track_scroll(handle);
                    }
                    d = apply_focus(d);
                    if let Some(cid) = on_click {
                        d = d
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _ev: &ClickEvent, _win, cx| {
                                let view = this.view as i32;
                                let changed = unsafe { mb_dispatch(ABI_VERSION, EVENT_CLICK, view, cid, 0) };
                                notify_if_changed(changed.max(take_input_dirty()), || cx.notify());
                            }));
                    }
                    Some(d.into_any_element())
                }
                (None, Some(cid)) => {
                    // Legacy: identity synthesized from the click id.
                    let mut el = d.id(("gpui_click", cid as usize));
                    if let Some(handle) = &scroll_handle {
                        el = el.track_scroll(handle);
                    }
                    el = apply_focus(el);
                    let el = el
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _ev: &ClickEvent, _win, cx| {
                            let view = this.view as i32;
                            let changed = unsafe { mb_dispatch(ABI_VERSION, EVENT_CLICK, view, cid, 0) };
                            notify_if_changed(changed.max(take_input_dirty()), || cx.notify());
                        }))
                        .into_any_element();
                    Some(el)
                }
                (None, None) => match (&scroll_handle, focus_nav) {
                    (Some(handle), _) => {
                        // Keyless scroll div: `track_scroll` needs element state,
                        // which needs an id, so synthesize one. The counter only
                        // disambiguates multiple keyless scroll divs within one
                        // render; it resets each render. Position still resets on
                        // every rebuild because the handle is fresh (a tracked
                        // handle's offset comes from the handle, not element state).
                        let id = keyless_scroll_id.get();
                        keyless_scroll_id.set(id + 1);
                        let el = d.id(("gpui_scroll", id)).track_scroll(handle);
                        Some(apply_focus(el).into_any_element())
                    }
                    (None, true) => {
                        // Focusable but neither keyed nor clickable: synthesize
                        // an id so the focus builders (which require element
                        // state) can attach. The counter disambiguates multiple
                        // such divs within one render and resets each render, so
                        // the focus handle is ephemeral across rebuilds — exactly
                        // like a keyless scroll div. Give the div a key via
                        // `set_key` for focus that survives rebuilds.
                        let id = keyless_focus_id.get();
                        keyless_focus_id.set(id + 1);
                        Some(apply_focus(d.id(("gpui_focus", id))).into_any_element())
                    }
                    (None, false) => Some(d.into_any_element()),
                },
            };
            // Probe keys: "caret" is the app's drawn caret (candidate anchor);
            // "probe:*" divs publish their window-space rects to the app's
            // hit-testing (gpui_probe_rect pull ABI). Wrap the element so its
            // prepaint bounds are recorded by `ProbeBoundsProbe`.
            let el = if let Some(k) = key.as_deref().filter(|k| *k == "caret" || k.starts_with("probe:")) {
                el.map(|child| {
                    ProbeBoundsProbe {
                        child,
                        key: k.to_string(),
                    }
                    .into_any_element()
                })
            } else {
                el
            };
            // Scroll feedback subscription (issue #89): wrap the subscribed
            // div so the settled offset is observed after every paint. Only a
            // div that actually scrolls (a tracked handle exists) can feed
            // back — an `OP_SET_SCROLL_ID` without `OP_SET_OVERFLOW`'s SCROLL
            // axis is inert by construction.
            match (el, *scroll_id, &scroll_handle) {
                (Some(el), Some(sid), Some(handle)) => Some(
                    ScrollFeedback {
                        child: el,
                        view: view_id,
                        scroll_id: sid,
                        handle: handle.clone(),
                        entity: cx.weak_entity(),
                    }
                    .into_any_element(),
                ),
                (el, _, _) => el,
            }
        }
        UiNode::Text {
            content,
            color: (r, g, b),
            size,
            runs,
        } => {
            // The content string flows through unmodified (issue #16). The
            // first-glyph subpixel fix lives in `TextGlyphInset`, a
            // paint-time-only shim — see its doc comment and
            // `docs/troubleshooting.md` §2.
            let mut text = div()
                .text_color(rgb(((*r as u32) << 16) | ((*g as u32) << 8) | (*b as u32)))
                .text_size(px(*size))
                // G24 headless harness: expose this text element's laid-out
                // bounds under `text:<content>` (no-op without `test-support`).
                .debug_selector(|| format!("text:{content}"));
            if runs.is_empty() {
                // Single-style text: the pre-#91 path, unchanged.
                text = text.child(content.clone());
            } else {
                // Rich text (issue #91): per-run overrides ride
                // `StyledText::with_highlights`, whose delayed variant
                // resolves the base style from `window.text_style()` at
                // layout — i.e. the same inherited div style the plain path
                // sees, so a run only overrides what its flags name. Ranges
                // were validated at decode time (see `OP_TEXT_RUN`), which is
                // what keeps gpui's panicking run machinery safe here.
                let styled = StyledText::new(SharedString::from(content.clone()))
                    .with_highlights(runs.iter().map(|run| {
                        (run.start..run.start + run.len, highlight_for_run(run))
                    }));
                #[cfg(any(test, feature = "test-support"))]
                text_layout_stash(content, styled.layout().clone());
                text = text.child(styled);
            }
            let inset = TextGlyphInset {
                child: text.into_any_element(),
            };
            Some(inset.into_any_element())
        }
        UiNode::TextInput {
            input_id,
            placeholder,
        } => {
            let input_id = *input_id;
            // Get-or-create the retained model (RFC 0003 §3.2): the entity
            // survives rebuilds; only the placeholder follows the tree.
            let model = {
                let mut map = inputs.borrow_mut();
                map.entry(input_id)
                    .or_insert_with(|| {
                        let focus = cx.focus_handle();
                        let model = cx.new(|_| TextInputModel {
                            view: view_id,
                            input_id,
                            content: String::new(),
                            placeholder: placeholder.clone(),
                            selected_range: 0..0,
                            marked_range: None,
                            last_layout: None,
                            last_bounds: None,
                            focus,
                        });
                        // Seed the mirror so the pull ABI works before the
                        // first edit.
                        mirror_update(view_id, input_id, "", false);
                        model
                    })
                    .clone()
            };
            model.update(cx, |m, _| {
                if m.placeholder != *placeholder {
                    m.placeholder = placeholder.clone();
                }
            });
            let focus = model.read(cx).focus.clone();
            let mouse_model = model.clone();
            let key_model = model.clone();
            let el = div()
                .id(("gpui_input", input_id as usize))
                .w_full()
                .track_focus(&focus)
                .tab_index(0)
                .tab_stop(true)
                .cursor(CursorStyle::IBeam)
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    let focus = mouse_model.read(cx).focus.clone();
                    window.focus(&focus);
                })
                .on_key_down(move |ev: &KeyDownEvent, window, cx| {
                    key_model.update(cx, |m, cx| {
                        m.handle_editing_key(ev.keystroke.key.as_str(), window, cx);
                    });
                })
                // G24 headless harness hook, mirroring the text arm.
                .debug_selector(|| format!("input:{input_id}"))
                .child(TextInputElement { input: model });
            Some(el.into_any_element())
        }
    }
}

/// Paint-phase observer for scroll position feedback (issue #89).
///
/// Wraps a scroll div that carries an `OP_SET_SCROLL_ID`. Layout is delegated
/// transparently (same shape as [`TextGlyphInset`]); the work happens after
/// the child paints: gpui's own wheel listener mutates the tracked
/// [`ScrollHandle`] and the div's prepaint clamps the offset to the content
/// bounds, so post-paint is the first moment the settled value is observable.
/// Every paint refreshes [`SCROLL_MIRROR`]; when the clamped offset differs
/// from the last announced one, the `EVENT_SCROLL` dispatch is deferred via
/// [`App::defer`] — dispatch re-enters MoonBit, which may commit a new tree
/// and mark entities dirty, none of which is legal in the middle of a window
/// draw.
///
/// The re-clamp here is not redundant: gpui's wheel handler adds the delta
/// unclamped and only the *next* prepaint clamps it back, silently — no
/// second notify. Without clamping at the observation point the announced
/// offset could overshoot the real range and never be corrected.
struct ScrollFeedback {
    child: AnyElement,
    view: i32,
    scroll_id: i32,
    handle: ScrollHandle,
    entity: WeakEntity<FfiView>,
}

impl Element for ScrollFeedback {
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
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Transparent: the child's own layout node is ours.
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        let max = self.handle.max_offset();
        let raw = self.handle.offset();
        let offset = (
            f32::from(raw.x.clamp(-max.width, px(0.))),
            f32::from(raw.y.clamp(-max.height, px(0.))),
        );
        scroll_mirror_update(
            self.view,
            self.scroll_id,
            ScrollMirrorEntry {
                offset,
                max: (f32::from(max.width), f32::from(max.height)),
                viewport: (f32::from(bounds.size.width), f32::from(bounds.size.height)),
            },
        );
        // Edge detection: announce only a change, and seed silently on the
        // first observation (nothing scrolled yet — the initial position is
        // always pullable).
        let announce = {
            let mut sent = SCROLL_SENT.lock().unwrap_or_else(|e| e.into_inner());
            match sent
                .get_or_insert_with(HashMap::new)
                .insert((self.view, self.scroll_id), offset)
            {
                None => false,
                Some(prev) => prev != offset,
            }
        };
        if announce {
            let (view, scroll_id, entity) = (self.view, self.scroll_id, self.entity.clone());
            cx.defer(move |cx| {
                let changed = unsafe { mb_dispatch(ABI_VERSION, EVENT_SCROLL, view, scroll_id, 0) };
                notify_if_changed(changed.max(take_input_dirty()), || {
                    let _ = entity.update(cx, |_, cx| cx.notify());
                });
            });
        }
    }
}

impl IntoElement for ScrollFeedback {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Map one decoded run onto gpui's [`HighlightStyle`] (issue #91). Only the
/// fields named by the run's `RUN_STYLE_*` flags are set; everything else
/// stays `None`, which `StyledText::compute_runs` resolves as "inherit the
/// base text style". Weight shares the decode-side 100–900 clamp contract
/// with `OP_SET_FONT_WEIGHT`; underline/strikethrough take gpui's 1px default
/// thickness and inherit the run's text color (`color: None`).
fn highlight_for_run(run: &TextRunSpec) -> HighlightStyle {
    let mut style = HighlightStyle::default();
    if run.flags & RUN_STYLE_COLOR != 0 {
        let (r, g, b, a) = run.color;
        style.color = Some(
            rgba(((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | (a as u32)).into(),
        );
    }
    if run.flags & RUN_STYLE_WEIGHT != 0 {
        style.font_weight = Some(FontWeight(run.weight.clamp(100, 900) as f32));
    }
    if run.flags & RUN_STYLE_ITALIC != 0 {
        style.font_style = Some(FontStyle::Italic);
    }
    if run.flags & RUN_STYLE_UNDERLINE != 0 {
        style.underline = Some(UnderlineStyle {
            thickness: px(1.),
            color: None,
            wavy: false,
        });
    }
    if run.flags & RUN_STYLE_STRIKETHROUGH != 0 {
        style.strikethrough = Some(StrikethroughStyle {
            thickness: px(1.),
            color: None,
        });
    }
    if run.flags & RUN_STYLE_BACKGROUND != 0 {
        let (r, g, b, a) = run.background;
        style.background_color = Some(
            rgba(((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | (a as u32)).into(),
        );
    }
    style
}

/// Test-only stash of rich-text [`TextLayout`] handles, keyed by content
/// (issue #91). `TextLayout` is an `Rc` around the layout gpui fills in during
/// draw, so a clone taken at render time lets a headless test map run
/// boundaries to pixel positions (`position_for_index`) after the draw — the
/// `debug_bounds` hook only exposes whole-element geometry, not intra-text
/// positions. Thread-local because `TextLayout` is not `Send`; tests render
/// and read on the same thread. Compiled out of the shipped staticlib.
#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEXT_LAYOUTS: RefCell<HashMap<String, TextLayout>> = RefCell::new(HashMap::new());
}

#[cfg(any(test, feature = "test-support"))]
fn text_layout_stash(content: &str, layout: TextLayout) {
    TEXT_LAYOUTS.with(|stash| {
        stash.borrow_mut().insert(content.to_string(), layout);
    });
}

/// Fetch the stashed layout handle for a rich-text node rendered on this
/// thread. `None` when no rich-text node with this content has rendered.
#[cfg(any(test, feature = "test-support"))]
pub fn text_layout_for(content: &str) -> Option<TextLayout> {
    TEXT_LAYOUTS.with(|stash| stash.borrow().get(content).cloned())
}

/// Paint-time-only wrapper that shifts a text element's prepaint origin by a
/// fractional ¼px so its first glyph escapes GPUI's subpixel variant 0.
///
/// GPUI rounds taffy layout to whole pixels (`taffy.enable_rounding()`), so a
/// text element's left edge always lands at an integer x and its first glyph
/// is rasterized at subpixel variant 0 — a hard, un-antialiased left edge
/// (the ~1px "cut" on a leading round glyph such as "G"; see
/// `docs/troubleshooting.md` §2 for the full incident).
///
/// The historical workaround padded the content string with spaces, which
/// polluted the text MoonBit sent (issue #16). This shim keeps the content
/// string untouched: it delegates layout transparently to the child (the
/// layout box is unchanged), and applies the ¼px shift only to the prepaint
/// origin via `Window::with_element_offset`. `Window::layout_bounds` folds
/// the element offset into the child's prepaint bounds, so the first glyph's
/// pen position carries a ¼px fraction — subpixel variant 1 — and gets the
/// same antialiasing as interior glyphs.
///
/// ¼px was chosen because it stays fractional at the scale factors GPUI
/// actually ships (1×, 2×, 3×: 0.25·n is never an integer), whereas ½px would
/// re-snap to variant 0 at 2× HiDPI — the very platform where the original
/// incident was observed. At an exotic scale where 0.25·n is integral (4×,
/// 8×) the glyph falls back to variant 0, i.e. exactly what GPUI renders for
/// every line-leading glyph by default — never worse than unmitigated. The
/// inset is invisible: it moves glyph ink by a quarter pixel and reserves no
/// layout space, so siblings and centering are unaffected.
struct TextGlyphInset {
    child: AnyElement,
}

impl Element for TextGlyphInset {
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
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Transparent: the child's own layout node is ours.
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        window.with_element_offset(point(px(0.25), px(0.0)), |window| {
            self.child.prepaint(window, cx);
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

impl IntoElement for TextGlyphInset {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// G24 golden layout tests: decode → headless render → assert exact bounds.
#[cfg(test)]
mod headless_tests;

/// RFC 0002 async injection tests: producer → post → drain pump → dispatch,
/// observed through the `test-dispatch-stub` recorder (needs the feature).
#[cfg(all(test, feature = "test-dispatch-stub"))]
mod async_inject_tests;

/// Scroll position feedback tests (issue #89): paint-phase edge detection →
/// `EVENT_SCROLL` dispatch → pull ABI, observed through the same recorder.
#[cfg(all(test, feature = "test-dispatch-stub"))]
mod scroll_feedback_tests;

/// Text-input state-machine and pull-ABI tests (RFC 0003, issue #88). The
/// entity/IME wiring needs a windowed context, but the boundary arithmetic and
/// the mirror-backed C exports are plain logic — fixed here without gpui.
#[cfg(test)]
mod text_input_tests {
    use super::*;

    fn reset_input_statics() {
        *INPUT_MIRROR.lock().unwrap_or_else(|e| e.into_inner()) = None;
        INPUT_SET_TEXT_QUEUE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *INPUT_DIRTY.lock().unwrap_or_else(|e| e.into_inner()) = false;
    }

    #[::core::prelude::v1::test]
    fn utf16_offsets_round_trip_across_surrogates() {
        // "aあ🎉b": 'a'=1/1 (u8/u16), 'あ'=3/1, '🎉'=4/2 (surrogate pair), 'b'=1/1.
        let s = "aあ🎉b";
        for (u8_off, u16_off) in [(0, 0), (1, 1), (4, 2), (8, 4), (9, 5)] {
            assert_eq!(offset_to_utf16(s, u8_off), u16_off, "to_utf16({u8_off})");
            assert_eq!(offset_from_utf16(s, u16_off), u8_off, "from_utf16({u16_off})");
        }
        // Clamped past the end.
        assert_eq!(offset_to_utf16(s, 100), 5);
        assert_eq!(offset_from_utf16(s, 100), 9);
    }

    #[::core::prelude::v1::test]
    fn replace_commits_text_and_clears_the_mark() {
        let mut content = String::from("こんにちは");
        let mut sel = 15..15; // caret at end (5 chars × 3 bytes)
        let mut marked = Some(6..15); // pretend にちは is preedit
        input_apply_replace(&mut content, &mut sel, &mut marked, 6..15, "日は");
        assert_eq!(content, "こん日は");
        assert_eq!(sel, 12..12); // 6 + "日は".len()
        assert_eq!(marked, None);
    }

    #[::core::prelude::v1::test]
    fn replace_and_mark_tracks_the_preedit_span() {
        let mut content = String::new();
        let mut sel = 0..0;
        let mut marked = None;
        // Type "に" via IME: preedit "に" appears, selected at its end.
        input_apply_replace_and_mark(&mut content, &mut sel, &mut marked, 0..0, "に", None);
        assert_eq!(content, "に");
        assert_eq!(marked, Some(0..3));
        assert_eq!(sel, 3..3);
        // Preedit grows to "にほ" (IME replaces the whole marked span).
        input_apply_replace_and_mark(&mut content, &mut sel, &mut marked, 0..3, "にほ", None);
        assert_eq!(content, "にほ");
        assert_eq!(marked, Some(0..6));
        // Candidate selection swaps in "日本" with an explicit inner selection.
        input_apply_replace_and_mark(
            &mut content,
            &mut sel,
            &mut marked,
            0..6,
            "日本",
            Some(0..6),
        );
        assert_eq!(content, "日本");
        assert_eq!(marked, Some(0..6));
        assert_eq!(sel, 0..6);
        // Commit clears the mark.
        input_apply_replace(&mut content, &mut sel, &mut marked, 0..6, "日本");
        assert_eq!(content, "日本");
        assert_eq!(marked, None);
    }

    #[::core::prelude::v1::test]
    fn empty_preedit_unmarks() {
        let mut content = String::from("xにほ");
        let mut sel = 7..7;
        let mut marked = Some(1..7);
        // IME cancel: replace the marked span with "".
        input_apply_replace_and_mark(&mut content, &mut sel, &mut marked, 1..7, "", None);
        assert_eq!(content, "x");
        assert_eq!(marked, None);
        assert_eq!(sel, 1..1);
    }

    #[::core::prelude::v1::test]
    fn pull_abi_reads_the_mirror() {
        let _lock = INPUT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_input_statics();
        assert_eq!(gpui_input_text_len(0, 7), GPUI_STATUS_INVALID_HANDLE);
        mirror_update(0, 7, "héllo", false);
        assert_eq!(gpui_input_text_len(0, 7), 6); // é is 2 bytes
        let mut buf = [0u8; 16];
        let n = gpui_input_copy_text(0, 7, buf.as_mut_ptr(), buf.len() as i32);
        assert_eq!(n, 6);
        assert_eq!(&buf[..6], "héllo".as_bytes());
        // Truncating copy still reports bytes written.
        let n = gpui_input_copy_text(0, 7, buf.as_mut_ptr(), 2);
        assert_eq!(n, 2);
        // Bad args.
        assert_eq!(
            gpui_input_copy_text(0, 7, std::ptr::null_mut(), 4),
            GPUI_STATUS_INVALID_HANDLE
        );
        assert_eq!(
            gpui_input_copy_text(0, 7, buf.as_mut_ptr(), -1),
            GPUI_STATUS_INVALID_HANDLE
        );
    }

    #[::core::prelude::v1::test]
    fn set_text_queues_and_is_rejected_mid_composition() {
        let _lock = INPUT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_input_statics();
        // Unknown widget.
        assert_eq!(
            gpui_input_set_text(0, 9, b"x".as_ptr(), 1),
            GPUI_STATUS_INVALID_HANDLE
        );
        mirror_update(0, 9, "old", false);
        assert_eq!(gpui_input_set_text(0, 9, b"new".as_ptr(), 3), GPUI_STATUS_OK);
        // The mirror sees the write immediately; the entity write is queued
        // and the dirty flag arms the next dispatch site's notify.
        assert_eq!(mirror_get(0, 9).unwrap().text, "new");
        assert_eq!(
            INPUT_SET_TEXT_QUEUE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1
        );
        assert_eq!(take_input_dirty(), 1);
        assert_eq!(take_input_dirty(), 0); // consumed
        // Mid-composition writes are rejected (RFC 0003 §3.5).
        mirror_update(0, 9, "にほ", true);
        assert_eq!(
            gpui_input_set_text(0, 9, b"z".as_ptr(), 1),
            GPUI_STATUS_BUSY_COMPOSING
        );
        assert_eq!(mirror_get(0, 9).unwrap().text, "にほ");
        // Clearing with len 0 / null ptr is allowed when not composing.
        mirror_update(0, 9, "done", false);
        assert_eq!(gpui_input_set_text(0, 9, std::ptr::null(), 0), GPUI_STATUS_OK);
        assert_eq!(mirror_get(0, 9).unwrap().text, "");
    }

    #[::core::prelude::v1::test]
    fn text_input_decodes_as_a_leaf() {
        // GPUI magic + version, OP_TEXT_INPUT(input_id=5, "hint"), OP_SET_ROOT.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"GPUI");
        buf.extend_from_slice(&(BUFFER_VERSION as u32).to_le_bytes());
        buf.push(OP_TEXT_INPUT as u8);
        buf.extend_from_slice(&5i32.to_le_bytes());
        buf.extend_from_slice(&(4u32).to_le_bytes());
        buf.extend_from_slice(b"hint");
        buf.push(OP_SET_ROOT as u8);
        let _lock = TEST_VIEWS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        VIEWS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let status = build_tree_from_buffer(0, &buf);
        assert_eq!(status, GPUI_STATUS_OK);
        let guard = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
        match guard.first().and_then(|slot| slot.as_ref()) {
            Some(UiNode::TextInput {
                input_id,
                placeholder,
            }) => {
                assert_eq!(*input_id, 5);
                assert_eq!(placeholder, "hint");
            }
            _ => panic!("expected TextInput root"),
        }
    }

    #[::core::prelude::v1::test]
    fn text_input_content_is_not_in_the_debug_dump() {
        let mut out = Vec::new();
        collect_text_contents(
            &UiNode::TextInput {
                input_id: 1,
                placeholder: "p".into(),
            },
            &mut out,
        );
        assert!(out.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    fn clear_state() {
        VIEWS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    struct TestReset;

    impl Drop for TestReset {
        fn drop(&mut self) {
            clear_state();
        }
    }

    fn with_test(f: impl FnOnce()) {
        let _lock = TEST_VIEWS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        clear_state();
        let _reset = TestReset;
        f();
    }

    /// Inspect the committed view trees.
    fn with_views<F, R>(f: F) -> R
    where
        F: FnOnce(&[Option<UiNode>]) -> R,
    {
        let guard = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    // --- Command buffer builder (mirrors the MoonBit encoder) --------------

    struct Buf(Vec<u8>);

    impl Buf {
        fn new() -> Self {
            let mut b = Buf(Vec::new());
            b.0.extend_from_slice(BUFFER_MAGIC);
            b.u32(BUFFER_VERSION as u32);
            b
        }

        fn u8(&mut self, v: u8) -> &mut Self {
            self.0.push(v);
            self
        }

        fn u32(&mut self, v: u32) -> &mut Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }

        fn i32(&mut self, v: i32) -> &mut Self {
            self.u32(v as u32)
        }

        fn f32(&mut self, v: f32) -> &mut Self {
            self.u32(v.to_bits())
        }

        fn op(&mut self, opcode: i32) -> &mut Self {
            self.u8(opcode as u8)
        }

        fn str(&mut self, s: &str) -> &mut Self {
            let bytes = s.as_bytes();
            self.u32(bytes.len() as u32);
            self.0.extend_from_slice(bytes);
            self
        }

        fn div(&mut self) -> &mut Self {
            self.op(OP_DIV)
        }

        fn text(&mut self, content: &str, r: u8, g: u8, b: u8, size: f32) -> &mut Self {
            self.op(OP_TEXT).str(content).u8(r).u8(g).u8(b).f32(size)
        }

        fn set_root(&mut self) -> &mut Self {
            self.op(OP_SET_ROOT)
        }

        fn add_child(&mut self) -> &mut Self {
            self.op(OP_ADD_CHILD)
        }

        fn build(&self, view: i32) -> i32 {
            gpui_build_tree(view, self.0.as_ptr(), self.0.len() as i32)
        }
    }

    // --- Happy path --------------------------------------------------------

    #[::core::prelude::v1::test]
    fn builds_and_commits_a_full_tree() {
        with_test(|| {
            let mut b = Buf::new();
            // root: bg(1,2,3), flex col, center, gap 7, rounded 8, padding 5,
            // border 2 (9,9,9), key "root"
            b.div()
                .op(OP_SET_BG)
                .u8(1)
                .u8(2)
                .u8(3)
                .op(OP_SET_FLEX)
                .u8(1)
                .op(OP_SET_CENTER)
                .op(OP_SET_GAP)
                .f32(7.0)
                .op(OP_SET_ROUNDED)
                .f32(8.0)
                .op(OP_SET_PADDING)
                .f32(5.0)
                .op(OP_SET_BORDER)
                .f32(2.0)
                .u8(9)
                .u8(9)
                .u8(9)
                .op(OP_SET_SIZE)
                .f32(100.0)
                .f32(50.0)
                .op(OP_SET_KEY)
                .str("root");
            // child button: clickable, keyed
            b.div()
                .op(OP_SET_ON_CLICK)
                .i32(9)
                .op(OP_SET_KEY)
                .str("btn");
            b.add_child();
            // text child (non-ASCII + embedded NUL)
            b.text("A\0あ", 4, 5, 6, 14.0);
            b.add_child();
            b.set_root();

            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                let Some(UiNode::Div {
                    width,
                    height,
                    bg: Some((1, 2, 3)),
                    flex: true,
                    flex_col: true,
                    center: true,
                    gap,
                    rounded,
                    padding,
                    border_width,
                    border_color: Some((9, 9, 9)),
                    on_click: None,
                    key: Some(root_key),
                    children,
                    ..
                }) = &views[0]
                else {
                    panic!("root div mismatch");
                };
                assert_eq!(*width, 100.0);
                assert_eq!(*height, 50.0);
                assert_eq!(*gap, 7.0);
                assert_eq!(*rounded, 8.0);
                assert_eq!(*padding, 5.0);
                assert_eq!(*border_width, 2.0);
                assert_eq!(root_key, "root");
                assert_eq!(children.len(), 2);
                assert!(matches!(
                    &children[0],
                    UiNode::Div {
                        on_click: Some(9),
                        key: Some(k),
                        ..
                    } if k == "btn"
                ));
                assert!(matches!(
                    &children[1],
                    UiNode::Text {
                        content,
                        color: (4, 5, 6),
                        size,
                        ..
                    } if content == "A\0あ" && *size == 14.0
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn commit_replaces_previous_tree() {
        with_test(|| {
            let mut first = Buf::new();
            first.div().op(OP_SET_BG).u8(1).u8(2).u8(3).set_root();
            assert_eq!(first.build(0), GPUI_STATUS_OK);

            let mut second = Buf::new();
            second.div().op(OP_SET_BG).u8(4).u8(5).u8(6).set_root();
            assert_eq!(second.build(0), GPUI_STATUS_OK);

            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { bg: Some((4, 5, 6)), .. })
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn commits_to_distinct_views() {
        with_test(|| {
            let mut v0 = Buf::new();
            v0.div().op(OP_SET_BG).u8(1).u8(0).u8(0).set_root();
            assert_eq!(v0.build(0), GPUI_STATUS_OK);

            let mut v1 = Buf::new();
            v1.div().op(OP_SET_BG).u8(0).u8(1).u8(0).set_root();
            assert_eq!(v1.build(1), GPUI_STATUS_OK);

            with_views(|views| {
                assert!(matches!(&views[0], Some(UiNode::Div { bg: Some((1, 0, 0)), .. })));
                assert!(matches!(&views[1], Some(UiNode::Div { bg: Some((0, 1, 0)), .. })));
            });
        });
    }

    // --- Header / framing validation ---------------------------------------

    #[::core::prelude::v1::test]
    fn rejects_bad_magic_and_version() {
        with_test(|| {
            let mut bad_magic = Buf::new();
            bad_magic.0[0] = b'X';
            bad_magic.div().set_root();
            assert_eq!(bad_magic.build(0), GPUI_STATUS_BAD_BUFFER_VERSION);

            let mut bad_version = Buf::new();
            bad_version.0[4] = 0xFF; // corrupt the version u32
            bad_version.div().set_root();
            assert_eq!(bad_version.build(0), GPUI_STATUS_BAD_BUFFER_VERSION);
        });
    }

    #[::core::prelude::v1::test]
    fn rejects_null_pointer_and_negative_length() {
        with_test(|| {
            assert_eq!(
                gpui_build_tree(0, std::ptr::null(), 8),
                GPUI_STATUS_TRUNCATED_BUFFER
            );
            let b = Buf::new();
            assert_eq!(
                gpui_build_tree(0, b.0.as_ptr(), -1),
                GPUI_STATUS_TRUNCATED_BUFFER
            );
        });
    }

    #[::core::prelude::v1::test]
    fn rejects_truncated_operand() {
        with_test(|| {
            // OP_SET_BG needs 3 bytes; supply only 2 then end the buffer.
            let mut b = Buf::new();
            b.div().op(OP_SET_BG).u8(1).u8(2);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);

            // OP_TEXT with a declared length longer than the remaining bytes.
            let mut t = Buf::new();
            t.op(OP_TEXT).u32(999).u8(b'a').set_root();
            assert_eq!(t.build(0), GPUI_STATUS_TRUNCATED_BUFFER);

            // OP_SET_PADDING needs an f32; end the buffer right after the opcode.
            let mut p = Buf::new();
            p.div().op(OP_SET_PADDING);
            assert_eq!(p.build(0), GPUI_STATUS_TRUNCATED_BUFFER);

            // OP_SET_BORDER needs f32 + 3 bytes; supply width and only 2 bytes.
            let mut br = Buf::new();
            br.div().op(OP_SET_BORDER).f32(1.0).u8(1).u8(2);
            assert_eq!(br.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn rejects_huge_string_length_without_panic() {
        with_test(|| {
            // A declared length near the address-space ceiling must report
            // truncation, not overflow the cursor's bounds check.
            let mut t = Buf::new();
            t.op(OP_TEXT).u32(u32::MAX).u8(b'a').set_root();
            assert_eq!(t.build(0), GPUI_STATUS_TRUNCATED_BUFFER);

            let mut k = Buf::new();
            k.div().op(OP_SET_KEY).u32(0x7FFF_FFFF);
            assert_eq!(k.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn invalid_utf8_text_is_lossy_not_fatal() {
        with_test(|| {
            // The boundary replaces invalid UTF-8 with U+FFFD rather than
            // rejecting: a malformed payload still commits, never panics.
            let mut b = Buf::new();
            b.op(OP_TEXT)
                .u32(2)
                .u8(0xFF)
                .u8(0xFE)
                .u8(1)
                .u8(2)
                .u8(3)
                .f32(10.0)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Text {
                        content,
                        color: (1, 2, 3),
                        size,
                        ..
                    }) if content == "\u{FFFD}\u{FFFD}" && *size == 10.0
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn rejects_unknown_opcode() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().u8(0xFE).set_root();
            assert_eq!(b.build(0), GPUI_STATUS_UNKNOWN_OPCODE);
        });
    }

    #[::core::prelude::v1::test]
    fn rejects_negative_view() {
        with_test(|| {
            let b = Buf::new();
            assert_eq!(b.build(-1), GPUI_STATUS_INVALID_HANDLE);
        });
    }

    #[::core::prelude::v1::test]
    fn run_window_rejects_negative_view() {
        assert_eq!(
            gpui_run_window(-1, 10.0, 10.0),
            GPUI_STATUS_INVALID_HANDLE
        );
    }

    // --- Stack / handle validation -----------------------------------------

    #[::core::prelude::v1::test]
    fn setter_on_empty_stack_fails() {
        with_test(|| {
            let mut b = Buf::new();
            b.op(OP_SET_CENTER).set_root();
            assert_eq!(b.build(0), GPUI_STATUS_INVALID_HANDLE);
        });
    }

    #[::core::prelude::v1::test]
    fn setter_on_text_top_fails() {
        with_test(|| {
            let mut b = Buf::new();
            b.text("x", 0, 0, 0, 12.0).op(OP_SET_CENTER);
            assert_eq!(b.build(0), GPUI_STATUS_WRONG_NODE_KIND);
        });
    }

    #[::core::prelude::v1::test]
    fn padding_and_border_apply_to_div() {
        with_test(|| {
            let mut b = Buf::new();
            b.div()
                .op(OP_SET_PADDING)
                .f32(12.0)
                .op(OP_SET_BORDER)
                .f32(3.0)
                .u8(10)
                .u8(20)
                .u8(30)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        padding,
                        border_width,
                        border_color: Some((10, 20, 30)),
                        ..
                    }) if *padding == 12.0 && *border_width == 3.0
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn padding_and_border_on_text_top_fail() {
        with_test(|| {
            let mut p = Buf::new();
            p.text("x", 0, 0, 0, 12.0).op(OP_SET_PADDING).f32(4.0);
            assert_eq!(p.build(0), GPUI_STATUS_WRONG_NODE_KIND);

            let mut br = Buf::new();
            br.text("x", 0, 0, 0, 12.0)
                .op(OP_SET_BORDER)
                .f32(1.0)
                .u8(0)
                .u8(0)
                .u8(0);
            assert_eq!(br.build(0), GPUI_STATUS_WRONG_NODE_KIND);
        });
    }

    #[::core::prelude::v1::test]
    fn add_child_underflow_fails() {
        with_test(|| {
            // One node on the stack: add_child needs two.
            let mut one = Buf::new();
            one.div().add_child();
            assert_eq!(one.build(0), GPUI_STATUS_INVALID_HANDLE);

            // Empty stack.
            let mut zero = Buf::new();
            zero.add_child();
            assert_eq!(zero.build(0), GPUI_STATUS_INVALID_HANDLE);
        });
    }

    #[::core::prelude::v1::test]
    fn add_child_to_text_parent_fails() {
        with_test(|| {
            let mut b = Buf::new();
            b.text("p", 0, 0, 0, 12.0).div().add_child();
            assert_eq!(b.build(0), GPUI_STATUS_WRONG_NODE_KIND);
        });
    }

    #[::core::prelude::v1::test]
    fn set_root_on_empty_stack_fails() {
        with_test(|| {
            // OP_SET_ROOT with nothing pushed: the stack underflows.
            let mut b = Buf::new();
            b.set_root();
            assert_eq!(b.build(0), GPUI_STATUS_INVALID_HANDLE);
        });
    }

    #[::core::prelude::v1::test]
    fn build_without_root_fails() {
        with_test(|| {
            let mut b = Buf::new();
            b.div(); // created but never set_root
            assert_eq!(b.build(0), GPUI_STATUS_NO_ROOT);
        });
    }

    #[::core::prelude::v1::test]
    fn nested_attachment_commits() {
        with_test(|| {
            // grandparent absorbs parent absorbs child; root = grandparent.
            let mut b = Buf::new();
            b.div(); // grandparent (0)
            b.div(); // parent (1)
            b.div(); // child (2)
            b.add_child(); // parent(1) absorbs child(2); stack [0, 1]
            b.add_child(); // grandparent(0) absorbs parent(1); stack [0]
            b.set_root(); // root = grandparent(0)
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { children, .. })
                        if matches!(children.as_slice(),
                            [UiNode::Div { children: inner, .. }]
                                if matches!(inner.as_slice(), [UiNode::Div { .. }]))
                ));
            });
        });
    }

    // --- Move / forest semantics (issue #8) --------------------------------

    #[::core::prelude::v1::test]
    fn add_child_moves_not_copies() {
        with_test(|| {
            // Two children attached in order; each appears exactly once under
            // the parent, in attachment order.
            let mut b = Buf::new();
            b.div(); // parent (0)
            b.div().op(OP_SET_BG).u8(1).u8(0).u8(0); // child A (1)
            b.add_child();
            b.div().op(OP_SET_BG).u8(0).u8(1).u8(0); // child B (2)
            b.add_child();
            b.set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { children, .. })
                        if matches!(
                            children.as_slice(),
                            [
                                UiNode::Div {
                                    bg: Some((1, 0, 0)),
                                    children: a,
                                    ..
                                },
                                UiNode::Div {
                                    bg: Some((0, 1, 0)),
                                    children: c,
                                    ..
                                },
                            ] if a.is_empty() && c.is_empty()
                        )
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn subtree_moves_intact() {
        with_test(|| {
            // Grandchild attached to child, then child moved into root: the
            // whole subtree relocates with its contents, nothing duplicated.
            let mut b = Buf::new();
            b.div(); // root (0)
            b.div(); // child (1)
            b.div().op(OP_SET_BG).u8(7).u8(7).u8(7); // grandchild (2)
            b.add_child(); // 2 into 1
            b.add_child(); // 1 into 0
            b.set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { children: root_kids, .. })
                        if matches!(
                            root_kids.as_slice(),
                            [UiNode::Div { children: inner, .. }]
                                if matches!(
                                    inner.as_slice(),
                                    [UiNode::Div {
                                        bg: Some((7, 7, 7)),
                                        children: leaf,
                                        ..
                                    }] if leaf.is_empty()
                                )
                        )
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn unattached_nodes_are_dropped_from_commit() {
        with_test(|| {
            // Forest model: only the designated root is committed; a node
            // never attached nor rooted (here handle 0) is silently discarded.
            let mut b = Buf::new();
            b.div().op(OP_SET_BG).u8(9).u8(9).u8(9); // orphan (0)
            b.div().op(OP_SET_BG).u8(1).u8(2).u8(3); // root (1)
            b.set_root(); // pops 1
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        bg: Some((1, 2, 3)),
                        children,
                        ..
                    }) if children.is_empty()
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn last_set_root_wins() {
        with_test(|| {
            // Two OP_SET_ROOT in one buffer: the last designation commits.
            let mut b = Buf::new();
            b.div().op(OP_SET_BG).u8(1).u8(0).u8(0);
            b.set_root();
            b.div().op(OP_SET_BG).u8(0).u8(1).u8(0);
            b.set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { bg: Some((0, 1, 0)), .. })
                ));
            });
        });
    }

    // --- Key semantics (issue #9, now via the buffer) ----------------------

    #[::core::prelude::v1::test]
    fn commit_rejects_duplicate_keys_in_tree() {
        with_test(|| {
            let mut b = Buf::new();
            b.div(); // root
            b.div().op(OP_SET_KEY).str("same");
            b.add_child();
            b.div().op(OP_SET_KEY).str("same");
            b.add_child();
            b.set_root();
            assert_eq!(b.build(0), GPUI_STATUS_DUPLICATE_KEY);
            // Failed build leaves the previous (empty) committed tree untouched.
            with_views(|views| assert!(views.is_empty() || views[0].is_none()));
        });
    }

    #[::core::prelude::v1::test]
    fn commit_allows_distinct_keys() {
        with_test(|| {
            let mut b = Buf::new();
            b.div();
            b.div().op(OP_SET_KEY).str("a");
            b.add_child();
            b.div().op(OP_SET_KEY).str("b");
            b.add_child();
            b.set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
        });
    }

    #[::core::prelude::v1::test]
    fn commit_allows_duplicate_click_ids() {
        with_test(|| {
            // click_id is action routing, not identity: duplicates are allowed.
            let mut b = Buf::new();
            b.div();
            b.div().op(OP_SET_ON_CLICK).i32(7);
            b.add_child();
            b.div().op(OP_SET_ON_CLICK).i32(7);
            b.add_child();
            b.set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
        });
    }

    // --- Notification gate -------------------------------------------------

    #[::core::prelude::v1::test]
    fn notification_gate_accepts_only_changed_one() {
        let calls = std::cell::Cell::new(0);
        notify_if_changed(0, || calls.set(calls.get() + 1));
        notify_if_changed(-1, || calls.set(calls.get() + 1));
        notify_if_changed(2, || calls.set(calls.get() + 1));
        assert_eq!(calls.get(), 0);
        notify_if_changed(1, || calls.set(calls.get() + 1));
        assert_eq!(calls.get(), 1);
    }

    // --- Cross-boundary ABI drift guard ------------------------------------

    /// Cross-boundary drift guard (issue #8: EVENT_*/EV_* compatibility).
    ///
    /// The integers Rust ships as the callback `kind` and modifier bits must be
    /// the exact integers MoonBit decodes. Both sides are generated from
    /// `gpui-sys/abi.toml`, but generation is independent (build.rs for Rust,
    /// build.sh/awk for MoonBit) and only *warns* on drift — nothing fails. This
    /// pins the contract headlessly: every compiled Rust constant must equal
    /// `abi.toml`, and `abi.toml` must equal the generated MoonBit file, so a
    /// stale or hand-edited generated file on either side fails here rather than
    /// at runtime.
    #[::core::prelude::v1::test]
    fn abi_constants_match_across_boundary() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let root = std::path::Path::new(&manifest);

        // abi.toml is the single source of truth: [section] headers + key = int.
        let abi_toml = std::fs::read_to_string(root.join("abi.toml")).expect("read abi.toml");
        let mut expected: std::collections::BTreeMap<String, i32> =
            std::collections::BTreeMap::new();
        let mut in_callback = false;
        for raw in abi_toml.lines() {
            let line = raw.split('#').next().unwrap().trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                in_callback = name.trim() == "callback";
                continue;
            }
            if in_callback {
                continue; // callback signature is string-valued, not a numeric constant
            }
            let (key, value) = line.split_once('=').expect("abi.toml key = value");
            let key = key.trim();
            let Ok(value) = value.trim().parse::<i32>() else {
                continue; // skip non-integer values defensively
            };
            let key = if key == "abi_version" { "ABI_VERSION" } else { key };
            expected.insert(key.to_string(), value);
        }
        assert!(
            !expected.is_empty(),
            "abi.toml yielded no numeric constants; parser drift?"
        );

        // 1) Compiled Rust constants must equal the source of truth.
        let rust_constants = [
            ("ABI_VERSION", ABI_VERSION),
            ("EVENT_CLICK", EVENT_CLICK),
            ("EVENT_KEY", EVENT_KEY),
            ("EVENT_TEXT", EVENT_TEXT),
            ("EVENT_NAMED_KEY", EVENT_NAMED_KEY),
            ("EVENT_ASYNC", EVENT_ASYNC),
            ("EVENT_INPUT_CHANGED", EVENT_INPUT_CHANGED),
            ("EVENT_INPUT_SUBMIT", EVENT_INPUT_SUBMIT),
            ("EVENT_SCROLL", EVENT_SCROLL),
            ("OP_SET_SCROLL_ID", OP_SET_SCROLL_ID),
            ("MOD_CTRL", MOD_CTRL),
            ("MOD_ALT", MOD_ALT),
            ("MOD_SHIFT", MOD_SHIFT),
            ("MOD_PLATFORM", MOD_PLATFORM),
            ("MOD_FUNCTION", MOD_FUNCTION),
            ("KEY_ENTER", KEY_ENTER),
            ("KEY_ESCAPE", KEY_ESCAPE),
            ("KEY_UP", KEY_UP),
            ("KEY_DOWN", KEY_DOWN),
            ("KEY_LEFT", KEY_LEFT),
            ("KEY_RIGHT", KEY_RIGHT),
            ("KEY_TAB", KEY_TAB),
            ("KEY_BACKSPACE", KEY_BACKSPACE),
            ("KEY_DELETE", KEY_DELETE),
            ("KEY_HOME", KEY_HOME),
            ("KEY_END", KEY_END),
            ("KEY_PAGEUP", KEY_PAGEUP),
            ("KEY_PAGEDOWN", KEY_PAGEDOWN),
            ("OP_DIV", OP_DIV),
            ("OP_TEXT", OP_TEXT),
            ("OP_SET_SIZE", OP_SET_SIZE),
            ("OP_SET_BG", OP_SET_BG),
            ("OP_SET_FLEX", OP_SET_FLEX),
            ("OP_SET_CENTER", OP_SET_CENTER),
            ("OP_SET_GAP", OP_SET_GAP),
            ("OP_SET_ROUNDED", OP_SET_ROUNDED),
            ("OP_SET_ON_CLICK", OP_SET_ON_CLICK),
            ("OP_SET_KEY", OP_SET_KEY),
            ("OP_SET_PADDING", OP_SET_PADDING),
            ("OP_SET_BORDER", OP_SET_BORDER),
            ("OP_SET_BG_COLOR", OP_SET_BG_COLOR),
            ("OP_SET_MARGIN", OP_SET_MARGIN),
            ("OP_SET_MIN_SIZE", OP_SET_MIN_SIZE),
            ("OP_SET_MAX_SIZE", OP_SET_MAX_SIZE),
            ("OP_SET_FLEX_ITEM", OP_SET_FLEX_ITEM),
            ("OP_SET_ALIGN", OP_SET_ALIGN),
            ("OP_SET_OVERFLOW", OP_SET_OVERFLOW),
            ("OP_SET_OPACITY", OP_SET_OPACITY),
            ("OP_SET_SHADOW", OP_SET_SHADOW),
            ("OP_SET_CURSOR", OP_SET_CURSOR),
            ("OP_SET_POSITION", OP_SET_POSITION),
            ("OP_SET_INSET", OP_SET_INSET),
            ("OP_SET_PADDING_SIDES", OP_SET_PADDING_SIDES),
            ("ALIGN_DEFAULT", ALIGN_DEFAULT),
            ("ALIGN_START", ALIGN_START),
            ("ALIGN_CENTER", ALIGN_CENTER),
            ("ALIGN_END", ALIGN_END),
            ("ALIGN_STRETCH", ALIGN_STRETCH),
            ("JUSTIFY_DEFAULT", JUSTIFY_DEFAULT),
            ("JUSTIFY_START", JUSTIFY_START),
            ("JUSTIFY_CENTER", JUSTIFY_CENTER),
            ("JUSTIFY_END", JUSTIFY_END),
            ("JUSTIFY_SPACE_BETWEEN", JUSTIFY_SPACE_BETWEEN),
            ("JUSTIFY_SPACE_AROUND", JUSTIFY_SPACE_AROUND),
            ("OVERFLOW_VISIBLE", OVERFLOW_VISIBLE),
            ("OVERFLOW_HIDDEN", OVERFLOW_HIDDEN),
            ("OVERFLOW_SCROLL", OVERFLOW_SCROLL),
            ("CURSOR_ARROW", CURSOR_ARROW),
            ("CURSOR_POINTER", CURSOR_POINTER),
            ("CURSOR_TEXT", CURSOR_TEXT),
            ("CURSOR_CROSSHAIR", CURSOR_CROSSHAIR),
            ("CURSOR_GRAB", CURSOR_GRAB),
            ("CURSOR_GRABBING", CURSOR_GRABBING),
            ("CURSOR_NOT_ALLOWED", CURSOR_NOT_ALLOWED),
            ("CURSOR_EW_RESIZE", CURSOR_EW_RESIZE),
            ("CURSOR_NS_RESIZE", CURSOR_NS_RESIZE),
            ("CURSOR_COL_RESIZE", CURSOR_COL_RESIZE),
            ("CURSOR_ROW_RESIZE", CURSOR_ROW_RESIZE),
            ("CURSOR_NONE", CURSOR_NONE),
            ("POSITION_RELATIVE", POSITION_RELATIVE),
            ("POSITION_ABSOLUTE", POSITION_ABSOLUTE),
            ("OP_SET_TEXT_SIZE", OP_SET_TEXT_SIZE),
            ("OP_SET_TEXT_COLOR", OP_SET_TEXT_COLOR),
            ("OP_SET_FONT_WEIGHT", OP_SET_FONT_WEIGHT),
            ("OP_SET_LINE_HEIGHT", OP_SET_LINE_HEIGHT),
            ("OP_SET_TEXT_ALIGN", OP_SET_TEXT_ALIGN),
            ("OP_SET_WHITESPACE", OP_SET_WHITESPACE),
            ("OP_SET_FONT_FAMILY", OP_SET_FONT_FAMILY),
            ("OP_SET_FOCUSABLE", OP_SET_FOCUSABLE),
            ("OP_SET_TAB_INDEX", OP_SET_TAB_INDEX),
            ("OP_SET_TAB_STOP", OP_SET_TAB_STOP),
            ("OP_TEXT_INPUT", OP_TEXT_INPUT),
            ("OP_TEXT_RUN", OP_TEXT_RUN),
            ("RUN_STYLE_COLOR", RUN_STYLE_COLOR),
            ("RUN_STYLE_WEIGHT", RUN_STYLE_WEIGHT),
            ("RUN_STYLE_ITALIC", RUN_STYLE_ITALIC),
            ("RUN_STYLE_UNDERLINE", RUN_STYLE_UNDERLINE),
            ("RUN_STYLE_STRIKETHROUGH", RUN_STYLE_STRIKETHROUGH),
            ("RUN_STYLE_BACKGROUND", RUN_STYLE_BACKGROUND),
            ("TEXT_ALIGN_DEFAULT", TEXT_ALIGN_DEFAULT),
            ("TEXT_ALIGN_LEFT", TEXT_ALIGN_LEFT),
            ("TEXT_ALIGN_CENTER", TEXT_ALIGN_CENTER),
            ("TEXT_ALIGN_RIGHT", TEXT_ALIGN_RIGHT),
            ("TEXT_ALIGN_JUSTIFY", TEXT_ALIGN_JUSTIFY),
            ("WHITESPACE_DEFAULT", WHITESPACE_DEFAULT),
            ("WHITESPACE_NORMAL", WHITESPACE_NORMAL),
            ("WHITESPACE_NOWRAP", WHITESPACE_NOWRAP),
            ("WHITESPACE_PRE", WHITESPACE_PRE),
            ("WHITESPACE_PRE_WRAP", WHITESPACE_PRE_WRAP),
            ("OP_ADD_CHILD", OP_ADD_CHILD),
            ("OP_SET_ROOT", OP_SET_ROOT),
            ("BUFFER_VERSION", BUFFER_VERSION),
        ];
        for (name, compiled) in rust_constants {
            assert_eq!(
                expected.get(name).copied(),
                Some(compiled),
                "Rust {name} drifted from abi.toml (regenerate src/abi_constants.rs via build.rs)"
            );
        }
        for name in expected.keys() {
            assert!(
                rust_constants.iter().any(|(n, _)| n == name),
                "abi.toml constant {name} has no compiled Rust counterpart"
            );
        }

        // 2) The generated MoonBit file must carry the same values. Whitespace is
        //    stripped so the check survives `moon fmt` spacing changes.
        let mb = std::fs::read_to_string(root.join("../moonbit-bindings/abi_constants.mbt"))
            .expect("read moonbit-bindings/abi_constants.mbt (run build.sh to regenerate)");
        let mb_compact: String = mb.chars().filter(|c| !c.is_whitespace()).collect();
        for (name, value) in &expected {
            let needle = format!("pubconst{name}:Int={value}");
            assert!(
                mb_compact.contains(&needle),
                "MoonBit abi_constants.mbt missing `pub const {name} : Int = {value}` — regenerate via build.sh"
            );
        }
    }

    #[::core::prelude::v1::test]
    fn debug_dump_text_round_trips() {
        with_test(|| {
            // div { text("A\0あ"), text("🎉") }
            let mut b = Buf::new();
            b.div()
                .text("A\0あ", 255, 255, 255, 16.0)
                .add_child()
                .text("🎉", 255, 255, 255, 16.0)
                .add_child()
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);

            // Expected: len u32 LE + utf8 for each text node, DFS pre-order.
            let mut expected = Vec::new();
            for s in ["A\0あ", "🎉"] {
                let bytes = s.as_bytes();
                expected.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                expected.extend_from_slice(bytes);
            }

            let mut buf = vec![0u8; 256];
            let n = gpui_debug_dump_text(0, buf.as_mut_ptr(), buf.len() as i32);
            assert_eq!(n, expected.len() as i32);
            assert_eq!(&buf[..n as usize], &expected[..]);
        });
    }

    #[::core::prelude::v1::test]
    fn debug_dump_text_rejects_bad_args() {
        with_test(|| {
            let mut buf = [0u8; 8];
            assert_eq!(
                gpui_debug_dump_text(-1, buf.as_mut_ptr(), 8),
                GPUI_STATUS_INVALID_HANDLE
            );
            assert_eq!(
                gpui_debug_dump_text(0, std::ptr::null_mut(), 8),
                GPUI_STATUS_INVALID_HANDLE
            );
            assert_eq!(
                gpui_debug_dump_text(0, buf.as_mut_ptr(), -1),
                GPUI_STATUS_INVALID_HANDLE
            );
            // No tree committed yet.
            assert_eq!(
                gpui_debug_dump_text(0, buf.as_mut_ptr(), 8),
                GPUI_STATUS_INVALID_HANDLE
            );
        });
    }

    #[::core::prelude::v1::test]
    fn named_key_id_maps_known_keys() {
        assert_eq!(named_key_id("enter"), Some(KEY_ENTER));
        assert_eq!(named_key_id("escape"), Some(KEY_ESCAPE));
        assert_eq!(named_key_id("up"), Some(KEY_UP));
        assert_eq!(named_key_id("down"), Some(KEY_DOWN));
        assert_eq!(named_key_id("left"), Some(KEY_LEFT));
        assert_eq!(named_key_id("right"), Some(KEY_RIGHT));
        assert_eq!(named_key_id("tab"), Some(KEY_TAB));
        assert_eq!(named_key_id("backspace"), Some(KEY_BACKSPACE));
        assert_eq!(named_key_id("delete"), Some(KEY_DELETE));
        assert_eq!(named_key_id("home"), Some(KEY_HOME));
        assert_eq!(named_key_id("end"), Some(KEY_END));
        assert_eq!(named_key_id("pageup"), Some(KEY_PAGEUP));
        assert_eq!(named_key_id("pagedown"), Some(KEY_PAGEDOWN));
    }

    #[::core::prelude::v1::test]
    fn named_key_id_rejects_unknown() {
        assert_eq!(named_key_id("k"), None);
        assert_eq!(named_key_id("space"), None);
        assert_eq!(named_key_id(""), None);
        assert_eq!(named_key_id("f13"), None);
    }

    #[::core::prelude::v1::test]
    fn ime_owned_when_idle_covers_printable_keys_but_not_enter() {
        // Printable keys are IME-owned while an input source is selected.
        assert!(ime_owned_when_idle("a", Some("a")));
        assert!(ime_owned_when_idle("space", Some(" ")));
        // Enter must fall through to the named-key path: an idle IME answers
        // Return with insertNewline:, which gpui drops, losing the key.
        assert!(!ime_owned_when_idle("enter", Some("\n")));
        // Keys without a committed character never take the IME branch.
        assert!(!ime_owned_when_idle("backspace", None));
        assert!(!ime_owned_when_idle("up", None));
    }

    #[::core::prelude::v1::test]
    fn abi_probe_echoes_boundary_values() {
        // The MoonBit side of this check lives in cmd/roundtrip (cross-boundary);
        // here we pin the Rust half: the probe is a pure identity, including the
        // i32 extremes the round-trip sends.
        for v in [i32::MAX, i32::MIN, 0, -1, 42, -42] {
            assert_eq!(gpui_abi_probe(v), v);
        }
    }

    // --- G7 core layout/style + G9 color (issue #51) ----------------------

    #[::core::prelude::v1::test]
    fn set_bg_color_decodes_rgba() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_BG_COLOR).u8(1).u8(2).u8(3).u8(128).set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        bg_color: Some((1, 2, 3, 128)),
                        bg: None,
                        ..
                    })
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_bg_color_truncated_fails() {
        with_test(|| {
            // OP_SET_BG_COLOR needs 4 bytes; supply only 3.
            let mut b = Buf::new();
            b.div().op(OP_SET_BG_COLOR).u8(1).u8(2).u8(3);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_margin_decodes_four_sides() {
        with_test(|| {
            let mut b = Buf::new();
            b.div()
                .op(OP_SET_MARGIN)
                .i32(1)
                .i32(2)
                .i32(3)
                .i32(4)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        margin: Some(m),
                        ..
                    }) if *m == (1.0, 2.0, 3.0, 4.0)
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_margin_truncated_fails() {
        with_test(|| {
            // OP_SET_MARGIN needs 4 i32; supply only 3.
            let mut b = Buf::new();
            b.div().op(OP_SET_MARGIN).i32(1).i32(2).i32(3);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_padding_sides_decodes_four_sides() {
        with_test(|| {
            let mut b = Buf::new();
            b.div()
                .op(OP_SET_PADDING_SIDES)
                .i32(5)
                .i32(6)
                .i32(7)
                .i32(8)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        padding_sides: Some(p),
                        ..
                    }) if *p == (5.0, 6.0, 7.0, 8.0)
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_padding_sides_truncated_fails() {
        with_test(|| {
            // OP_SET_PADDING_SIDES needs 4 i32; supply only 3.
            let mut b = Buf::new();
            b.div().op(OP_SET_PADDING_SIDES).i32(1).i32(2).i32(3);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_flex_item_scales_milliunits() {
        with_test(|| {
            let mut b = Buf::new();
            // grow 1.5 (1500), shrink 0.5 (500), basis 100px.
            b.div()
                .op(OP_SET_FLEX_ITEM)
                .i32(1500)
                .i32(500)
                .i32(100)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        flex_item: Some(f),
                        ..
                    }) if *f == (1.5, 0.5, 100.0)
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_flex_item_truncated_fails() {
        with_test(|| {
            // OP_SET_FLEX_ITEM needs 3 i32; supply only 2.
            let mut b = Buf::new();
            b.div().op(OP_SET_FLEX_ITEM).i32(1000).i32(1000);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_opacity_scales_milliunits() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_OPACITY).i32(500).set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        opacity: Some(o),
                        ..
                    }) if (*o - 0.5).abs() < 1e-6
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_opacity_truncated_fails() {
        with_test(|| {
            // OP_SET_OPACITY needs an i32; end the buffer right after the opcode.
            let mut b = Buf::new();
            b.div().op(OP_SET_OPACITY);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_shadow_decodes_geometry_and_color() {
        with_test(|| {
            let mut b = Buf::new();
            b.div()
                .op(OP_SET_SHADOW)
                .i32(0)
                .i32(4)
                .i32(6)
                .i32(-1)
                .u8(0)
                .u8(0)
                .u8(0)
                .u8(64)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        shadow: Some(s),
                        ..
                    }) if s.x == 0.0
                        && s.y == 4.0
                        && s.blur == 6.0
                        && s.spread == -1.0
                        && s.color == (0, 0, 0, 64)
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_shadow_truncated_fails() {
        with_test(|| {
            // OP_SET_SHADOW needs 4 i32 + 4 bytes; supply geometry + 2 bytes.
            let mut b = Buf::new();
            b.div()
                .op(OP_SET_SHADOW)
                .i32(0)
                .i32(0)
                .i32(0)
                .i32(0)
                .u8(0)
                .u8(0);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_align_overflow_cursor_position_inset_decode() {
        with_test(|| {
            let mut b = Buf::new();
            b.div()
                .op(OP_SET_ALIGN)
                .i32(ALIGN_CENTER)
                .i32(JUSTIFY_SPACE_BETWEEN)
                .op(OP_SET_OVERFLOW)
                .i32(OVERFLOW_HIDDEN)
                .i32(OVERFLOW_SCROLL)
                .op(OP_SET_CURSOR)
                .i32(CURSOR_POINTER)
                .op(OP_SET_POSITION)
                .i32(POSITION_ABSOLUTE)
                .op(OP_SET_INSET)
                .i32(10)
                .i32(-1)
                .i32(20)
                .i32(-1)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        align: Some((a, j)),
                        overflow: Some((ox, oy)),
                        cursor: Some(c),
                        position: Some(p),
                        inset: Some(ins),
                        ..
                    }) if *a == ALIGN_CENTER
                        && *j == JUSTIFY_SPACE_BETWEEN
                        && *ox == OVERFLOW_HIDDEN
                        && *oy == OVERFLOW_SCROLL
                        && *c == CURSOR_POINTER
                        && *p == POSITION_ABSOLUTE
                        && *ins == (10.0, -1.0, 20.0, -1.0)
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_min_max_size_decode_with_auto_sentinel() {
        with_test(|| {
            let mut b = Buf::new();
            b.div()
                .op(OP_SET_MIN_SIZE)
                .i32(100)
                .i32(-1)
                .op(OP_SET_MAX_SIZE)
                .i32(-1)
                .i32(400)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        min_size: Some(mn),
                        max_size: Some(mx),
                        ..
                    }) if *mn == (100.0, -1.0) && *mx == (-1.0, 400.0)
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn new_setters_on_text_top_fail() {
        with_test(|| {
            // Every new setter routes through with_top_div, so a text top must
            // be rejected with WRONG_NODE_KIND rather than corrupting state.
            let cases: Vec<Box<dyn Fn(&mut Buf)>> = vec![
                Box::new(|b| { b.op(OP_SET_BG_COLOR).u8(0).u8(0).u8(0).u8(0); }),
                Box::new(|b| { b.op(OP_SET_MARGIN).i32(0).i32(0).i32(0).i32(0); }),
                Box::new(|b| { b.op(OP_SET_OPACITY).i32(1000); }),
                Box::new(|b| { b.op(OP_SET_CURSOR).i32(CURSOR_POINTER); }),
                Box::new(|b| { b.op(OP_SET_TEXT_SIZE).i32(14); }),
                Box::new(|b| { b.op(OP_SET_FONT_FAMILY).str("Arial"); }),
            ];
            for apply in cases {
                let mut b = Buf::new();
                b.text("x", 0, 0, 0, 12.0);
                apply(&mut b);
                assert_eq!(b.build(0), GPUI_STATUS_WRONG_NODE_KIND);
            }
        });
    }

    // --- G8 typography (issue #51) -----------------------------------------

    #[::core::prelude::v1::test]
    fn set_text_size_decodes_px() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_TEXT_SIZE).i32(18).set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { text_size: Some(s), .. }) if *s == 18.0
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_text_size_truncated_fails() {
        with_test(|| {
            // OP_SET_TEXT_SIZE needs an i32; end the buffer right after the opcode.
            let mut b = Buf::new();
            b.div().op(OP_SET_TEXT_SIZE);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_text_color_decodes_rgba() {
        with_test(|| {
            let mut b = Buf::new();
            b.div()
                .op(OP_SET_TEXT_COLOR)
                .u8(10)
                .u8(20)
                .u8(30)
                .u8(128)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        text_color: Some((10, 20, 30, 128)),
                        ..
                    })
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_text_color_truncated_fails() {
        with_test(|| {
            // OP_SET_TEXT_COLOR needs 4 bytes; supply only 3.
            let mut b = Buf::new();
            b.div().op(OP_SET_TEXT_COLOR).u8(1).u8(2).u8(3);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_font_weight_clamps_out_of_range() {
        with_test(|| {
            let mut b = Buf::new();
            // 50 clamps up to 100; 1000 clamps down to 900; 700 passes through.
            b.div()
                .op(OP_SET_FONT_WEIGHT)
                .i32(50)
                .op(OP_SET_FONT_WEIGHT)
                .i32(1000)
                .op(OP_SET_FONT_WEIGHT)
                .i32(700)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { font_weight: Some(700), .. })
                ));
            });
            // The clamp is observable on the first two writes too: rebuild a
            // tree that stops at each clamp boundary.
            let mut lo = Buf::new();
            lo.div().op(OP_SET_FONT_WEIGHT).i32(50).set_root();
            assert_eq!(lo.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { font_weight: Some(100), .. })
                ));
            });
            let mut hi = Buf::new();
            hi.div().op(OP_SET_FONT_WEIGHT).i32(1000).set_root();
            assert_eq!(hi.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { font_weight: Some(900), .. })
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_font_weight_truncated_fails() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_FONT_WEIGHT);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_line_height_scales_milliunits_and_negative_unsets() {
        with_test(|| {
            let mut b = Buf::new();
            // 1500 → 1.5px; a later negative operand unsets it again.
            b.div()
                .op(OP_SET_LINE_HEIGHT)
                .i32(1500)
                .op(OP_SET_LINE_HEIGHT)
                .i32(-1)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { line_height: None, .. })
                ));
            });
            let mut set = Buf::new();
            set.div().op(OP_SET_LINE_HEIGHT).i32(2250).set_root();
            assert_eq!(set.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { line_height: Some(lh), .. }) if *lh == 2.25
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_line_height_truncated_fails() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_LINE_HEIGHT);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_text_align_and_whitespace_decode_ids() {
        with_test(|| {
            let mut b = Buf::new();
            b.div()
                .op(OP_SET_TEXT_ALIGN)
                .i32(TEXT_ALIGN_CENTER)
                .op(OP_SET_WHITESPACE)
                .i32(WHITESPACE_NOWRAP)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div {
                        text_align: Some(TEXT_ALIGN_CENTER),
                        whitespace: Some(WHITESPACE_NOWRAP),
                        ..
                    })
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_text_align_truncated_fails() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_TEXT_ALIGN);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn set_font_family_decodes_string() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_FONT_FAMILY).str("Fira Code").set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { font_family: Some(f), .. }) if f == "Fira Code"
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_font_family_truncated_fails() {
        with_test(|| {
            // Declares a 16-byte string but the buffer ends before the payload.
            let mut b = Buf::new();
            b.div().op(OP_SET_FONT_FAMILY).u32(16);
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn typography_setters_on_text_top_fail() {
        with_test(|| {
            // The G8 setters route through with_top_div like every other
            // setter, so a text top must be rejected with WRONG_NODE_KIND.
            let cases: Vec<Box<dyn Fn(&mut Buf)>> = vec![
                Box::new(|b| { b.op(OP_SET_TEXT_SIZE).i32(14); }),
                Box::new(|b| { b.op(OP_SET_TEXT_COLOR).u8(0).u8(0).u8(0).u8(0); }),
                Box::new(|b| { b.op(OP_SET_FONT_WEIGHT).i32(400); }),
                Box::new(|b| { b.op(OP_SET_LINE_HEIGHT).i32(1500); }),
                Box::new(|b| { b.op(OP_SET_TEXT_ALIGN).i32(TEXT_ALIGN_LEFT); }),
                Box::new(|b| { b.op(OP_SET_WHITESPACE).i32(WHITESPACE_NORMAL); }),
                Box::new(|b| { b.op(OP_SET_FONT_FAMILY).str("Arial"); }),
            ];
            for apply in cases {
                let mut b = Buf::new();
                b.text("x", 0, 0, 0, 12.0);
                apply(&mut b);
                assert_eq!(b.build(0), GPUI_STATUS_WRONG_NODE_KIND);
            }
        });
    }

    // --- Keyboard navigation / a11y (issue #52) ---------------------------

    #[::core::prelude::v1::test]
    fn set_focusable_decodes_mode() {
        with_test(|| {
            // Nonzero → focusable; a later zero clears it back to not-focusable.
            let mut b = Buf::new();
            b.div()
                .op(OP_SET_FOCUSABLE)
                .i32(1)
                .op(OP_SET_FOCUSABLE)
                .i32(0)
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { focusable: Some(false), .. })
                ));
            });
            let mut on = Buf::new();
            on.div().op(OP_SET_FOCUSABLE).i32(1).set_root();
            assert_eq!(on.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { focusable: Some(true), .. })
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_tab_index_decodes_value() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_TAB_INDEX).i32(3).set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { tab_index: Some(3), .. })
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn set_tab_stop_decodes_mode() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_TAB_STOP).i32(0).set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { tab_stop: Some(false), .. })
                ));
            });
            let mut on = Buf::new();
            on.div().op(OP_SET_TAB_STOP).i32(1).set_root();
            assert_eq!(on.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { tab_stop: Some(true), .. })
                ));
            });
        });
    }

    #[::core::prelude::v1::test]
    fn focus_setters_truncated_fail() {
        with_test(|| {
            // Each focus setter needs one i32 operand; end the buffer right
            // after the opcode so the reader runs out of bytes.
            for opcode in [OP_SET_FOCUSABLE, OP_SET_TAB_INDEX, OP_SET_TAB_STOP] {
                let mut b = Buf::new();
                b.div().op(opcode);
                assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
            }
        });
    }

    #[::core::prelude::v1::test]
    fn focus_setters_on_text_top_fail() {
        with_test(|| {
            // The focus setters route through with_top_div like every other
            // setter, so a text top must be rejected with WRONG_NODE_KIND.
            for opcode in [OP_SET_FOCUSABLE, OP_SET_TAB_INDEX, OP_SET_TAB_STOP] {
                let mut b = Buf::new();
                b.text("x", 0, 0, 0, 12.0).op(opcode).i32(1);
                assert_eq!(b.build(0), GPUI_STATUS_WRONG_NODE_KIND);
            }
        });
    }

    // --- G6 scroll handle retention (issue #51) ----------------------------

    /// A keyed scroll div must reuse the same `ScrollHandle` across renders so
    /// its scroll position survives the full tree rebuild every state change
    /// triggers. `ScrollHandle` is `Rc`-based, so two lookups of the same key
    /// share one underlying offset cell: mutating it through one handle is
    /// visible through the other. This is the headless proof of the retention
    /// contract (the real scroll wiring needs a window and is exercised by the
    /// demo). Keyless divs get a fresh handle each call and share nothing.
    #[::core::prelude::v1::test]
    fn keyed_scroll_handle_is_retained_across_renders() {
        let store = Rc::new(RefCell::new(HashMap::new()));

        // Two renders of the same keyed div → the same retained handle.
        let first = scroll_handle_for(&store, Some("list"));
        let second = scroll_handle_for(&store, Some("list"));
        first.set_offset(point(px(0.0), px(-120.0)));
        assert_eq!(second.offset(), point(px(0.0), px(-120.0)));

        // A distinct key gets an independent handle (still at the origin).
        let other = scroll_handle_for(&store, Some("other"));
        assert_eq!(other.offset(), point(px(0.0), px(0.0)));

        // Keyless divs never retain: every call is a fresh, isolated handle.
        let keyless_a = scroll_handle_for(&store, None);
        let keyless_b = scroll_handle_for(&store, None);
        keyless_a.set_offset(point(px(0.0), px(-50.0)));
        assert_eq!(keyless_b.offset(), point(px(0.0), px(0.0)));
    }

    // --- Incremental keyed text update (issue #10) -------------------------

    /// Commit a small tree: a keyed `count` div wrapping one text node, plus a
    /// sibling keyed `static` div with its own text. Mirrors the Counter's
    /// count-card shape (keyed div → single text child).
    fn commit_count_tree(view: i32) {
        let mut b = Buf::new();
        b.div().op(OP_SET_KEY).str("root");
        b.div().op(OP_SET_KEY).str("count");
        b.text("Count: 0", 120, 200, 255, 44.0);
        b.add_child(); // text -> count
        b.add_child(); // count -> root
        b.div().op(OP_SET_KEY).str("static");
        b.text("keys: k j r", 130, 135, 148, 14.0);
        b.add_child(); // text -> static
        b.add_child(); // static -> root
        b.set_root();
        assert_eq!(b.build(view), GPUI_STATUS_OK);
    }

    /// Read the content of the first text child of the keyed div `key` in the
    /// committed tree for `view`.
    fn keyed_text(view: usize, key: &str) -> Option<String> {
        fn find<'a>(node: &'a UiNode, key: &str) -> Option<&'a str> {
            let UiNode::Div {
                key: node_key,
                children,
                ..
            } = node
            else {
                return None;
            };
            if node_key.as_deref() == Some(key) {
                return match children.first() {
                    Some(UiNode::Text { content, .. }) => Some(content.as_str()),
                    _ => None,
                };
            }
            children.iter().find_map(|c| find(c, key))
        }
        with_views(|views| {
            views
                .get(view)
                .and_then(|slot| slot.as_ref())
                .and_then(|root| find(root, key))
                .map(str::to_string)
        })
    }

    #[::core::prelude::v1::test]
    fn update_text_updates_keyed_node_in_place() {
        with_test(|| {
            commit_count_tree(0);
            assert_eq!(keyed_text(0, "count").as_deref(), Some("Count: 0"));

            let key = b"count";
            let text = b"Count: 42";
            let status = gpui_update_text(
                0,
                key.as_ptr(),
                key.len() as i32,
                text.as_ptr(),
                text.len() as i32,
            );
            assert_eq!(status, GPUI_STATUS_OK);
            // The keyed node's text changed in place...
            assert_eq!(keyed_text(0, "count").as_deref(), Some("Count: 42"));
            // ...and the sibling subtree is untouched (no rebuild happened).
            assert_eq!(keyed_text(0, "static").as_deref(), Some("keys: k j r"));
        });
    }

    #[::core::prelude::v1::test]
    fn update_text_missing_key_returns_not_found() {
        with_test(|| {
            commit_count_tree(0);
            let key = b"does-not-exist";
            let text = b"x";
            let status = gpui_update_text(
                0,
                key.as_ptr(),
                key.len() as i32,
                text.as_ptr(),
                text.len() as i32,
            );
            assert_eq!(status, GPUI_STATUS_KEY_NOT_FOUND);
            // Tree untouched.
            assert_eq!(keyed_text(0, "count").as_deref(), Some("Count: 0"));
        });
    }

    #[::core::prelude::v1::test]
    fn update_text_no_committed_tree_returns_not_found() {
        with_test(|| {
            // No build_tree call: the view slot is empty.
            let key = b"count";
            let text = b"x";
            let status = gpui_update_text(
                0,
                key.as_ptr(),
                key.len() as i32,
                text.as_ptr(),
                text.len() as i32,
            );
            assert_eq!(status, GPUI_STATUS_KEY_NOT_FOUND);
        });
    }

    #[::core::prelude::v1::test]
    fn update_text_rejects_bad_handles() {
        with_test(|| {
            commit_count_tree(0);
            let key = b"count";
            let text = b"x";
            // Negative view.
            assert_eq!(
                gpui_update_text(
                    -1,
                    key.as_ptr(),
                    key.len() as i32,
                    text.as_ptr(),
                    text.len() as i32,
                ),
                GPUI_STATUS_INVALID_HANDLE
            );
            // Null key pointer / negative length.
            assert_eq!(
                gpui_update_text(0, std::ptr::null(), 0, text.as_ptr(), text.len() as i32),
                GPUI_STATUS_TRUNCATED_BUFFER
            );
            assert_eq!(
                gpui_update_text(0, key.as_ptr(), -1, text.as_ptr(), text.len() as i32),
                GPUI_STATUS_TRUNCATED_BUFFER
            );
        });
    }

    #[::core::prelude::v1::test]
    fn update_text_keyed_div_without_text_child_returns_not_found() {
        with_test(|| {
            // A keyed div whose only child is another div (no text child).
            let mut b = Buf::new();
            b.div().op(OP_SET_KEY).str("root");
            b.div().op(OP_SET_KEY).str("empty");
            b.div(); // non-text child
            b.add_child(); // inner -> empty
            b.add_child(); // empty -> root
            b.set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);

            let key = b"empty";
            let text = b"x";
            let status = gpui_update_text(
                0,
                key.as_ptr(),
                key.len() as i32,
                text.as_ptr(),
                text.len() as i32,
            );
            assert_eq!(status, GPUI_STATUS_KEY_NOT_FOUND);
        });
    }

    // --- EVENT_QUEUE / gpui_event_copy_text (issue #70) --------------------

    /// Serializes every test that touches the process-global `EVENT_QUEUE`
    /// against the other suites that clear it (the drain-pump tests in
    /// `async_inject_tests` and the injection tests below all dispatch, and
    /// every dispatch clears the queue). Without this, a parallel test's
    /// clear between a push and its copy makes the token dangle — the
    /// nondeterministic CI failure first seen on the post-#102 main run. The closure also starts
    /// from an empty queue so tokens are deterministic.
    fn with_event_queue_test(f: impl FnOnce()) {
        let _lock = INJECT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_event_queue();
        f();
        clear_event_queue();
    }

    /// Test helper: push a payload into `EVENT_QUEUE` and return its token,
    /// mirroring the dispatch sites' push (the queue is cleared after every
    /// dispatch, so a test may seed it directly).
    fn push_event_payload(payload: &[u8]) -> i32 {
        let mut q = EVENT_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        q.push(payload.to_vec());
        (q.len() - 1) as i32
    }

    fn clear_event_queue() {
        EVENT_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    #[::core::prelude::v1::test]
    fn copy_text_copies_payload() {
        with_event_queue_test(|| {
            let token = push_event_payload(b"hello");
            let mut buf = [0u8; 8];
            let n = gpui_event_copy_text(token, buf.as_mut_ptr(), buf.len() as i32);
            assert_eq!(n, 5);
            assert_eq!(&buf[..5], b"hello");
        });
    }

    #[::core::prelude::v1::test]
    fn copy_text_truncates_to_buffer_len() {
        with_event_queue_test(|| {
            let token = push_event_payload(b"hello world");
            let mut buf = [0u8; 5];
            let n = gpui_event_copy_text(token, buf.as_mut_ptr(), buf.len() as i32);
            assert_eq!(n, 5);
            assert_eq!(&buf, b"hello");
        });
    }

    #[::core::prelude::v1::test]
    fn copy_text_zero_len_writes_nothing() {
        with_event_queue_test(|| {
            let token = push_event_payload(b"abc");
            let mut buf = [0xAAu8; 4];
            let n = gpui_event_copy_text(token, buf.as_mut_ptr(), 0);
            assert_eq!(n, 0);
            assert_eq!(&buf, &[0xAA; 4]);
        });
    }

    #[::core::prelude::v1::test]
    fn copy_text_rejects_invalid_arguments() {
        with_event_queue_test(|| {
            let token = push_event_payload(b"abc");
            let mut buf = [0u8; 4];
            assert_eq!(
                gpui_event_copy_text(-1, buf.as_mut_ptr(), 4),
                GPUI_STATUS_INVALID_HANDLE
            );
            assert_eq!(gpui_event_copy_text(token, std::ptr::null_mut(), 4), GPUI_STATUS_INVALID_HANDLE);
            assert_eq!(
                gpui_event_copy_text(token, buf.as_mut_ptr(), -1),
                GPUI_STATUS_INVALID_HANDLE
            );
            // Token past the end of the queue.
            assert_eq!(
                gpui_event_copy_text(token + 1, buf.as_mut_ptr(), 4),
                GPUI_STATUS_INVALID_HANDLE
            );
        });
    }

    /// #70 regression: the payload must be valid only during the synchronous
    /// dispatch. After the dispatch site clears the queue, the token no longer
    /// resolves — and the queue holds no stale entries to leak.
    #[::core::prelude::v1::test]
    fn copy_text_token_invalid_after_clear() {
        with_event_queue_test(|| {
            let token = push_event_payload(b"leak-me");
            let mut buf = [0u8; 8];
            assert_eq!(gpui_event_copy_text(token, buf.as_mut_ptr(), 8), 7);
            // Dispatch sites clear immediately after mb_dispatch returns.
            clear_event_queue();
            assert_eq!(
                gpui_event_copy_text(token, buf.as_mut_ptr(), 8),
                GPUI_STATUS_INVALID_HANDLE
            );
            assert!(EVENT_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        });
    }

    // --- gpui_post_event / injection queue (RFC 0002) ----------------------

    /// Serializes the injection-queue tests against each other and against the
    /// headless drain-pump tests (`async_inject_tests`): `INJECT` is a process
    /// global, so both suites lock `crate::INJECT_TEST_LOCK`.

    fn with_inject_test(f: impl FnOnce()) {
        let _lock = INJECT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(q) = INJECT.get() {
            q.entries.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        f();
        if let Some(q) = INJECT.get() {
            q.entries.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    #[::core::prelude::v1::test]
    fn post_event_queues_payload_for_drain() {
        with_inject_test(|| {
            let payload = b"hello";
            assert_eq!(gpui_post_event(3, payload.as_ptr(), payload.len() as i32), GPUI_STATUS_OK);
            let entry = pop_injected().expect("queued entry");
            assert_eq!(entry.view, 3);
            assert_eq!(entry.payload, payload);
            assert!(pop_injected().is_none());
        });
    }

    #[::core::prelude::v1::test]
    fn post_event_zero_len_carries_no_payload() {
        with_inject_test(|| {
            assert_eq!(gpui_post_event(0, std::ptr::null(), 0), GPUI_STATUS_OK);
            let entry = pop_injected().expect("queued entry");
            assert_eq!(entry.view, 0);
            assert!(entry.payload.is_empty());
        });
    }

    #[::core::prelude::v1::test]
    fn post_event_rejects_invalid_arguments() {
        with_inject_test(|| {
            let payload = b"x";
            assert_eq!(
                gpui_post_event(-1, payload.as_ptr(), 1),
                GPUI_STATUS_INVALID_HANDLE
            );
            assert_eq!(
                gpui_post_event(0, payload.as_ptr(), -1),
                GPUI_STATUS_INVALID_HANDLE
            );
            assert_eq!(gpui_post_event(0, std::ptr::null(), 1), GPUI_STATUS_INVALID_HANDLE);
            assert!(pop_injected().is_none());
        });
    }

    #[::core::prelude::v1::test]
    fn post_event_enforces_payload_limit() {
        with_inject_test(|| {
            // Exactly at the limit: accepted.
            let max_payload = vec![0xABu8; INJECT_PAYLOAD_MAX_BYTES];
            assert_eq!(
                gpui_post_event(0, max_payload.as_ptr(), INJECT_PAYLOAD_MAX_BYTES as i32),
                GPUI_STATUS_OK
            );
            assert_eq!(
                pop_injected().expect("queued entry").payload.len(),
                INJECT_PAYLOAD_MAX_BYTES
            );
            // One byte over: rejected before any copy.
            assert_eq!(
                gpui_post_event(0, max_payload.as_ptr(), INJECT_PAYLOAD_MAX_BYTES as i32 + 1),
                GPUI_STATUS_PAYLOAD_TOO_LARGE
            );
            assert!(pop_injected().is_none());
        });
    }

    #[::core::prelude::v1::test]
    fn post_event_back_pressures_when_full() {
        with_inject_test(|| {
            let payload = b"x";
            for _ in 0..INJECT_QUEUE_MAX_ENTRIES {
                assert_eq!(gpui_post_event(0, payload.as_ptr(), 1), GPUI_STATUS_OK);
            }
            // The 1025th entry is rejected, not dropped-silently or blocked on.
            assert_eq!(
                gpui_post_event(0, payload.as_ptr(), 1),
                GPUI_STATUS_QUEUE_FULL
            );
            // Draining one entry frees one slot.
            assert!(pop_injected().is_some());
            assert_eq!(gpui_post_event(0, payload.as_ptr(), 1), GPUI_STATUS_OK);
        });
    }

    #[::core::prelude::v1::test]
    fn post_event_preserves_fifo_order() {
        with_inject_test(|| {
            for i in 0..8u8 {
                assert_eq!(gpui_post_event(i as i32, &i, 1), GPUI_STATUS_OK);
            }
            for i in 0..8u8 {
                let entry = pop_injected().expect("queued entry");
                assert_eq!(entry.view, i as i32);
                assert_eq!(entry.payload, [i]);
            }
        });
    }

    // --- Scroll position feedback: decode, pull ABI, prune (issue #89) -----

    fn reset_scroll_statics() {
        *SCROLL_MIRROR.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *SCROLL_SENT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    #[::core::prelude::v1::test]
    fn scroll_id_decodes_onto_div() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_SCROLL_ID).i32(7).set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                assert!(matches!(
                    &views[0],
                    Some(UiNode::Div { scroll_id: Some(7), .. })
                ));
            });
        });
    }

    // --- OP_TEXT_RUN decode + validation (issue #91) ------------------------

    impl Buf {
        /// One `OP_TEXT_RUN` record with every operand slot spelled out.
        #[allow(clippy::too_many_arguments)]
        fn text_run(
            &mut self,
            start: u32,
            len: u32,
            flags: i32,
            color: (u8, u8, u8, u8),
            weight: i32,
            background: (u8, u8, u8, u8),
        ) -> &mut Self {
            self.op(OP_TEXT_RUN)
                .u32(start)
                .u32(len)
                .u8(flags as u8)
                .u8(color.0)
                .u8(color.1)
                .u8(color.2)
                .u8(color.3)
                .i32(weight)
                .u8(background.0)
                .u8(background.1)
                .u8(background.2)
                .u8(background.3)
        }
    }

    #[::core::prelude::v1::test]
    fn text_run_decodes_onto_text_node_in_order() {
        with_test(|| {
            let mut b = Buf::new();
            b.text("abcdef", 0, 0, 0, 12.0)
                .text_run(
                    0,
                    2,
                    RUN_STYLE_COLOR | RUN_STYLE_WEIGHT,
                    (1, 2, 3, 4),
                    700,
                    (0, 0, 0, 0),
                )
                .text_run(3, 2, RUN_STYLE_ITALIC, (0, 0, 0, 0), 0, (0, 0, 0, 0))
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
            with_views(|views| {
                let Some(UiNode::Text { runs, .. }) = &views[0] else {
                    panic!("text root expected");
                };
                assert_eq!(
                    runs,
                    &vec![
                        TextRunSpec {
                            start: 0,
                            len: 2,
                            flags: RUN_STYLE_COLOR | RUN_STYLE_WEIGHT,
                            color: (1, 2, 3, 4),
                            weight: 700,
                            background: (0, 0, 0, 0),
                        },
                        TextRunSpec {
                            start: 3,
                            len: 2,
                            flags: RUN_STYLE_ITALIC,
                            color: (0, 0, 0, 0),
                            weight: 0,
                            background: (0, 0, 0, 0),
                        },
                    ]
                );
            });
        });
    }

    #[::core::prelude::v1::test]
    fn scroll_id_on_text_is_wrong_node_kind() {
        with_test(|| {
            let mut b = Buf::new();
            b.text("x", 0, 0, 0, 1.0).op(OP_SET_SCROLL_ID).i32(1);
            assert_eq!(b.build(0), GPUI_STATUS_WRONG_NODE_KIND);
        });
    }

    #[::core::prelude::v1::test]
    fn text_run_accepts_multibyte_char_boundaries() {
        with_test(|| {
            let mut b = Buf::new();
            // "あいう" = 9 UTF-8 bytes; run over "い" (bytes 3..6).
            b.text("あいう", 0, 0, 0, 12.0)
                .text_run(3, 3, RUN_STYLE_WEIGHT, (0, 0, 0, 0), 700, (0, 0, 0, 0))
                .set_root();
            assert_eq!(b.build(0), GPUI_STATUS_OK);
        });
    }

    #[::core::prelude::v1::test]
    fn text_run_on_div_is_wrong_node_kind() {
        with_test(|| {
            let mut b = Buf::new();
            b.div()
                .text_run(0, 0, 0, (0, 0, 0, 0), 0, (0, 0, 0, 0));
            assert_eq!(b.build(0), GPUI_STATUS_WRONG_NODE_KIND);
        });
    }

    #[::core::prelude::v1::test]
    fn scroll_id_truncated_operand_is_rejected() {
        with_test(|| {
            let mut b = Buf::new();
            b.div().op(OP_SET_SCROLL_ID).u8(1); // 1 of 4 operand bytes
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn text_run_rejects_out_of_bounds() {
        with_test(|| {
            let mut b = Buf::new();
            b.text("ab", 0, 0, 0, 12.0)
                .text_run(1, 5, 0, (0, 0, 0, 0), 0, (0, 0, 0, 0));
            assert_eq!(b.build(0), GPUI_STATUS_INVALID_TEXT_RUN);
        });
    }

    #[::core::prelude::v1::test]
    fn text_run_rejects_non_char_boundary() {
        with_test(|| {
            let mut b = Buf::new();
            // byte 1 is inside "あ"'s 3-byte encoding.
            b.text("あ", 0, 0, 0, 12.0)
                .text_run(1, 1, 0, (0, 0, 0, 0), 0, (0, 0, 0, 0));
            assert_eq!(b.build(0), GPUI_STATUS_INVALID_TEXT_RUN);
        });
    }

    #[::core::prelude::v1::test]
    fn text_run_rejects_overlapping_or_unsorted_runs() {
        with_test(|| {
            let mut b = Buf::new();
            b.text("abcdef", 0, 0, 0, 12.0)
                .text_run(0, 3, 0, (0, 0, 0, 0), 0, (0, 0, 0, 0))
                .text_run(2, 2, 0, (0, 0, 0, 0), 0, (0, 0, 0, 0));
            assert_eq!(b.build(0), GPUI_STATUS_INVALID_TEXT_RUN);
        });
    }

    #[::core::prelude::v1::test]
    fn text_run_rejects_unknown_flag_bits() {
        with_test(|| {
            let mut b = Buf::new();
            b.text("ab", 0, 0, 0, 12.0)
                .text_run(0, 1, 0x40, (0, 0, 0, 0), 0, (0, 0, 0, 0));
            assert_eq!(b.build(0), GPUI_STATUS_INVALID_TEXT_RUN);
        });
    }

    #[::core::prelude::v1::test]
    fn text_run_truncated_operands_are_rejected() {
        with_test(|| {
            let mut b = Buf::new();
            b.text("ab", 0, 0, 0, 12.0).op(OP_TEXT_RUN).u32(0); // record cut short
            assert_eq!(b.build(0), GPUI_STATUS_TRUNCATED_BUFFER);
        });
    }

    #[::core::prelude::v1::test]
    fn scroll_pull_validates_arguments_and_reads_the_mirror() {
        with_test(|| {
            reset_scroll_statics();
            let mut buf = [0u8; SCROLL_STATE_BYTES];

            assert_eq!(
                gpui_scroll_copy_state(-1, 7, buf.as_mut_ptr(), buf.len() as i32),
                GPUI_STATUS_INVALID_HANDLE
            );
            assert_eq!(
                gpui_scroll_copy_state(0, 7, std::ptr::null_mut(), buf.len() as i32),
                GPUI_STATUS_INVALID_HANDLE
            );
            assert_eq!(
                gpui_scroll_copy_state(0, 7, buf.as_mut_ptr(), SCROLL_STATE_BYTES as i32 - 1),
                GPUI_STATUS_INVALID_HANDLE
            );
            assert_eq!(
                gpui_scroll_copy_state(0, 7, buf.as_mut_ptr(), buf.len() as i32),
                GPUI_STATUS_KEY_NOT_FOUND
            );

            scroll_mirror_update(
                0,
                7,
                ScrollMirrorEntry {
                    offset: (-1.5, -30.0),
                    max: (0.0, 400.0),
                    viewport: (200.0, 100.0),
                },
            );
            assert_eq!(
                gpui_scroll_copy_state(0, 7, buf.as_mut_ptr(), buf.len() as i32),
                SCROLL_STATE_BYTES as i32
            );
            let values: Vec<f32> = buf
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            assert_eq!(values, [-1.5, -30.0, 0.0, 400.0, 200.0, 100.0]);
        });
    }

    #[::core::prelude::v1::test]
    fn rebuild_prunes_removed_scroll_ids_per_view() {
        with_test(|| {
            reset_scroll_statics();
            let mut buf = [0u8; SCROLL_STATE_BYTES];

            // View 0 subscribes id 7; view 1's mirror entry must survive
            // view 0's rebuilds untouched.
            let mut with_id = Buf::new();
            with_id.div().op(OP_SET_SCROLL_ID).i32(7).set_root();
            assert_eq!(with_id.build(0), GPUI_STATUS_OK);
            scroll_mirror_update(0, 7, ScrollMirrorEntry::default());
            scroll_mirror_update(1, 7, ScrollMirrorEntry::default());
            SCROLL_SENT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(HashMap::new)
                .insert((0, 7), (0.0, 0.0));

            // Rebuilding view 0 with the id still present keeps the state.
            assert_eq!(with_id.build(0), GPUI_STATUS_OK);
            assert_eq!(
                gpui_scroll_copy_state(0, 7, buf.as_mut_ptr(), buf.len() as i32),
                SCROLL_STATE_BYTES as i32
            );

            // Rebuilding without it prunes mirror + edge-detection state for
            // view 0 only.
            let mut without_id = Buf::new();
            without_id.div().set_root();
            assert_eq!(without_id.build(0), GPUI_STATUS_OK);
            assert_eq!(
                gpui_scroll_copy_state(0, 7, buf.as_mut_ptr(), buf.len() as i32),
                GPUI_STATUS_KEY_NOT_FOUND
            );
            assert!(
                !SCROLL_SENT
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .is_some_and(|m| m.contains_key(&(0, 7)))
            );
            assert_eq!(
                gpui_scroll_copy_state(1, 7, buf.as_mut_ptr(), buf.len() as i32),
                SCROLL_STATE_BYTES as i32
            );
        });
    }

    /// Every `#[unsafe(no_mangle)] pub extern "C"` export must run its body
    /// inside `ffi_export`, the `catch_unwind` wrapper that turns a panic into
    /// `GPUI_STATUS_INTERNAL_PANIC`. This check is textual (we read this crate's
    /// own source) because there is no attribute-level hook that could enforce
    /// it at compile time. The failure it prevents is silent: an unwrapped
    /// export only misbehaves when something inside it actually panics, at which
    /// point the process aborts instead of returning a status.
    #[::core::prelude::v1::test]
    fn every_c_export_goes_through_ffi_export() {
        let source = include_str!("lib.rs");
        let lines: Vec<&str> = source.lines().collect();
        let mut i = 0;
        let mut exports_found = 0;
        while i < lines.len() {
            if lines[i].trim() == "#[unsafe(no_mangle)]" {
                let mut j = i + 1;
                let mut name: Option<&str> = None;
                while j < lines.len() {
                    let trimmed = lines[j].trim();
                    if let Some(idx) = lines[j].find("pub extern \"C\" fn ") {
                        let rest = &lines[j][idx + "pub extern \"C\" fn ".len()..];
                        let name_end = rest
                            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .unwrap_or(rest.len());
                        name = Some(&rest[..name_end]);
                    }
                    if trimmed.ends_with('{') {
                        break;
                    }
                    j += 1;
                }
                if j >= lines.len() {
                    break;
                }
                let Some(name) = name else {
                    let offending = &lines[j].trim();
                    panic!(
                        "could not find the export name near `{}`; \
                         every #[unsafe(no_mangle)] export runs under ffi_export(\"<name>\", || ...) \
                         and this export was missing one",
                        offending
                    );
                };
                exports_found += 1;
                let mut k = j + 1;
                while k < lines.len()
                    && (lines[k].trim().is_empty() || lines[k].trim().starts_with("//"))
                {
                    k += 1;
                }
                let first = lines.get(k).map(|l| l.trim()).unwrap_or("");
                // Two distinct failures, reported separately: no wrapper at all,
                // and a wrapper labelled with someone else's name (a copy-paste
                // of a neighbouring export, which would misattribute the panic
                // in `report_panic` while still catching it).
                assert!(
                    first.starts_with("ffi_export("),
                    "unwrapped C export `{name}`: its body must be wrapped in \
                     ffi_export(\"{name}\", || {{ ... }}) so a panic returns \
                     GPUI_STATUS_INTERNAL_PANIC instead of aborting the process"
                );
                assert!(
                    first.starts_with(&format!("ffi_export(\"{name}\"")),
                    "C export `{name}` is wrapped under the wrong name: `{first}`. \
                     The label is what `report_panic` prints, so it must match \
                     the exported function"
                );
                i = k + 1;
            } else {
                i += 1;
            }
        }
        assert!(
            exports_found >= 10,
            "expected at least 10 #[unsafe(no_mangle)] exports, found {}; \
             this floor keeps the check non-vacuous so a parsing change that \
             silently matches nothing cannot make it pass",
            exports_found
        );
    }

    /// The other half of the contract the guard above enforces textually: that
    /// `ffi_export` actually converts a panic into a status instead of letting
    /// it cross the C boundary. Wrapping every export is only worth checking if
    /// the wrapper does its job.
    ///
    /// The panic hook is swapped out for the duration: `catch_unwind` still runs
    /// it, and the default one would print a scary backtrace for a panic this
    /// test is deliberately causing.
    #[::core::prelude::v1::test]
    fn ffi_export_converts_panic_to_status() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let status = ffi_export("test_panicking_export", || panic!("deliberate"));
        let ok = ffi_export("test_ok_export", || GPUI_STATUS_OK);
        std::panic::set_hook(previous);

        assert_eq!(status, GPUI_STATUS_INTERNAL_PANIC);
        assert_eq!(ok, GPUI_STATUS_OK, "the non-panicking path is untouched");
    }
}

/// G25 decoder fuzzing: deterministic seeded PRNG over random and
/// structurally-plausible command buffers; the decoder must never panic.
#[cfg(test)]
mod fuzz_tests;
