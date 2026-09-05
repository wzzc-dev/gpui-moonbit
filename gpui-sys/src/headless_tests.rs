//! G24 golden layout tests.
//!
//! Each test decodes a hand-built command buffer through the real decoder,
//! renders it headlessly through the real `render_node` path, and asserts the
//! exact laid-out bounds of keyed elements. The headless stack is fully
//! deterministic (taffy rounding + gpui's `NoopTextSystem`: ascent 1025,
//! descent 275 per 1000 units; glyph advance 600/1000·size), so the goldens
//! below were captured from the first verified run and now serve as a
//! regression net: any drift in decoding, style mapping, or gpui layout
//! fails here with the offending selector named.
//!
//! The headless window is the `TestDisplay`'s 1920×1080; `FfiView`'s root
//! fills it, so a root without an explicit size measures 1920×1080 and
//! children lay out from the top-left corner.

use crate::headless::{assert_bounds_eq, layout_bound, layout_bounds};
use crate::abi_constants::{ALIGN_START, BUFFER_VERSION, JUSTIFY_DEFAULT, OP_ADD_CHILD, OP_DIV,
    OP_SET_ALIGN, OP_SET_BORDER, OP_SET_FLEX, OP_SET_GAP, OP_SET_KEY, OP_SET_PADDING,
    OP_SET_ROOT, OP_SET_ROUNDED, OP_SET_SIZE, OP_SET_TEXT_ROW, OP_TEXT, OP_TEXT_RUN,
    RUN_STYLE_COLOR, RUN_STYLE_WEIGHT};
use crate::{GPUI_STATUS_BAD_BUFFER_VERSION, GPUI_STATUS_INVALID_FLOAT};
use gpui::TestAppContext;

/// Minimal command-buffer builder (little-endian, matching the wire layout
/// documented on `build_tree_from_buffer`).
struct Buf(Vec<u8>);

impl Buf {
    fn new() -> Self {
        let mut b = Buf(Vec::new());
        b.0.extend_from_slice(b"GPUI");
        b.0.extend_from_slice(&BUFFER_VERSION.to_le_bytes());
        b
    }
    fn op(mut self, opcode: i32) -> Self {
        self.0.push(opcode as u8);
        self
    }
    fn u8(self, v: u8) -> Self {
        self.op(v as i32)
    }
    fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f32(mut self, v: f32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn key(self, k: &str) -> Self {
        self.op(OP_SET_KEY).u32(k.len() as u32).bytes(k.as_bytes())
    }
    fn bytes(mut self, bs: &[u8]) -> Self {
        self.0.extend_from_slice(bs);
        self
    }
    fn div(self) -> Self {
        self.op(OP_DIV)
    }
    fn size(self, w: f32, h: f32) -> Self {
        self.op(OP_SET_SIZE).f32(w).f32(h)
    }
    fn add_child(self) -> Self {
        self.op(OP_ADD_CHILD)
    }
    fn root(self) -> Self {
        self.op(OP_SET_ROOT)
    }
    fn finish(self) -> Vec<u8> {
        self.0
    }
}

/// A fixed-size div at the window origin.
#[gpui::test]
fn sized_div(cx: &mut TestAppContext) {
    let buf = Buf::new().div().key("box").size(100.0, 50.0).root().finish();
    let b = layout_bound(cx, &buf, "box").expect("decode");
    assert_bounds_eq("box", b, 0.0, 0.0, 100.0, 50.0);
}

/// A flex row with a gap and two fixed-size children: the row fills the
/// window, children sit side by side with the gap between them.
#[gpui::test]
fn flex_row_gap_children(cx: &mut TestAppContext) {
    let buf = Buf::new()
        .div()
        .key("row")
        .op(OP_SET_FLEX)
        .u8(0) // row
        .op(OP_SET_GAP)
        .f32(10.0)
        .div()
        .key("a")
        .size(30.0, 20.0)
        .add_child()
        .div()
        .key("b")
        .size(40.0, 25.0)
        .add_child()
        .root()
        .finish();
    let bounds = layout_bounds(cx, &buf, &["row", "a", "b"]).expect("decode");
    assert_bounds_eq("row", bounds["row"], 0.0, 0.0, 1920.0, 1080.0);
    assert_bounds_eq("a", bounds["a"], 0.0, 0.0, 30.0, 20.0);
    // b starts after a's width (30) plus the 10px gap.
    assert_bounds_eq("b", bounds["b"], 40.0, 0.0, 40.0, 25.0);
}

/// Padding and border both inset children: a 200×100 box with 10px padding
/// and a 2px border places its 50×30 child at (12, 12).
#[gpui::test]
fn padded_border_box(cx: &mut TestAppContext) {
    let buf = Buf::new()
        .div()
        .key("box")
        .size(200.0, 100.0)
        .op(OP_SET_PADDING)
        .f32(10.0)
        .op(OP_SET_BORDER)
        .f32(2.0)
        .u8(0)
        .u8(0)
        .u8(0)
        .div()
        .key("inner")
        .size(50.0, 30.0)
        .add_child()
        .root()
        .finish();
    let bounds = layout_bounds(cx, &buf, &["box", "inner"]).expect("decode");
    assert_bounds_eq("box", bounds["box"], 0.0, 0.0, 200.0, 100.0);
    assert_bounds_eq("inner", bounds["inner"], 12.0, 12.0, 50.0, 30.0);
}

/// A text node's natural size comes from the headless `NoopTextSystem`:
/// advance 600/1000·size per glyph, ascent 1025/1000·size + descent
/// 275/1000·size, so "Hello" at 20px is 5 × 12px = 60px wide. The line height
/// is gpui's default `phi()` (1.618034 × size), which the text div rounds to
/// 32.5px tall.
///
/// As a bare root the text div would be stretched to the window width by
/// `FfiView`'s flex-col root (default `align: stretch`), so we wrap it in a
/// column with `align_items: start` to read its natural width. The wrapper is
/// the root and thus fills the 1920×1080 window.
#[gpui::test]
fn text_node_known_size(cx: &mut TestAppContext) {
    let buf = Buf::new()
        .div()
        .key("wrap")
        .op(OP_SET_FLEX)
        .u8(1) // column
        .op(OP_SET_ALIGN)
        .u32(ALIGN_START as u32)
        .u32(JUSTIFY_DEFAULT as u32)
        .op(OP_TEXT)
        .u32(5)
        .bytes(b"Hello")
        .u8(255)
        .u8(255)
        .u8(255)
        .f32(20.0)
        .add_child() // text -> wrap
        .root()
        .finish();
    let bounds = layout_bounds(cx, &buf, &["wrap", "text:Hello"]).expect("decode");
    assert_bounds_eq("wrap", bounds["wrap"], 0.0, 0.0, 1920.0, 1080.0);
    // Natural text size: 60px wide (5 glyphs × 12px), 32.5px tall (phi line
    // height). The x origin is 0.25px, not 0: `TextGlyphInset` shifts the
    // text's paint origin by a fractional ¼px so the leading glyph escapes
    // subpixel variant 0 (see its doc comment). Captured from the first
    // verified headless run.
    assert_bounds_eq("text:Hello", bounds["text:Hello"], 0.25, 0.0, 60.0, 32.5);
}

/// Rich text (issue #91): a text node with styled runs must lay out exactly
/// like the same text without runs — style overrides change paint, not
/// geometry — and the run boundaries must map to the glyph-advance grid.
///
/// The `NoopTextSystem` advances 600/1000·size per glyph regardless of weight
/// or color, so "abcdef" at 20px spans 6 × 12px = 72px whether or not runs
/// split it. Run-boundary positions are read through the render-time
/// `TextLayout` stash (`text_layout_for`): `position_for_index` maps a UTF-8
/// byte offset to pixels, and the run over bytes 2..4 must start and end
/// exactly two advances apart.
#[gpui::test]
fn rich_text_runs_keep_layout_and_map_boundaries(cx: &mut TestAppContext) {
    let buf = Buf::new()
        .div()
        .key("wrap")
        .op(OP_SET_FLEX)
        .u8(1) // column
        .op(OP_SET_ALIGN)
        .u32(ALIGN_START as u32)
        .u32(JUSTIFY_DEFAULT as u32)
        .op(OP_TEXT)
        .u32(6)
        .bytes(b"abcdef")
        .u8(255)
        .u8(255)
        .u8(255)
        .f32(20.0)
        // One run over "cd" (bytes 2..4): color + weight overrides.
        .op(OP_TEXT_RUN)
        .u32(2)
        .u32(2)
        .u8((RUN_STYLE_COLOR | RUN_STYLE_WEIGHT) as u8)
        .u8(10)
        .u8(20)
        .u8(30)
        .u8(255)
        .u32(700) // weight (i32 LE == u32 LE for positive values)
        .u8(0)
        .u8(0)
        .u8(0)
        .u8(0)
        .add_child() // text -> wrap
        .root()
        .finish();
    let bounds = layout_bounds(cx, &buf, &["wrap", "text:abcdef"]).expect("decode");
    // Same geometry as the plain-text path: 72×32.5 at the ¼px glyph inset.
    assert_bounds_eq("text:abcdef", bounds["text:abcdef"], 0.25, 0.0, 72.0, 32.5);

    let layout = crate::text_layout_for("abcdef").expect("rich text stashes its layout");
    let x_at = |index: usize| -> f32 {
        f32::from(
            layout
                .position_for_index(index)
                .unwrap_or_else(|| panic!("no position for byte {index}"))
                .x,
        )
    };
    // Run boundaries land on the glyph-advance grid: 12px per glyph at 20px.
    assert_eq!(x_at(2) - x_at(0), 24.0, "run start (byte 2)");
    assert_eq!(x_at(4) - x_at(2), 24.0, "run end (byte 4)");
}

/// A rejected buffer surfaces the decoder status instead of rendering.
#[gpui::test]
fn decoder_rejection_surfaces_status(cx: &mut TestAppContext) {
    let mut buf = Buf::new().div().key("box").size(10.0, 10.0).root().finish();
    buf[0] = b'X'; // corrupt the magic
    let err = layout_bound(cx, &buf, "box").expect_err("must reject");
    assert_eq!(err, GPUI_STATUS_BAD_BUFFER_VERSION);
    // Sanity: the uncorrupted buffer decodes and renders fine (through the
    // lock-protected harness, like every other read of `VIEWS`).
    buf[0] = b'G';
    let b = layout_bound(cx, &buf, "box").expect("uncorrupted buffer must decode");
    assert_bounds_eq("box", b, 0.0, 0.0, 10.0, 10.0);
}

// ---------------------------------------------------------------------
// Issue #75: non-finite and extreme f32 layout operands.
//
// `BufferReader::read_layout_f32` (lib.rs) rejects non-finite values (NaN,
// ±infinity) with `GPUI_STATUS_INVALID_FLOAT` and clamps finite values to
// ±`MAX_LAYOUT_PX`, six opcodes deep: `OP_TEXT` (font size), `OP_SET_SIZE`
// (width, height), `OP_SET_GAP`, `OP_SET_ROUNDED`, `OP_SET_PADDING`, and
// `OP_SET_BORDER` (width only; the border color bytes are untouched).
// ---------------------------------------------------------------------

/// Every non-finite value tried against each opcode's f32 operand(s).
const NON_FINITE_PROBES: [(&str, f32); 3] = [
    ("+inf", f32::INFINITY),
    ("-inf", f32::NEG_INFINITY),
    ("NaN", f32::NAN),
];

/// `OP_TEXT`'s font-size operand rejects non-finite values.
#[gpui::test]
fn op_text_rejects_non_finite_size(cx: &mut TestAppContext) {
    for (label, v) in NON_FINITE_PROBES {
        let buf = Buf::new()
            .op(OP_TEXT)
            .u32(5)
            .bytes(b"Hello")
            .u8(255)
            .u8(255)
            .u8(255)
            .f32(v)
            .root()
            .finish();
        let err = layout_bound(cx, &buf, "text:Hello")
            .expect_err(&format!("OP_TEXT size={label} must be rejected"));
        assert_eq!(err, GPUI_STATUS_INVALID_FLOAT, "OP_TEXT size={label}");
    }
}

/// `OP_SET_SIZE`'s width/height operands reject non-finite values (both set
/// to the probed value).
#[gpui::test]
fn op_set_size_rejects_non_finite(cx: &mut TestAppContext) {
    for (label, v) in NON_FINITE_PROBES {
        let buf = Buf::new().div().key("box").size(v, v).root().finish();
        let err = layout_bound(cx, &buf, "box")
            .expect_err(&format!("OP_SET_SIZE w=h={label} must be rejected"));
        assert_eq!(err, GPUI_STATUS_INVALID_FLOAT, "OP_SET_SIZE w=h={label}");
    }
}

/// `OP_SET_GAP`'s gap operand rejects non-finite values.
#[gpui::test]
fn op_set_gap_rejects_non_finite(cx: &mut TestAppContext) {
    for (label, v) in NON_FINITE_PROBES {
        let buf = Buf::new()
            .div()
            .key("box")
            .op(OP_SET_FLEX)
            .u8(0) // row
            .op(OP_SET_GAP)
            .f32(v)
            .root()
            .finish();
        let err = layout_bound(cx, &buf, "box")
            .expect_err(&format!("OP_SET_GAP={label} must be rejected"));
        assert_eq!(err, GPUI_STATUS_INVALID_FLOAT, "OP_SET_GAP={label}");
    }
}

/// `OP_SET_ROUNDED`'s radius operand rejects non-finite values.
#[gpui::test]
fn op_set_rounded_rejects_non_finite(cx: &mut TestAppContext) {
    for (label, v) in NON_FINITE_PROBES {
        let buf = Buf::new()
            .div()
            .key("box")
            .size(100.0, 50.0)
            .op(OP_SET_ROUNDED)
            .f32(v)
            .root()
            .finish();
        let err = layout_bound(cx, &buf, "box")
            .expect_err(&format!("OP_SET_ROUNDED radius={label} must be rejected"));
        assert_eq!(err, GPUI_STATUS_INVALID_FLOAT, "OP_SET_ROUNDED radius={label}");
    }
}

/// `OP_SET_PADDING`'s padding operand rejects non-finite values.
#[gpui::test]
fn op_set_padding_rejects_non_finite(cx: &mut TestAppContext) {
    for (label, v) in NON_FINITE_PROBES {
        let buf = Buf::new()
            .div()
            .key("box")
            .size(100.0, 50.0)
            .op(OP_SET_PADDING)
            .f32(v)
            .root()
            .finish();
        let err = layout_bound(cx, &buf, "box")
            .expect_err(&format!("OP_SET_PADDING padding={label} must be rejected"));
        assert_eq!(err, GPUI_STATUS_INVALID_FLOAT, "OP_SET_PADDING padding={label}");
    }
}

/// `OP_SET_BORDER`'s width operand rejects non-finite values; the color
/// bytes are fixed at (0, 0, 0) and unrelated to the check.
#[gpui::test]
fn op_set_border_rejects_non_finite(cx: &mut TestAppContext) {
    for (label, v) in NON_FINITE_PROBES {
        let buf = Buf::new()
            .div()
            .key("box")
            .size(100.0, 50.0)
            .op(OP_SET_BORDER)
            .f32(v)
            .u8(0)
            .u8(0)
            .u8(0)
            .root()
            .finish();
        let err = layout_bound(cx, &buf, "box")
            .expect_err(&format!("OP_SET_BORDER width={label} must be rejected"));
        assert_eq!(err, GPUI_STATUS_INVALID_FLOAT, "OP_SET_BORDER width={label}");
    }
}

/// `f32::MAX` fed into `OP_SET_SIZE` must clamp to a *finite* layout instead
/// of overflowing to `inf` (which is what happened before `read_layout_f32`
/// clamped geometry operands to ±`MAX_LAYOUT_PX`). The finiteness of the
/// bounds below is the regression net; the exact numbers were captured from
/// the first verified run.
#[gpui::test]
fn op_set_size_clamps_extreme_to_finite(cx: &mut TestAppContext) {
    let buf = Buf::new()
        .div()
        .key("box")
        .size(f32::MAX, f32::MAX)
        .root()
        .finish();
    let b = layout_bound(cx, &buf, "box").expect("clamped size must decode");
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    assert!(w.is_finite(), "width must be finite, got {w}");
    assert!(h.is_finite(), "height must be finite, got {h}");
    // Width clamps to MAX_LAYOUT_PX as requested; height is further capped to
    // the 1080px window (taffy still constrains the root's cross-axis extent
    // to its container even past the clamp). Captured from the first
    // verified run.
    assert_bounds_eq("box", b, 0.0, 0.0, 1_000_000.0, 1080.0);
}

/// `f32::MAX` fed into `OP_SET_GAP` must clamp to a finite gap instead of
/// making the second child's position `NaN` (which is what happened before
/// clamping: `inf` gap arithmetic in taffy produces `NaN` widths). Measures
/// the row's second child (`key("b")`), not the row itself: the row fills
/// the window regardless of the gap, so its own bounds say nothing about it.
#[gpui::test]
fn op_set_gap_clamps_extreme_to_finite(cx: &mut TestAppContext) {
    let buf = Buf::new()
        .div()
        .key("row")
        .op(OP_SET_FLEX)
        .u8(0) // row
        .op(OP_SET_GAP)
        .f32(f32::MAX)
        .div()
        .key("a")
        .size(30.0, 20.0)
        .add_child()
        .div()
        .key("b")
        .size(40.0, 25.0)
        .add_child()
        .root()
        .finish();
    let b = layout_bound(cx, &buf, "b").expect("clamped gap must decode");
    let (x, w) = (f32::from(b.origin.x), f32::from(b.size.width));
    assert!(x.is_finite(), "b.origin.x must be finite, got {x}");
    assert!(w.is_finite(), "b.size.width must be finite, got {w}");
    // `b`'s origin.x clamps to MAX_LAYOUT_PX (the gap pushed it there); its
    // width shrinks to 0 because a 1e6px gap in a 1920px-wide row leaves no
    // room for the child. Both are finite, unlike the pre-clamp NaN width.
    // Captured from the first verified run.
    assert_bounds_eq("b", b, 1_000_000.0, 0.0, 0.0, 25.0);
}

/// A negative `OP_SET_SIZE` isn't rejected by `read_layout_f32` (only
/// non-finite values are); the pre-existing `> 0.0` size guard elsewhere in
/// gpui simply ignores it, so the div falls back to its parent's size (here,
/// the 1920×1080 window, since it's the root).
#[gpui::test]
fn op_set_size_negative_is_ignored(cx: &mut TestAppContext) {
    let buf = Buf::new().div().key("box").size(-1.0, -1.0).root().finish();
    let b = layout_bound(cx, &buf, "box").expect("decode");
    assert_bounds_eq("box", b, 0.0, 0.0, 1920.0, 1080.0);
}

// ---------------------------------------------------------------------
// Issue #74: recursion depth of the tree walkers.
//
// `render_node` measured at roughly 70 KB of stack per nesting level in a
// debug build: before `stacker` was wired in, a chain of 32 divs aborted the
// test process outright ("has overflowed its stack"), and 8 MiB of thread
// stack only reached the low hundreds. A stack overflow is not catchable, so
// the failure mode was process death rather than a status code.
//
// Two invariants are pinned here: a tree at exactly `MAX_TREE_DEPTH` still
// renders (the growth actually works), and one level deeper is rejected at
// decode time (the bound is enforced before anything walks the tree).
// ---------------------------------------------------------------------

/// Build a chain of `depth` nested divs with the outermost as root.
fn nested_chain(depth: usize) -> Vec<u8> {
    let mut buf = Buf::new();
    for _ in 0..depth {
        buf = buf.div();
    }
    for _ in 1..depth {
        buf = buf.add_child();
    }
    buf.key("leaf").root().finish()
}

/// A tree nested to exactly `MAX_TREE_DEPTH` decodes, commits, and renders.
/// Without `stacker` this aborts the process long before reaching 1024.
#[gpui::test]
fn tree_at_max_depth_renders(cx: &mut TestAppContext) {
    let buf = nested_chain(crate::MAX_TREE_DEPTH as usize);
    layout_bounds(cx, &buf, &[]).expect("a tree at MAX_TREE_DEPTH must render");
}

/// One level past the limit is rejected before commit, so no walker ever sees
/// it — the status arrives instead of an abort.
#[gpui::test]
fn tree_past_max_depth_is_rejected(cx: &mut TestAppContext) {
    let buf = nested_chain(crate::MAX_TREE_DEPTH as usize + 1);
    let err = layout_bounds(cx, &buf, &[]).expect_err("past MAX_TREE_DEPTH must be rejected");
    assert_eq!(err, crate::GPUI_STATUS_DEPTH_EXCEEDED);
}

// --- text input sizing (RFC 0003) ------------------------------------
//
// The `OP_TEXT_INPUT` leaf is laid out at 100% of its parent's width, so the
// parent needs a *definite* width. A centered column (`OP_SET_CENTER`) sizes
// its children to their content, which makes that percentage resolve to 0:
// the frame collapses to padding + border, the placeholder paints outside it,
// and the click hitbox is zero-wide (the widget cannot be focused at all).
// The `text_input` component therefore always pins a minimum width.

/// The failure mode: no width on the frame, so the input measures 0px wide.
#[gpui::test]
fn text_input_without_a_definite_frame_width_collapses(cx: &mut TestAppContext) {
    let buf = prompt_box_buffer(None);
    let b = layout_bounds(cx, &buf, &["frame", "input:1"]).expect("decode");
    // Frame = 2×8 padding + 2×1 border only; the input leaf gets nothing.
    assert_bounds_eq("frame", b["frame"], 951.0, 518.0, 18.0, 44.0);
    assert_bounds_eq("input:1", b["input:1"], 960.0, 527.0, 0.0, 26.0);
}

/// With a minimum width the frame is definite, so the leaf fills its content
/// box (360 − 2×1 border − 2×8 padding = 342) and the placeholder, the caret
/// and the click hitbox all live inside the drawn box.
#[gpui::test]
fn text_input_min_width_sizes_the_input_leaf(cx: &mut TestAppContext) {
    let buf = prompt_box_buffer(Some(360));
    let b = layout_bounds(cx, &buf, &["frame", "input:1"]).expect("decode");
    assert_bounds_eq("frame", b["frame"], 780.0, 518.0, 360.0, 44.0);
    assert_bounds_eq("input:1", b["input:1"], 789.0, 527.0, 342.0, 26.0);
}

/// The demo's prompt box: a bordered, padded frame holding one `OP_TEXT_INPUT`
/// leaf, inside a centered flex column (mirrors `app.mbt`'s `prompt_box`).
fn prompt_box_buffer(min_width: Option<i32>) -> Vec<u8> {
    use crate::abi_constants::{OP_SET_CENTER, OP_SET_MIN_SIZE, OP_TEXT_INPUT};
    let placeholder = "type a number, press Enter";
    let mut b = Buf::new()
        .div()
        .op(OP_SET_FLEX)
        .u8(1) // column
        .op(OP_SET_CENTER)
        .op(OP_SET_GAP)
        .f32(28.0)
        .op(OP_SET_PADDING)
        .f32(32.0)
        .div()
        .key("frame");
    if let Some(w) = min_width {
        b = b.op(OP_SET_MIN_SIZE).u32(w as u32).u32(-1i32 as u32); // height auto
    }
    b.op(OP_SET_BORDER)
        .f32(1.0)
        .u8(120)
        .u8(120)
        .u8(140)
        .op(OP_SET_ROUNDED)
        .f32(6.0)
        .op(OP_SET_PADDING)
        .f32(8.0)
        .op(OP_TEXT_INPUT)
        .u32(1) // input_id
        .u32(placeholder.len() as u32)
        .bytes(placeholder.as_bytes())
        .add_child() // leaf -> frame
        .add_child() // frame -> root
        .root()
        .finish()
}

/// The behavioral half of the sizing story: a click has to land on the widget
/// and typed characters have to end up in its buffer instead of falling
/// through to the app's window-level key handler. Needs the dispatch recorder,
/// hence the feature gate.
#[cfg(feature = "test-dispatch-stub")]
mod text_input_interaction {
    use super::prompt_box_buffer;
    use crate::headless::with_rendered_tree;
    use crate::{EVENT_TEXT, gpui_input_text_len, take_recorded_dispatches};
    use gpui::{Modifiers, TestAppContext, point, px};
    use std::sync::MutexGuard;

    /// Where a user aims: inside the 360px-wide box (x 780..1140) but off its
    /// center line. The collapsed frame is 18px wide around x = 960, so the
    /// same click misses it — and gpui's `Bounds::contains` is inclusive on
    /// both edges, so only a pixel-exact hit on the center line would land on
    /// a zero-width hitbox anyway.
    const CLICK: (f32, f32) = (1060.0, 540.0);

    /// Take the process-global test locks this suite needs, in the documented
    /// order (`INJECT_TEST_LOCK` → `INPUT_TEST_LOCK`; `with_rendered_tree`
    /// takes `TEST_VIEWS_MUTEX` last), install a fresh dispatch recorder, and
    /// clear the input mirror.
    ///
    /// `VIEWS` is deliberately NOT touched here: it belongs to
    /// `TEST_VIEWS_MUTEX`, which is only held inside `with_rendered_tree`.
    /// Clearing it from out here wipes whatever tree a concurrently running
    /// headless test committed, between its commit and its render — which
    /// shows up as an unrelated `no element with debug selector …` panic.
    fn setup() -> (MutexGuard<'static, ()>, MutexGuard<'static, ()>, crate::RecorderGuard) {
        let inject = crate::INJECT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let input = crate::INPUT_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let recorder = crate::install_dispatch_recorder();
        crate::set_dispatch_changed(0);
        *crate::INPUT_MIRROR.lock().unwrap_or_else(|e| e.into_inner()) = None;
        (inject, input, recorder)
    }

    /// With a definite frame width the click hits the widget, focus lands on
    /// it, and "42" goes into the text model — never to the app as
    /// `EVENT_TEXT` (which is what the demo's `on_text` handler would have
    /// parsed into the counter).
    #[gpui::test]
    fn click_focuses_the_input_and_typed_text_stays_in_the_widget(cx: &mut TestAppContext) {
        let _guards = setup();

        with_rendered_tree(cx, &prompt_box_buffer(Some(360)), |vcx| {
            vcx.simulate_click(point(px(CLICK.0), px(CLICK.1)), Modifiers::none());
            vcx.simulate_input("42");
        })
        .expect("decode");

        assert_eq!(gpui_input_text_len(0, 1), 2, "typed text must reach the model");
        let text_events = take_recorded_dispatches()
            .into_iter()
            .filter(|e| e.kind == EVENT_TEXT)
            .count();
        assert_eq!(text_events, 0, "a focused input must swallow typed keys");
    }

    /// The reported bug: with a collapsed frame the click misses the zero-wide
    /// hitbox, so nothing is focused and every keystroke is delivered to the
    /// app as `EVENT_TEXT` — the demo's counter ate the input.
    #[gpui::test]
    fn collapsed_input_cannot_be_focused_and_leaks_keys_to_the_app(cx: &mut TestAppContext) {
        let _guards = setup();

        with_rendered_tree(cx, &prompt_box_buffer(None), |vcx| {
            vcx.simulate_click(point(px(CLICK.0), px(CLICK.1)), Modifiers::none());
            vcx.simulate_input("42");
        })
        .expect("decode");

        assert_eq!(gpui_input_text_len(0, 1), 0, "nothing can reach the model");
        let text_events = take_recorded_dispatches()
            .into_iter()
            .filter(|e| e.kind == EVENT_TEXT)
            .count();
        assert_eq!(text_events, 2, "both keys leaked to the app");
    }
}

// ---------------------------------------------------------------------
// OP_SET_TEXT_ROW — the metrics mirror + painted caret.
//
// NoopTextSystem determinism (see module header): glyph advance 600/1000·size,
// so at 20px each character — including "あ" — occupies exactly 12px.
// ---------------------------------------------------------------------

/// Render a row-declared text node and pull its mirrored layout through both
/// metric exports; also assert the caret rect reached the IME anchor key
/// (`PROBE_BOUNDS["caret"]`). Character indices are MoonBit's unit, so the
/// multibyte content pins the char↔byte conversion inside the exports.
#[gpui::test]
fn text_row_metrics_and_caret_roundtrip(cx: &mut TestAppContext) {
    let content = "aあbc"; // bytes: a=0, あ=1..4, b=4, c=5; len 6
    let buf = Buf::new()
        .div()
        // The adapter emits this as the root's first child every frame;
        // exercising it here pins that the mirrors survive the clear.
        .div()
        .key("probe:clear")
        .add_child()
        .div()
        .key("row")
        .op(OP_TEXT)
        .u32(content.len() as u32)
        .bytes(content.as_bytes())
        .u8(0)
        .u8(0)
        .u8(0)
        .f32(20.0)
        // Caret at char 2 ("b" → byte 4), static (blink = 0) for determinism.
        .op(OP_SET_TEXT_ROW)
        .u32(3)
        .bytes(b"row")
        .u8(1)
        .u32(4)
        .u8(10)
        .u8(20)
        .u8(30)
        .u8(0)
        .add_child() // text -> row
        .add_child() // row -> root
        .root()
        .finish();
    layout_bound(cx, &buf, "row").expect("decode");

    let key = b"row";
    let x_for_char = |idx: i32| -> Option<(f32, f32)> {
        let mut out = [0u8; 8];
        if crate::gpui_text_x_for_char(key.as_ptr(), key.len() as i32, idx, out.as_mut_ptr()) != 0
        {
            return None;
        }
        let quarter = |i: usize| -> f32 {
            i32::from_le_bytes(out[i * 4..i * 4 + 4].try_into().unwrap()) as f32 / 4.0
        };
        Some((quarter(0), quarter(1)))
    };
    let (x0, y0) = x_for_char(0).expect("char 0");
    let (x1, _) = x_for_char(1).expect("char 1 (multibyte start)");
    let (x2, y2) = x_for_char(2).expect("char 2");
    let (x3, _) = x_for_char(3).expect("char 3");
    let (x4, _) = x_for_char(4).expect("char count == row-end insertion point");
    assert_eq!(x_for_char(5), None, "char past the end must miss");
    assert_eq!(
        crate::gpui_text_x_for_char(
            b"never-painted".as_ptr(),
            13,
            0,
            [0u8; 8].as_mut_ptr()
        ),
        -1,
        "unknown key must miss"
    );
    for (label, a, b) in [("0→1", x0, x1), ("1→2", x1, x2), ("2→3", x2, x3), ("3→end", x3, x4)] {
        assert!(
            (b - a - 12.0).abs() < 0.26,
            "{label}: expected one 12px glyph, got {:.3}",
            b - a
        );
    }
    assert_eq!(y0, y2, "single wrapped line shares y");

    let char_at = |x: f32, y: f32| -> Option<i32> {
        let mut out = [0u8; 4];
        let status = crate::gpui_text_char_for_position(
            key.as_ptr(),
            key.len() as i32,
            (x * 4.0).round() as i32,
            (y * 4.0).round() as i32,
            out.as_mut_ptr(),
        );
        (status == 0).then(|| i32::from_le_bytes(out))
    };
    // Mid-glyph clicks land on the exact character (the whole point:
    // proportional fonts must not fall back to linear interpolation).
    assert_eq!(char_at(x0 + 6.0, y0), Some(0));
    assert_eq!(char_at(x1 + 6.0, y0), Some(1));
    assert_eq!(char_at(x2 + 6.0, y2), Some(2));
    // gpui's nearest-line semantics: far right clamps to the row end, far
    // left / above land at 0.
    assert_eq!(char_at(x4 + 200.0, y0), Some(4));
    assert_eq!(char_at(x0 - 500.0, y0), Some(0));
    assert_eq!(char_at(x2, y0 - 500.0), Some(0));

    // The caret rect rode into the IME anchor key during prepaint — the
    // exact position `position_for_index(char 2)` produced.
    let mut probe = [0u8; 16];
    assert_eq!(
        crate::gpui_probe_rect(b"caret".as_ptr(), 5, probe.as_mut_ptr()),
        0,
        "the caret must publish its rect under the IME anchor key"
    );
    let caret_x = i32::from_le_bytes(probe[0..4].try_into().unwrap()) as f32;
    let caret_w = i32::from_le_bytes(probe[8..12].try_into().unwrap()) as f32;
    assert!(
        (caret_x - x2).abs() < 1.0,
        "caret x {caret_x} must sit at char 2's x {x2}"
    );
    assert_eq!(caret_w, 2.0, "caret is the 2px bar");
}
