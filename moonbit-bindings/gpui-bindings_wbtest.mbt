// Whitebox tests run within the package scope.
// Use them to validate internal helpers or invariants.
// Keep them focused; public behavior belongs in `_test.mbt`.

///|
test "clamp_color_component: clamps to the byte range" {
  assert_eq(clamp_color_component(-1), 0)
  assert_eq(clamp_color_component(0), 0)
  assert_eq(clamp_color_component(128), 128)
  assert_eq(clamp_color_component(255), 255)
  assert_eq(clamp_color_component(300), 255)
}

///|
test "to_utf8_bytes: ascii is full utf8 without a terminator" {
  let b = to_utf8_bytes("GPUI + MoonBit")
  assert_eq(b.length(), 14)
  assert_eq(b[0], b'G')
  assert_eq(b[13], b't')
  for i in 0..<14 {
    assert_not_eq(b[i], b'\x00')
  }
}

///|
test "to_utf8_bytes: multibyte is UTF-8 encoded" {
  // "あ" is U+3042 -> UTF-8 0xE3 0x81 0x82.
  let b = to_utf8_bytes("あ")
  assert_eq(b.length(), 3)
  assert_eq(b[0], b'\xE3')
  assert_eq(b[1], b'\x81')
  assert_eq(b[2], b'\x82')
}

///|
test "to_utf8_bytes: empty string is empty" {
  let b = to_utf8_bytes("")
  assert_eq(b.length(), 0)
}

///|
fn read_u32_le(b : Bytes, offset : Int) -> Int {
  b[offset].to_int() +
  b[offset + 1].to_int() * 256 +
  b[offset + 2].to_int() * 65536 +
  b[offset + 3].to_int() * 16777216
}

///|
test "to_utf8_bytes: embedded NUL is data, not a terminator" {
  let b = to_utf8_bytes("A\u{0}B")
  assert_eq(b.length(), 3)
  assert_eq(b[0], b'A')
  assert_eq(b[1], b'\x00')
  assert_eq(b[2], b'B')
}

///|
test "command buffer wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.text("あ", 4, 5, 6, 14.0)
  cb.set_root()
  let b = cb.to_bytes()
  // Header: magic "GPUI" + BUFFER_VERSION (u32 LE).
  assert_eq(b[0], b'G')
  assert_eq(b[1], b'P')
  assert_eq(b[2], b'U')
  assert_eq(b[3], b'I')
  assert_eq(read_u32_le(b, 4), BUFFER_VERSION)
  // OP_TEXT: opcode, u32 length, UTF-8 payload, RGB, f32 size (LE).
  assert_eq(b[8].to_int(), OP_TEXT)
  assert_eq(read_u32_le(b, 9), 3) // "あ" = E3 81 82
  assert_eq(b[13], b'\xE3')
  assert_eq(b[14], b'\x81')
  assert_eq(b[15], b'\x82')
  assert_eq(b[16], b'\x04')
  assert_eq(b[17], b'\x05')
  assert_eq(b[18], b'\x06')
  // 14.0f32 = 0x41600000 -> little-endian 00 00 60 41.
  assert_eq(b[19], b'\x00')
  assert_eq(b[20], b'\x00')
  assert_eq(b[21], b'\x60')
  assert_eq(b[22], b'\x41')
  assert_eq(b[23].to_int(), OP_SET_ROOT)
  assert_eq(b.length(), 24)
}

///|
test "command buffer text carries embedded NUL as payload data" {
  let cb = CommandBuffer::new()
  cb.text("A\u{0}B", 0, 0, 0, 1.0)
  cb.set_root()
  let b = cb.to_bytes()
  assert_eq(b[8].to_int(), OP_TEXT)
  assert_eq(read_u32_le(b, 9), 3)
  assert_eq(b[13], b'A')
  assert_eq(b[14], b'\x00')
  assert_eq(b[15], b'B')
}

///|
test "command buffer set_padding wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_padding(4.0)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_PADDING | f32 padding (LE).
  assert_eq(b[8].to_int(), OP_SET_PADDING)
  // 4.0f32 = 0x40800000 -> little-endian 00 00 80 40.
  assert_eq(b[9], b'\x00')
  assert_eq(b[10], b'\x00')
  assert_eq(b[11], b'\x80')
  assert_eq(b[12], b'\x40')
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_border wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_border(2.0, 10, 20, 30)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_BORDER | f32 width | r u8 | g u8 | b u8.
  assert_eq(b[8].to_int(), OP_SET_BORDER)
  // 2.0f32 = 0x40000000 -> little-endian 00 00 00 40.
  assert_eq(b[9], b'\x00')
  assert_eq(b[10], b'\x00')
  assert_eq(b[11], b'\x00')
  assert_eq(b[12], b'\x40')
  assert_eq(b[13], b'\x0A')
  assert_eq(b[14], b'\x14')
  assert_eq(b[15], b'\x1E')
  assert_eq(b.length(), 16)
}

///|
test "set_border_color emits the same wire format as set_border" {
  let a = CommandBuffer::new()
  a.set_border(2.0, 10, 20, 30)
  let b = CommandBuffer::new()
  b.set_border_color(2.0, Color::rgb(10, 20, 30))
  assert_eq(b.to_bytes(), a.to_bytes())
}

///|
test "command buffer set_position wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_position(POSITION_ABSOLUTE)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_POSITION | mode i32.
  assert_eq(b[8].to_int(), OP_SET_POSITION)
  assert_eq(read_i32_le(b, 9), POSITION_ABSOLUTE)
  assert_eq(b.length(), 13)
}

///|
fn read_i32_le(b : Bytes, offset : Int) -> Int {
  // MoonBit's Int is a 32-bit wrapping two's-complement word, so the four
  // little-endian bytes assembled by read_u32_le already carry the correct
  // signed i32 value (see the "Int wraps at the i32 boundary" test below).
  read_u32_le(b, offset)
}

///|
test "command buffer set_bg_color wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_bg_color(Color::rgba(10, 20, 30, 128))
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_BG_COLOR | r u8 | g u8 | b u8 | a u8.
  assert_eq(b[8].to_int(), OP_SET_BG_COLOR)
  assert_eq(b[9], b'\x0A')
  assert_eq(b[10], b'\x14')
  assert_eq(b[11], b'\x1E')
  assert_eq(b[12], b'\x80')
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_margin wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_margin(1, 2, 3, 4)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_MARGIN | top i32 | right i32 | bottom i32 | left i32.
  assert_eq(b[8].to_int(), OP_SET_MARGIN)
  assert_eq(read_i32_le(b, 9), 1)
  assert_eq(read_i32_le(b, 13), 2)
  assert_eq(read_i32_le(b, 17), 3)
  assert_eq(read_i32_le(b, 21), 4)
  assert_eq(b.length(), 25)
}

///|
test "command buffer set_flex wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_flex(1.5, 0.5, 100.0)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_FLEX_ITEM | grow_x1000 i32 | shrink_x1000 i32 | basis_px i32.
  assert_eq(b[8].to_int(), OP_SET_FLEX_ITEM)
  assert_eq(read_i32_le(b, 9), 1500) // 1.5 * 1000
  assert_eq(read_i32_le(b, 13), 500) // 0.5 * 1000
  assert_eq(read_i32_le(b, 17), 100) // basis px
  assert_eq(b.length(), 21)
}

///|
test "command buffer set_opacity wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_opacity(0.5)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_OPACITY | x1000 i32.
  assert_eq(b[8].to_int(), OP_SET_OPACITY)
  assert_eq(read_i32_le(b, 9), 500) // 0.5 * 1000
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_shadow wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_shadow(0, 4, 6, -1, Color::rgba(0, 0, 0, 64))
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_SHADOW | x i32 | y i32 | blur i32 | spread i32 | r u8 | g u8 | b u8 | a u8.
  assert_eq(b[8].to_int(), OP_SET_SHADOW)
  assert_eq(read_i32_le(b, 9), 0)
  assert_eq(read_i32_le(b, 13), 4)
  assert_eq(read_i32_le(b, 17), 6)
  assert_eq(read_i32_le(b, 21), -1)
  assert_eq(b[25], b'\x00')
  assert_eq(b[26], b'\x00')
  assert_eq(b[27], b'\x00')
  assert_eq(b[28], b'\x40')
  assert_eq(b.length(), 29)
}

///|
test "command buffer set_align wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_align(ALIGN_CENTER, JUSTIFY_SPACE_BETWEEN)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_ALIGN | align_items i32 | justify_content i32.
  assert_eq(b[8].to_int(), OP_SET_ALIGN)
  assert_eq(read_i32_le(b, 9), ALIGN_CENTER)
  assert_eq(read_i32_le(b, 13), JUSTIFY_SPACE_BETWEEN)
  assert_eq(b.length(), 17)
}

///|
test "command buffer set_overflow wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_overflow(OVERFLOW_HIDDEN, OVERFLOW_SCROLL)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_OVERFLOW | x_mode i32 | y_mode i32.
  assert_eq(b[8].to_int(), OP_SET_OVERFLOW)
  assert_eq(read_i32_le(b, 9), OVERFLOW_HIDDEN)
  assert_eq(read_i32_le(b, 13), OVERFLOW_SCROLL)
  assert_eq(b.length(), 17)
}

///|
test "command buffer set_cursor wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_cursor(CURSOR_POINTER)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_CURSOR | kind i32.
  assert_eq(b[8].to_int(), OP_SET_CURSOR)
  assert_eq(read_i32_le(b, 9), CURSOR_POINTER)
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_inset wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_inset(10, -1, 20, -1)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_INSET | top i32 | right i32 | bottom i32 | left i32.
  assert_eq(b[8].to_int(), OP_SET_INSET)
  assert_eq(read_i32_le(b, 9), 10)
  assert_eq(read_i32_le(b, 13), -1)
  assert_eq(read_i32_le(b, 17), 20)
  assert_eq(read_i32_le(b, 21), -1)
  assert_eq(b.length(), 25)
}

///|
test "command buffer set_min_size wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_min_size(100, -1)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_MIN_SIZE | w i32 | h i32.
  assert_eq(b[8].to_int(), OP_SET_MIN_SIZE)
  assert_eq(read_i32_le(b, 9), 100)
  assert_eq(read_i32_le(b, 13), -1)
  assert_eq(b.length(), 17)
}

///|
test "command buffer set_max_size wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_max_size(-1, 400)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_MAX_SIZE | w i32 | h i32.
  assert_eq(b[8].to_int(), OP_SET_MAX_SIZE)
  assert_eq(read_i32_le(b, 9), -1)
  assert_eq(read_i32_le(b, 13), 400)
  assert_eq(b.length(), 17)
}

///|
test "command buffer set_padding_sides wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_padding_sides(5, 6, 7, 8)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_PADDING_SIDES | top i32 | right i32 | bottom i32 | left i32.
  assert_eq(b[8].to_int(), OP_SET_PADDING_SIDES)
  assert_eq(read_i32_le(b, 9), 5)
  assert_eq(read_i32_le(b, 13), 6)
  assert_eq(read_i32_le(b, 17), 7)
  assert_eq(read_i32_le(b, 21), 8)
  assert_eq(b.length(), 25)
}

// --- G8 typography wire format (issue #51) ---------------------------------

///|
test "command buffer set_text_size wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_text_size(18)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_TEXT_SIZE | size i32.
  assert_eq(b[8].to_int(), OP_SET_TEXT_SIZE)
  assert_eq(read_i32_le(b, 9), 18)
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_text_color wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_text_color(Color::rgba(10, 20, 30, 128))
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_TEXT_COLOR | r u8 | g u8 | b u8 | a u8.
  assert_eq(b[8].to_int(), OP_SET_TEXT_COLOR)
  assert_eq(b[9], b'\x0A')
  assert_eq(b[10], b'\x14')
  assert_eq(b[11], b'\x1E')
  assert_eq(b[12], b'\x80')
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_font_weight wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_font_weight(700)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_FONT_WEIGHT | weight i32.
  assert_eq(b[8].to_int(), OP_SET_FONT_WEIGHT)
  assert_eq(read_i32_le(b, 9), 700)
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_line_height wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_line_height(1.5)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_LINE_HEIGHT | px_x1000 i32.
  assert_eq(b[8].to_int(), OP_SET_LINE_HEIGHT)
  assert_eq(read_i32_le(b, 9), 1500) // 1.5 * 1000
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_text_align wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_text_align(TEXT_ALIGN_CENTER)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_TEXT_ALIGN | id i32.
  assert_eq(b[8].to_int(), OP_SET_TEXT_ALIGN)
  assert_eq(read_i32_le(b, 9), TEXT_ALIGN_CENTER)
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_whitespace wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_whitespace(WHITESPACE_NOWRAP)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_WHITESPACE | id i32.
  assert_eq(b[8].to_int(), OP_SET_WHITESPACE)
  assert_eq(read_i32_le(b, 9), WHITESPACE_NOWRAP)
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_font_family wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_font_family("あ")
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_FONT_FAMILY | u32 length | UTF-8 payload.
  assert_eq(b[8].to_int(), OP_SET_FONT_FAMILY)
  assert_eq(read_u32_le(b, 9), 3) // "あ" = E3 81 82
  assert_eq(b[13], b'\xE3')
  assert_eq(b[14], b'\x81')
  assert_eq(b[15], b'\x82')
  assert_eq(b.length(), 16)
}

// --- Keyboard navigation / a11y wire format (issue #52) --------------------

///|
test "command buffer set_focusable wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_focusable(1)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_FOCUSABLE | mode i32.
  assert_eq(b[8].to_int(), OP_SET_FOCUSABLE)
  assert_eq(read_i32_le(b, 9), 1)
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_tab_index wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_tab_index(3)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_TAB_INDEX | index i32.
  assert_eq(b[8].to_int(), OP_SET_TAB_INDEX)
  assert_eq(read_i32_le(b, 9), 3)
  assert_eq(b.length(), 13)
}

///|
test "command buffer set_tab_stop wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.set_tab_stop(0)
  let b = cb.to_bytes()
  // 8-byte header, then OP_SET_TAB_STOP | mode i32.
  assert_eq(b[8].to_int(), OP_SET_TAB_STOP)
  assert_eq(read_i32_le(b, 9), 0)
  assert_eq(b.length(), 13)
}

// --- Text input wire format (RFC 0003, issue #88) ---------------------------

///|
test "command buffer text_input wire format matches the Rust decoder layout" {
  // OP_TEXT_INPUT is ABI-frozen at 38 (RFC 0003 §4); pin the value, not just
  // the generated constant, so a toml renumbering fails here loudly.
  assert_eq(OP_TEXT_INPUT, 38)
  let cb = CommandBuffer::new()
  cb.text_input(7, "あ")
  let b = cb.to_bytes()
  // 8-byte header, then OP_TEXT_INPUT | input_id i32 | u32 len | UTF-8 placeholder.
  assert_eq(b[8].to_int(), OP_TEXT_INPUT)
  assert_eq(read_i32_le(b, 9), 7)
  assert_eq(read_u32_le(b, 13), 3) // "あ" = E3 81 82
  assert_eq(b[17], b'\xE3')
  assert_eq(b[18], b'\x81')
  assert_eq(b[19], b'\x82')
  assert_eq(b.length(), 20)
}

///|
test "command buffer text_input with an empty placeholder" {
  let cb = CommandBuffer::new()
  cb.text_input(1, "")
  let b = cb.to_bytes()
  assert_eq(b[8].to_int(), OP_TEXT_INPUT)
  assert_eq(read_i32_le(b, 9), 1)
  assert_eq(read_u32_le(b, 13), 0)
  assert_eq(b.length(), 17)
}

// --- G23: MoonBit native Int is a 32-bit two's-complement machine word ------
//
// The whole Rust↔MoonBit bridge assumes `Int` == `i32` across the C boundary.
// These assertions hold ONLY for a 32-bit wrapping Int: on a 64-bit Int,
// `0x7FFFFFFF + 1` would stay positive (2147483648) and the test would fail.
// The cross-boundary half of this check lives in cmd/roundtrip (gpui_abi_probe).

///|
test "Int wraps at the i32 boundary (32-bit machine word)" {
  // 0x7FFFFFFF is i32::MAX; +1 must wrap to i32::MIN (negative).
  assert_eq(0x7FFFFFFF + 1, -0x80000000)
  assert_true(0x7FFFFFFF + 1 < 0)
  // -0x80000000 is i32::MIN; -1 must wrap to i32::MAX (positive).
  assert_eq(-0x80000000 - 1, 0x7FFFFFFF)
  assert_true(-0x80000000 - 1 > 0)
}

// --- G20: structured error classification ----------------------------------

///|
test "classify_status maps every GPUI_STATUS_* code" {
  assert_eq(classify_status(-1).code(), -1)
  assert_eq(classify_status(-9).code(), -9)
  assert_eq(classify_status(-10).code(), -10)
  assert_eq(classify_status(-11).code(), -11)
  assert_eq(classify_status(-12).code(), -12)
  assert_eq(classify_status(-13).code(), -13)
  assert_eq(classify_status(-14).code(), -14)
  assert_eq(classify_status(-15).code(), -15)
  match classify_status(-1) {
    InvalidHandle => ()
    _ => fail("expected InvalidHandle")
  }
  match classify_status(-9) {
    DuplicateKey => ()
    _ => fail("expected DuplicateKey")
  }
  match classify_status(-10) {
    KeyNotFound => ()
    _ => fail("expected KeyNotFound")
  }
  match classify_status(-11) {
    QueueFull => ()
    _ => fail("expected QueueFull")
  }
  match classify_status(-12) {
    PayloadTooLarge => ()
    _ => fail("expected PayloadTooLarge")
  }
  match classify_status(-13) {
    BusyComposing => ()
    _ => fail("expected BusyComposing")
  }
  match classify_status(-14) {
    InvalidFloat => ()
    _ => fail("expected InvalidFloat")
  }
  match classify_status(-15) {
    DepthExceeded => ()
    _ => fail("expected DepthExceeded")
  }
  // Unmapped codes (including success 0) fall through to Unknown, kept verbatim.
  match classify_status(0) {
    Unknown(0) => ()
    _ => fail("expected Unknown(0)")
  }
  match classify_status(-99) {
    Unknown(-99) => ()
    _ => fail("expected Unknown(-99)")
  }
}

///|
test "status_message is a non-empty one-liner per code" {
  let codes = [
    -1, -2, -3, -4, -5, -6, -7, -8, -9, -10, -11, -12, -13, -14, -15, 0, -99,
  ]
  for c in codes {
    let msg = status_message(c)
    assert_true(msg.length() > 0)
    // One line: no embedded newline.
    assert_true(!msg.contains("\n"))
  }
}

///|
test "GpuiError to_string carries code and message" {
  let err = classify_status(-8)
  let s = err.to_string()
  assert_true(s.contains("-8"))
  assert_true(s.contains("GpuiError"))
  assert_eq(err.message(), status_message(-8))
}

// --- Scroll position feedback (issue #89) -----------------------------------

///|
test "command buffer set_scroll_id wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.div()
  cb.set_scroll_id(7)
  let b = cb.to_bytes()
  // 8-byte header, then OP_DIV, then OP_SET_SCROLL_ID | id i32.
  assert_eq(b[8].to_int(), OP_DIV)
  assert_eq(b[9].to_int(), OP_SET_SCROLL_ID)
  assert_eq(read_i32_le(b, 10), 7)
  assert_eq(b.length(), 14)
}

///|
test "read_f32_le decodes the six f32 fields of a scroll state" {
  // 14.0f32 = 0x41600000, 1.0f32 = 0x3F800000, -0.5f32 = 0xBF000000 (LE).
  let b = b"\x00\x00\x60\x41\x00\x00\x80\x3F\x00\x00\x00\xBF"
  assert_eq(read_f32_le(b, 0), 14.0)
  assert_eq(read_f32_le(b, 4), 1.0)
  assert_eq(read_f32_le(b, 8), -0.5)
}

// NOTE: the `scroll_state` pull ABI is not exercised as a `moon test` case
// because it calls the Rust export `gpui_scroll_copy_state`, which the root
// package's test executable does not link (link/link.mbt). Its error contract
// (unknown pair -> -10 KEY_NOT_FOUND, negative view -> -1 INVALID_HANDLE) is
// verified by the Rust-side suite (gpui-sys/scroll_feedback_tests.rs) and
// belongs in the linked round-trip binary (cmd/roundtrip) alongside the other
// FFI smoke tests.

// --- text_run / rich_text wire format (issue #91) ---------------------------
//
// OP_TEXT_RUN is ABI-frozen at 39; the record is a fixed 22 bytes:
// opcode u8 | start u32 LE | len u32 LE | flags u8 | r,g,b,a u8x4 |
// weight i32 LE | br,bg,bb,ba u8x4. Unset fields still occupy their slot
// (zero-filled), so the total run size never varies with the style flags.

///|
test "text_run wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  cb.text("abc", 0, 0, 0, 1.0)
  cb.text_run(0, 1, RunStyle::new(color=Color::rgba(1, 2, 3, 4), weight=700))
  cb.set_root()
  let b = cb.to_bytes()
  // OP_TEXT: opcode@8, len@9-12, "abc"@13-15, rgb@16-18, f32@19-22.
  assert_eq(b[8].to_int(), OP_TEXT)
  assert_eq(read_u32_le(b, 9), 3)
  assert_eq(b[13], b'a')
  assert_eq(b[14], b'b')
  assert_eq(b[15], b'c')
  // OP_TEXT_RUN starts at byte 23: opcode | start u32 | len u32 | flags u8.
  assert_eq(b[23].to_int(), OP_TEXT_RUN)
  assert_eq(read_u32_le(b, 24), 0)
  assert_eq(read_u32_le(b, 28), 1)
  assert_eq(b[32].to_int(), RUN_STYLE_COLOR | RUN_STYLE_WEIGHT)
  // color slot r,g,b,a.
  assert_eq(b[33], b'\x01')
  assert_eq(b[34], b'\x02')
  assert_eq(b[35], b'\x03')
  assert_eq(b[36], b'\x04')
  // weight i32 LE = 700 -> BC 02 00 00.
  assert_eq(b[37], b'\xBC')
  assert_eq(b[38], b'\x02')
  assert_eq(b[39], b'\x00')
  assert_eq(b[40], b'\x00')
  // background slot is zero-filled even though not set.
  assert_eq(b[41], b'\x00')
  assert_eq(b[42], b'\x00')
  assert_eq(b[43], b'\x00')
  assert_eq(b[44], b'\x00')
  assert_eq(b[45].to_int(), OP_SET_ROOT)
  assert_eq(b.length(), 46)
}

///|
test "text_run flag-only styles and background" {
  let cb = CommandBuffer::new()
  cb.text("a", 0, 0, 0, 1.0)
  cb.text_run(
    0,
    1,
    RunStyle::new(
      italic=true,
      underline=true,
      strikethrough=true,
      background=Color::rgba(9, 8, 7, 6),
    ),
  )
  let b = cb.to_bytes()
  // OP_TEXT: opcode@8, len@9-12, "a"@13, rgb@14-16, f32@17-20.
  assert_eq(b[8].to_int(), OP_TEXT)
  assert_eq(read_u32_le(b, 9), 1)
  // OP_TEXT_RUN starts at byte 21.
  assert_eq(b[21].to_int(), OP_TEXT_RUN)
  assert_eq(read_u32_le(b, 22), 0)
  assert_eq(read_u32_le(b, 26), 1)
  let flags = RUN_STYLE_ITALIC |
    RUN_STYLE_UNDERLINE |
    RUN_STYLE_STRIKETHROUGH |
    RUN_STYLE_BACKGROUND
  assert_eq(b[30].to_int(), flags)
  // color slot is zero-filled (color not set).
  assert_eq(b[31], b'\x00')
  assert_eq(b[32], b'\x00')
  assert_eq(b[33], b'\x00')
  assert_eq(b[34], b'\x00')
  // weight is zero-filled (weight not set).
  assert_eq(b[35], b'\x00')
  assert_eq(b[36], b'\x00')
  assert_eq(b[37], b'\x00')
  assert_eq(b[38], b'\x00')
  // background slot carries the set color.
  assert_eq(b[39], b'\x09')
  assert_eq(b[40], b'\x08')
  assert_eq(b[41], b'\x07')
  assert_eq(b[42], b'\x06')
  assert_eq(b.length(), 43)
}

///|
test "rich_text computes UTF-8 byte offsets" {
  let cb = CommandBuffer::new()
  cb.rich_text(
    [("あ", None), ("いう", Some(RunStyle::new(weight=700))), ("x", None)],
    0,
    0,
    0,
    1.0,
  )
  cb.set_root()
  let b = cb.to_bytes()
  // Concatenated "あいうx" is 10 UTF-8 bytes (3+3+3+1).
  assert_eq(b[8].to_int(), OP_TEXT)
  assert_eq(read_u32_le(b, 9), 10)
  // OP_TEXT_RUN is emitted once, at byte 30 (8 + 1+4+10+3+4).
  assert_eq(b[30].to_int(), OP_TEXT_RUN)
  // start = 3 ("あ"), len = 6 ("いう").
  assert_eq(read_u32_le(b, 31), 3)
  assert_eq(read_u32_le(b, 35), 6)
  assert_eq(b[39].to_int(), RUN_STYLE_WEIGHT)
  // color slot zero-filled, weight = 700.
  assert_eq(b[40], b'\x00')
  assert_eq(b[41], b'\x00')
  assert_eq(b[42], b'\x00')
  assert_eq(b[43], b'\x00')
  assert_eq(b[44], b'\xBC')
  assert_eq(b[45], b'\x02')
  assert_eq(b[46], b'\x00')
  assert_eq(b[47], b'\x00')
  // background slot zero-filled.
  assert_eq(b[48], b'\x00')
  assert_eq(b[49], b'\x00')
  assert_eq(b[50], b'\x00')
  assert_eq(b[51], b'\x00')
  // A single 22-byte run ends at byte 51; OP_SET_ROOT follows at 52.
  assert_eq(b[52].to_int(), OP_SET_ROOT)
  assert_eq(b.length(), 53)
}

///|
test "rich_text with no styled segments emits no runs" {
  let cb = CommandBuffer::new()
  cb.rich_text([("ab", None), ("cd", None)], 0, 0, 0, 1.0)
  cb.set_root()
  let cb2 = CommandBuffer::new()
  cb2.text("abcd", 0, 0, 0, 1.0)
  cb2.set_root()
  let a = cb.to_bytes()
  let b = cb2.to_bytes()
  assert_eq(a.length(), b.length())
  for i in 0..<a.length() {
    assert_eq(a[i], b[i])
  }
}

// --- set_text_row wire format (text-row ABI) -----------------------------------
//
// OP_SET_TEXT_ROW is ABI-frozen at 41:
// opcode u8 | key_len u32 LE | key bytes | has_caret u8 | byte_offset u32 LE |
// r,g,b u8x3 | blink u8. The writer owns the char → UTF-8 byte conversion.

///|
test "set_text_row wire format matches the Rust decoder layout" {
  let cb = CommandBuffer::new()
  // "aあb": 3 chars, 5 UTF-8 bytes (1+3+1). Caret at char 2 → byte 4.
  cb.text("aあb", 0, 0, 0, 1.0)
  cb.set_text_row("probe:r1", "aあb", 2, 10, 20, 30, true)
  cb.set_root()
  let b = cb.to_bytes()
  // OP_TEXT: opcode@8, len@9-12 (=5), "aあb"@13-17, rgb@18-20, f32@21-24.
  assert_eq(b[8].to_int(), OP_TEXT)
  assert_eq(read_u32_le(b, 9), 5)
  // OP_SET_TEXT_ROW starts at byte 25.
  assert_eq(b[25].to_int(), OP_SET_TEXT_ROW)
  assert_eq(read_u32_le(b, 26), 8) // len("probe:r1")
  assert_eq(b[30], b'p')
  assert_eq(b[37], b'1')
  assert_eq(b[38].to_int(), 1) // has_caret
  assert_eq(read_u32_le(b, 39), 4) // char 2 of "aあb" == byte 4
  assert_eq(b[43].to_int(), 10)
  assert_eq(b[44].to_int(), 20)
  assert_eq(b[45].to_int(), 30)
  assert_eq(b[46].to_int(), 1) // blink
  assert_eq(b[47].to_int(), OP_SET_ROOT)
  assert_eq(b.length(), 48)
}

///|
test "set_text_row without caret zeroes the caret slots" {
  let cb = CommandBuffer::new()
  cb.text("ab", 0, 0, 0, 1.0)
  cb.set_text_row("k", "ab", -1, 1, 2, 3, false)
  let b = cb.to_bytes()
  // OP_TEXT: opcode@8, len@9-12, "ab"@13-14, rgb@15-17, f32@18-21.
  assert_eq(b[22].to_int(), OP_SET_TEXT_ROW)
  assert_eq(read_u32_le(b, 23), 1) // len("k")
  assert_eq(b[27], b'k') // key bytes 27
  assert_eq(b[28].to_int(), 0) // has_caret
  assert_eq(read_u32_le(b, 29), 0) // inert offset slot
  assert_eq(b[36].to_int(), 0) // blink
  assert_eq(b.length(), 37) // ... + rgb(3) at 33-35 + blink(1)
}

///|
test "set_text_row clamps caret_char past the end to the row end" {
  let cb = CommandBuffer::new()
  cb.text("ab", 0, 0, 0, 1.0)
  cb.set_text_row("k", "ab", 9, 0, 0, 0, false)
  let b = cb.to_bytes()
  assert_eq(b[22].to_int(), OP_SET_TEXT_ROW)
  assert_eq(b[28].to_int(), 1) // has_caret stays 1
  assert_eq(read_u32_le(b, 29), 2) // clamped to byte offset 2 == row end
}
