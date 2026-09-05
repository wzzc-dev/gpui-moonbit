//! G25 decoder fuzzing (stable-compatible).
//!
//! A deterministic, seeded in-crate fuzz harness: a tiny xorshift64* PRNG
//! generates (a) raw random byte buffers and (b) structurally plausible
//! command buffers (valid header, real opcodes with random operands, random
//! truncation/corruption), all fed through the real decoder. The decoder must
//! never panic and must only ever return a `GPUI_STATUS_*` code.
//!
//! Deterministic by construction: fixed seed, no threads, no time — a failure
//! reproduces byte-for-byte on any machine. The nightly `cargo-fuzz` scaffold
//! under `gpui-sys/fuzz/` complements this with coverage-guided mutation; it
//! is a separate crate and never builds as part of the main workspace.

use crate::abi_constants::{
    BUFFER_VERSION, OP_ADD_CHILD, OP_DIV, OP_SET_ALIGN, OP_SET_BG, OP_SET_BG_COLOR, OP_SET_BORDER,
    OP_SET_CENTER, OP_SET_CURSOR, OP_SET_FLEX, OP_SET_FLEX_ITEM, OP_SET_FONT_FAMILY,
    OP_SET_FONT_WEIGHT, OP_SET_GAP, OP_SET_INSET, OP_SET_KEY, OP_SET_LINE_HEIGHT, OP_SET_MARGIN,
    OP_SET_MAX_SIZE, OP_SET_MIN_SIZE, OP_SET_ON_CLICK, OP_SET_OPACITY, OP_SET_OVERFLOW,
    OP_SET_PADDING, OP_SET_PADDING_SIDES, OP_SET_POSITION, OP_SET_ROOT, OP_SET_ROUNDED,
    OP_SET_SHADOW, OP_SET_SIZE, OP_SET_TEXT_ALIGN, OP_SET_TEXT_COLOR, OP_SET_TEXT_ROW,
    OP_SET_TEXT_SIZE, OP_SET_WHITESPACE, OP_TEXT, OP_TEXT_RUN,
};
use crate::{
    GPUI_STATUS_BAD_BUFFER_VERSION, GPUI_STATUS_DEPTH_EXCEEDED, GPUI_STATUS_DUPLICATE_KEY,
    GPUI_STATUS_INTERNAL_PANIC, GPUI_STATUS_INVALID_FLOAT, GPUI_STATUS_INVALID_HANDLE,
    GPUI_STATUS_INVALID_TEXT_RUN, GPUI_STATUS_NO_ROOT, GPUI_STATUS_NODE_ABSENT, GPUI_STATUS_OK,
    GPUI_STATUS_TRUNCATED_BUFFER, GPUI_STATUS_UNKNOWN_OPCODE, GPUI_STATUS_WRONG_NODE_KIND,
    MAX_TREE_DEPTH, build_tree_from_buffer,
};
use gpui::TestAppContext;

/// The full set of statuses the decoder may legally return.
const LEGAL_STATUSES: [i32; 13] = [
    GPUI_STATUS_OK,
    GPUI_STATUS_INVALID_HANDLE,
    GPUI_STATUS_WRONG_NODE_KIND,
    GPUI_STATUS_NODE_ABSENT,
    GPUI_STATUS_INTERNAL_PANIC,
    GPUI_STATUS_BAD_BUFFER_VERSION,
    GPUI_STATUS_TRUNCATED_BUFFER,
    GPUI_STATUS_UNKNOWN_OPCODE,
    GPUI_STATUS_NO_ROOT,
    GPUI_STATUS_DUPLICATE_KEY,
    // Random operand bytes hit the non-finite f32 exponent often (~1 in 256 per
    // f32), so this is a routine outcome here, not an exotic one (issue #75).
    GPUI_STATUS_INVALID_FLOAT,
    // Reachable from any generator that nests past MAX_TREE_DEPTH (issue #74).
    GPUI_STATUS_DEPTH_EXCEEDED,
    // Random OP_TEXT_RUN operands rarely satisfy the range/boundary/flag
    // validation, so rejection is the expected fuzz outcome (issue #91).
    GPUI_STATUS_INVALID_TEXT_RUN,
];

/// xorshift64* — tiny, fast, deterministic. Plenty of statistical quality for
/// fuzz input generation; the seed makes every run reproducible.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // xorshift64 must not be seeded with zero.
        Rng(seed | 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % n as u64) as u32
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_u64() as u8).collect()
    }
}

/// Every real opcode, so the structured generator exercises all decode arms.
const OPCODES: [i32; 36] = [
    OP_DIV,
    OP_TEXT,
    OP_TEXT_RUN,
    OP_SET_SIZE,
    OP_SET_BG,
    OP_SET_FLEX,
    OP_SET_CENTER,
    OP_SET_GAP,
    OP_SET_ROUNDED,
    OP_SET_ON_CLICK,
    OP_SET_KEY,
    OP_SET_PADDING,
    OP_SET_BORDER,
    OP_SET_BG_COLOR,
    OP_SET_MARGIN,
    OP_SET_MIN_SIZE,
    OP_SET_MAX_SIZE,
    OP_SET_FLEX_ITEM,
    OP_SET_ALIGN,
    OP_SET_OVERFLOW,
    OP_SET_OPACITY,
    OP_SET_SHADOW,
    OP_SET_CURSOR,
    OP_SET_POSITION,
    OP_SET_INSET,
    OP_SET_PADDING_SIDES,
    OP_SET_TEXT_SIZE,
    OP_SET_TEXT_COLOR,
    OP_SET_FONT_WEIGHT,
    OP_SET_LINE_HEIGHT,
    OP_SET_TEXT_ALIGN,
    OP_SET_WHITESPACE,
    OP_SET_FONT_FAMILY,
    OP_SET_TEXT_ROW,
    OP_ADD_CHILD,
    OP_SET_ROOT,
];

/// Feed `data` through the real decoder, asserting it never panics and only
/// returns a `GPUI_STATUS_*` code. Uses a private `VIEWS` slot (9999) so
/// successful decodes never clobber the golden tests' slot 0.
fn decode_never_panics(data: &[u8]) -> i32 {
    let status = build_tree_from_buffer(9999, data);
    assert!(
        LEGAL_STATUSES.contains(&status),
        "decoder returned out-of-range status {status} for {}-byte buffer",
        data.len()
    );
    status
}

/// Append a random operand stream for `opcode`, sized from its wire layout
/// (see the wire-format table on `build_tree_from_buffer`). Length-prefixed
/// payloads (`OP_TEXT`, `OP_SET_KEY`, `OP_SET_FONT_FAMILY`) get a random
/// length followed by that many bytes — the decoder must handle any length.
fn emit_operands(rng: &mut Rng, buf: &mut Vec<u8>, opcode: i32) {
    let fixed: u32 = match opcode {
        OP_DIV | OP_SET_CENTER | OP_ADD_CHILD | OP_SET_ROOT => 0,
        OP_SET_FLEX => 1,
        OP_SET_BG => 3,
        OP_SET_GAP | OP_SET_ROUNDED | OP_SET_PADDING | OP_SET_ON_CLICK | OP_SET_OPACITY
        | OP_SET_CURSOR | OP_SET_POSITION | OP_SET_FONT_WEIGHT | OP_SET_LINE_HEIGHT
        | OP_SET_TEXT_ALIGN | OP_SET_WHITESPACE | OP_SET_TEXT_SIZE | OP_SET_BG_COLOR
        | OP_SET_TEXT_COLOR => 4,
        OP_SET_BORDER => 7,
        OP_SET_SIZE | OP_SET_MIN_SIZE | OP_SET_MAX_SIZE | OP_SET_ALIGN | OP_SET_OVERFLOW => 8,
        OP_SET_FLEX_ITEM => 12,
        OP_SET_MARGIN | OP_SET_INSET | OP_SET_PADDING_SIDES => 16,
        OP_SET_SHADOW => 20,
        // start u32 + len u32 + flags u8 + rgba + weight i32 + rgba (#91).
        // Random ranges rarely validate, so this mostly exercises the
        // INVALID_TEXT_RUN rejection path — which is the point.
        OP_TEXT_RUN => 21,
        // Length-prefixed: u32 len + len bytes.
        OP_TEXT | OP_SET_KEY | OP_SET_FONT_FAMILY => {
            let len = rng.below(64);
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(&rng.bytes(len as usize));
            return;
        }
        // key_len u32 + key bytes + has_caret u8 + byte_offset u32 + rgb + blink u8.
        // Random operands almost never validate against a real text node, so
        // this mostly exercises the INVALID_TEXT_RUN rejection path.
        OP_SET_TEXT_ROW => {
            let len = rng.below(64);
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(&rng.bytes(len as usize));
            buf.extend_from_slice(&rng.bytes(9));
            return;
        }
        _ => unreachable!("OPCODES is exhaustive"),
    };
    buf.extend_from_slice(&rng.bytes(fixed as usize));
}

/// Build a structurally plausible buffer: valid header, a random program of
/// real opcodes with correctly-sized operands, then optionally truncate or
/// corrupt it so the decoder's error paths get exercised too.
fn plausible_buffer(rng: &mut Rng) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"GPUI");
    buf.extend_from_slice(&(BUFFER_VERSION as u32).to_le_bytes());

    let ops = 1 + rng.below(40);
    for _ in 0..ops {
        let opcode = OPCODES[rng.below(OPCODES.len() as u32) as usize];
        buf.push(opcode as u8);
        emit_operands(rng, &mut buf, opcode);
    }

    match rng.below(4) {
        // Truncate mid-stream (exercises every reader's `None` arm).
        0 if buf.len() > 1 => buf.truncate(1 + rng.below((buf.len() - 1) as u32) as usize),
        // Flip random bytes (exercises mid-field corruption).
        1 => {
            let flips = 1 + rng.below(4);
            for _ in 0..flips {
                let idx = rng.below(buf.len() as u32) as usize;
                buf[idx] ^= 0xFF;
            }
        }
        _ => {}
    }
    buf
}

/// 10,000 raw random buffers of varying length: the decoder must reject or
/// accept every one without panicking, returning only `GPUI_STATUS_*` codes.
#[::core::prelude::v1::test]
fn fuzz_random_bytes_never_panic() {
    let _guard = crate::TEST_VIEWS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let mut rng = Rng::new(0x5EED_0001);
    for _ in 0..10_000 {
        let len = rng.below(512) as usize;
        let data = rng.bytes(len);
        decode_never_panics(&data);
    }
}

/// 10,000 structurally plausible buffers drive the decoder deep into every
/// opcode arm (valid header, real opcodes, correctly-sized operands) plus
/// their truncated/corrupted variants.
#[::core::prelude::v1::test]
fn fuzz_plausible_buffers_never_panic() {
    let _guard = crate::TEST_VIEWS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let mut rng = Rng::new(0x5EED_0002);
    let mut ok_count = 0usize;
    for _ in 0..10_000 {
        let data = plausible_buffer(&mut rng);
        if decode_never_panics(&data) == GPUI_STATUS_OK {
            ok_count += 1;
        }
    }
    // Sanity: the generator must actually produce decodable trees sometimes,
    // or the "deep path" claim is hollow. (A random program needs a SET_ROOT
    // over a live node with unique keys — rare but not vanishing.)
    assert!(
        ok_count > 0,
        "structured generator produced no decodable buffers; check operand sizing"
    );
}

/// Edge cases the random generators might miss: empty, header-only, every
/// single opcode alone, and a maximally nested valid tree.
#[::core::prelude::v1::test]
fn fuzz_edge_cases_never_panic() {
    let _guard = crate::TEST_VIEWS_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    decode_never_panics(&[]);
    decode_never_panics(b"GPUI");
    let mut header = b"GPUI".to_vec();
    header.extend_from_slice(&(BUFFER_VERSION as u32).to_le_bytes());
    decode_never_panics(&header);

    // Each opcode alone, with zero operands (truncated operand stream).
    for &opcode in &OPCODES {
        let mut buf = header.clone();
        buf.push(opcode as u8);
        decode_never_panics(&buf);
    }

    // A deep chain: N nested divs, each the child of the previous, root set.
    // Decoding itself is an iterative loop and handles any depth; the cap is
    // applied at commit, so these cases pin the boundary from both sides
    // (issue #74). One level over the limit must come back as a status — the
    // failure it replaces is a stack overflow inside a walker, which aborts
    // the process and cannot be caught.
    let nested_chain = |depth: usize| {
        let mut buf = header.clone();
        for _ in 0..depth {
            buf.push(OP_DIV as u8);
        }
        for _ in 1..depth {
            buf.push(OP_ADD_CHILD as u8);
        }
        buf.push(OP_SET_ROOT as u8);
        buf
    };
    for depth in [1usize, 8, MAX_TREE_DEPTH as usize - 1, MAX_TREE_DEPTH as usize] {
        assert_eq!(
            decode_never_panics(&nested_chain(depth)),
            GPUI_STATUS_OK,
            "nested chain of depth {depth} is at or under MAX_TREE_DEPTH and must commit"
        );
    }
    for depth in [MAX_TREE_DEPTH as usize + 1, 512, 4096] {
        assert_eq!(
            decode_never_panics(&nested_chain(depth)),
            GPUI_STATUS_DEPTH_EXCEEDED,
            "nested chain of depth {depth} exceeds MAX_TREE_DEPTH and must be rejected"
        );
    }
}

/// Style opcodes that apply to the div on top of the stack. Structural opcodes
/// (`OP_DIV`, `OP_TEXT`, `OP_ADD_CHILD`, `OP_SET_ROOT`) are driven explicitly by
/// [`renderable_buffer`], and `OP_SET_KEY` is left out so random key bytes can
/// never collide into `DUPLICATE_KEY`.
const STYLE_OPCODES: [i32; 29] = [
    OP_SET_SIZE,
    OP_SET_BG,
    OP_SET_FLEX,
    OP_SET_CENTER,
    OP_SET_GAP,
    OP_SET_ROUNDED,
    OP_SET_ON_CLICK,
    OP_SET_PADDING,
    OP_SET_BORDER,
    OP_SET_BG_COLOR,
    OP_SET_MARGIN,
    OP_SET_MIN_SIZE,
    OP_SET_MAX_SIZE,
    OP_SET_FLEX_ITEM,
    OP_SET_ALIGN,
    OP_SET_OVERFLOW,
    OP_SET_OPACITY,
    OP_SET_SHADOW,
    OP_SET_CURSOR,
    OP_SET_POSITION,
    OP_SET_INSET,
    OP_SET_PADDING_SIDES,
    OP_SET_TEXT_SIZE,
    OP_SET_TEXT_COLOR,
    OP_SET_FONT_WEIGHT,
    OP_SET_LINE_HEIGHT,
    OP_SET_TEXT_ALIGN,
    OP_SET_WHITESPACE,
    OP_SET_FONT_FAMILY,
];

/// Append 0–5 random style opcodes to whatever div is on top of the stack.
fn emit_random_styles(rng: &mut Rng, buf: &mut Vec<u8>) {
    for _ in 0..rng.below(6) {
        let opcode = STYLE_OPCODES[rng.below(STYLE_OPCODES.len() as u32) as usize];
        buf.push(opcode as u8);
        emit_operands(rng, buf, opcode);
    }
}

/// Build a buffer whose *framing* is always valid, so the tree commits and the
/// render path actually runs; only the operand **values** are random.
///
/// This is deliberately not [`plausible_buffer`]. That generator picks opcodes
/// uniformly, including structural ones, so its programs almost never satisfy
/// "a live node designated as root": measured, 1 buffer in 500 committed. It is
/// the right shape for hammering the decoder's error arms and the wrong shape
/// for reaching layout at all.
///
/// The stack discipline here is the one documented on `build_tree_from_buffer`:
/// `OP_DIV` pushes, `OP_ADD_CHILD` pops the child and re-pushes the parent,
/// `OP_SET_ROOT` pops the root. Text children use ASCII payloads — `read_string`
/// is lossy rather than rejecting, but a random byte soup would exercise
/// replacement-character handling instead of text layout.
fn renderable_buffer(rng: &mut Rng) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"GPUI");
    buf.extend_from_slice(&(BUFFER_VERSION as u32).to_le_bytes());

    buf.push(OP_DIV as u8);
    emit_random_styles(rng, &mut buf);

    for _ in 0..rng.below(5) {
        if rng.below(4) == 0 {
            // Text leaf: len u32 | utf8[len] | r u8 | g u8 | b u8 | size f32.
            let len = rng.below(12);
            buf.push(OP_TEXT as u8);
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend((0..len).map(|_| b'a' + (rng.below(26) as u8)));
            buf.extend_from_slice(&rng.bytes(3));
            buf.extend_from_slice(&rng.bytes(4));
        } else {
            buf.push(OP_DIV as u8);
            emit_random_styles(rng, &mut buf);
        }
        buf.push(OP_ADD_CHILD as u8);
    }

    buf.push(OP_SET_ROOT as u8);
    buf
}

/// Carry generated buffers all the way through `render_node` and taffy instead
/// of stopping at the decoder (issue #75).
///
/// The three tests above only prove the *decoder* is total. Everything the
/// decoder accepts then flows into style mapping and layout, which is where
/// operand values (rather than operand framing) start to matter — that is the
/// gap that let non-finite `f32` reach taffy unnoticed. A decoded buffer that
/// lays out to infinite or `NaN` geometry is not a decoder bug, and no
/// decoder-level assertion can see it.
///
/// Fewer iterations than the decoder fuzzers: each one builds and tears down a
/// headless window, so this trades raw volume for depth of path coverage.
/// Deterministic all the same — its own fixed seed, no threads, no time.
///
/// The floor on `rendered` is the point of the test, not a formality: with the
/// uniform generator this same loop reached layout once in 500 tries while
/// still passing a `> 0` check, i.e. it would have reported coverage it did not
/// have. Rejections that remain are almost all `INVALID_FLOAT` — random operand
/// bytes land on the non-finite exponent roughly 1 time in 256 per `f32`.
///
/// Note there is no explicit `TEST_VIEWS_MUTEX` guard here: `layout_bounds`
/// takes that lock itself for each call, and the lock is not reentrant.
#[gpui::test]
fn fuzz_renderable_buffers_render_never_panic(cx: &mut TestAppContext) {
    const ROUNDS: usize = 300;
    let mut rng = Rng::new(0x5EED_0004);
    let mut rendered = 0usize;
    for _ in 0..ROUNDS {
        let data = renderable_buffer(&mut rng);
        match crate::headless::layout_bounds(cx, &data, &[]) {
            Ok(_) => rendered += 1,
            Err(status) => assert!(
                LEGAL_STATUSES.contains(&status),
                "render harness returned out-of-range status {status} for {}-byte buffer",
                data.len()
            ),
        }
    }
    assert!(
        rendered * 2 > ROUNDS,
        "only {rendered}/{ROUNDS} generated buffers reached layout; the generator \
         stopped producing committable trees, so this test is no longer covering render"
    );
}
