// Composite widgets (issue #51, G6) built entirely from existing command-buffer
// opcodes. These add NO ABI surface: each helper only sequences `div` / `text`
// and the setters already defined in gpui-bindings.mbt, so the wire format and
// the Rust decoder are unchanged. Widgets that genuinely need new machinery
// (editable text input landed in issue #88 as `OP_TEXT_INPUT`, the image opcode
// in issue #103 as `OP_IMAGE`; a virtualized list stays deferred) — see
// docs/framework-gaps.md G6.

///|
/// チェックボックスをコマンドバッファに出力する合成ウィジェットです。☐/☑ グリフと
/// ラベルを横並びにしたクリック可能な div で、`checked` によりグリフが
/// ☐（未チェック）/ ☑（チェック済み）に切り替わります。クリックは `click_id` として
/// `dispatch` の `data_a` にルーティングされます（クリック id がそのまま div の安定
/// 識別子にもなります）。既存の opcode のみで構成されるため ABI 変更はありません。
/// 出力後、外枠 div はスタックトップに残るため、呼び出し元は `add_child` で親に接続できます。
pub fn checkbox(
  cb : CommandBuffer,
  checked : Bool,
  label : String,
  click_id : Int,
) -> Unit {
  cb.div()
  cb.set_flex_row()
  cb.set_gap(6.0)
  cb.set_center()
  cb.set_on_click(click_id)
  cb.text(if checked { "☑" } else { "☐" }, 200, 200, 210, 18.0)
  cb.add_child()
  cb.text(label, 220, 220, 230, 16.0)
  cb.add_child()
}

///|
/// ラベルと値を横並びにする行を出力する合成ウィジェットです。フォーム風の表示向けで、
/// `key` は再構築をまたぐ安定識別子（GPUI element id）になります。既存の opcode のみで
/// 構成されるため ABI 変更はありません。出力後、外枠 div はスタックトップに残るため、
/// 呼び出し元は `add_child` で親に接続できます。
pub fn labeled_row(
  cb : CommandBuffer,
  key : String,
  label : String,
  value : String,
) -> Unit {
  cb.div()
  cb.set_key(key)
  cb.set_flex_row()
  cb.set_gap(8.0)
  cb.set_center()
  cb.text(label, 170, 175, 188, 14.0)
  cb.add_child()
  cb.text(value, 220, 220, 230, 14.0)
  cb.add_child()
}
