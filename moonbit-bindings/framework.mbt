// Framework event loop glue (RFC 0001 §3.4, G14).
//
// Replaces the hand-written `if changed == 1 { build_tree }` that apps used to
// inline in `dispatch`. `framework_dispatch` decodes the envelope, routes it
// through the handler registry, and — if any handler dirtied the store —
// invokes the app's rebuild callback and reports `1` to Rust (the `0`/`1`
// contract of architecture.md §4b is preserved: Rust calls `cx.notify()` only
// on `1`).
//
// A single dispatch batches any number of `set`s into one rebuild: dirt is
// checked once, at the end (RFC 0001 §3.4). The rebuild path stays total —
// failures surface as `Err(status)` and never panic across the FFI boundary.

///|
/// envelope のデコード → ハンドラ配送 → dirty 判定 → 再構築を 1 本にまとめた
/// フレームワークのイベントループ接着です（RFC 0001 §3.4）。アプリの `dispatch` は
/// これを呼ぶだけになります。
///
/// - `version` が `ABI_VERSION` と不一致なら（stale な Rust バイナリ）ハンドラを
///   実行せず `0` を返します。
/// - ハンドラ配送の前後で store の dirty 追跡を区切り、`set` が 1 度でも起きたとき
///   だけ `rebuild(view)` を呼んで `1` を返します（Rust が `cx.notify()` する条件）。
/// - `rebuild` の失敗は呼び出し側の責任で処理されます（`Result` をそのまま返す）。
pub fn framework_dispatch(
  ctx : RenderCtx,
  version : Int,
  kind : Int,
  view : Int,
  data_a : Int,
  data_b : Int,
  rebuild : (Int) -> Result[Unit, Int],
) -> Int {
  // Runtime version handshake: reject a stale Rust binary before any handler
  // runs or the tree is touched.
  if version != ABI_VERSION {
    return 0
  }
  ctx.store.begin_dirty_tracking()
  ctx.handlers.dispatch(decode_event(version, kind, data_a, data_b), view)
  if ctx.store.has_dirty() {
    ignore(rebuild(view))
    1
  } else {
    0
  }
}

/// Dispatch with an optional retained-tree patch before falling back to a full
/// rebuild. The patch callback must return `false` when styling or structure
/// changed and the complete command tree is required.
pub fn framework_dispatch_with_patch(
  ctx : RenderCtx,
  version : Int,
  kind : Int,
  view : Int,
  data_a : Int,
  data_b : Int,
  patch : (Int) -> Bool,
  rebuild : (Int) -> Result[Unit, Int],
) -> Int {
  if version != ABI_VERSION {
    return 0
  }
  ctx.store.begin_dirty_tracking()
  ctx.handlers.dispatch(decode_event(version, kind, data_a, data_b), view)
  if !ctx.store.has_dirty() {
    return 0
  }
  if patch(view) {
    // The retained tree now reflects the changed state; clear the marker so
    // the next dispatch starts with a clean dirty-tracking window.
    ctx.store.begin_dirty_tracking()
    1
  } else {
    ignore(rebuild(view))
    1
  }
}
