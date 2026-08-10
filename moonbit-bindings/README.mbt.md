# gpui-bindings 消費者向けガイド

Rust/GPUI を MoonBit native から C FFI 越しに呼び、ネイティブウィンドウを描画するための MoonBit モジュール（`nakake/gpui-bindings`）です。UI ツリー全体を 1 つの**コマンドバッファ**として記述し、`build_tree` 1 回の FFI 呼び出しで Rust 側にコミットします。クリック・キー・テキストイベントは Rust から MoonBit のライブラリ所有 callback `dispatch_entry`（アプリが `register_dispatch` で登録した dispatch へ委譲）に戻り、フレームワーク層（型付きハンドラレジストリ・状態ストア・signal・コンポーネント、RFC 0001）がデコード・配送・再構築を担います。

このモジュールはローカル向けの実験的プロジェクトであり、安定した汎用 UI API ではありません。内部設計の詳細は [`docs/architecture.md`](../docs/architecture.md)（AI 向け内部文書）を参照してください。

## 必要条件

native build のみ対応です。対応 OS/architecture は macOS arm64・x86_64、Linux x86_64、Windows MSVC x64（cross compile 非対応）。

- Rust toolchain（`cargo`）と MoonBit native toolchain（`moon`）
- macOS: Xcode Command Line Tools / Xcode、macOS SDK、GPUI/Metal 用フレームワーク
- Linux: native C/C++ toolchain と X11/XKB 系ライブラリ。システムの XCB/XKB runtime library が使えない場合は、リポジトリ root の ignored な fallback `.linux-libs/` を利用できます
- Windows: Rust/MoonBit と MSVC x64 C++ build tools（`build.ps1` は `cl.exe` 未設定時に Visual Studio の x64 開発シェルを自動検出します）

## クイックスタート

このディレクトリ単独の `moon build` は完全な最終 build 手順ではありません。Rust static library、OS 別 link flags、Rust→MoonBit callback symbol はリポジトリ root の build driver が準備します。

```bash
git clone https://github.com/nakake/gpui-moonbit.git
cd gpui-moonbit

# macOS / Linux
./build.sh
```

```powershell
# Windows
.\build.ps1
```

build 完了後、build driver が起動コマンドを表示します。

```bash
# macOS（キーボード入力には .app バンドルが必要）
open dist/Runner.app
# stderr をターミナルで見る場合
./dist/Runner.app/Contents/MacOS/Runner

# Linux / WSLg（X11 経路を明示）
cd moonbit-bindings
env -u WAYLAND_DISPLAY \
  ./_build/native/debug/build/cmd/main/main.exe
# システムの XCB/XKB runtime library が見つからない場合だけ:
LD_LIBRARY_PATH=$PWD/../.linux-libs env -u WAYLAND_DISPLAY \
  ./_build/native/debug/build/cmd/main/main.exe
```

```powershell
# Windows
.\moonbit-bindings\_build\native\debug\build\cmd\main\main.exe
```

起動するのは `cmd/main` の最小ランナーです（クリックと `space` キーで受信イベント数が動きます）。build driver が用意するのはこの実行ファイルで、デモアプリは `examples/` 配下の独立モジュールになりました（#125）。Counter デモを動かすには `cd examples/counter && moon build` してから `./_build/native/debug/build/main/main.exe` を実行します。`-1` / `Reset` / `+1` / `+10` ボタン、`j` / `k` / `r` キー、Enter/Escape/矢印キー、数字入力で値を操作でき、テキスト入力ボックス（RFC 0003）に数字を入力して Enter を押すとその値がカウントにセットされます（IME 合成にも対応）。

MoonBit 側の型検査だけなら、このディレクトリで `moon check`（および `moon test`）を実行できます。

## 依存として消費する（prebuild 方式、#93）

本モジュールは `--moonbit-unstable-prebuild`（実験的機能）により、**path / git 依存として消費できます**。コンシューマの `moon build` 時に、同梱の `build.py` が Rust staticlib をビルドし、リンクフラグを自動伝播します。**Linux x86_64 / macOS arm64 / Windows MSVC x64 の 3 つで CI 検証済み**です（`tests/consumer` の最小コンシューマを 3 ランナーすべてでビルド・実行、#103）。なお、このリポジトリ内でデモアプリをフルビルドする場合は従来どおり `build.sh` / `build.ps1` を使います。

### 1. 依存を追加する

DSL 形式の `moon.mod` は registry 依存しか記述できないため、**JSON 形式の `moon.mod.json`** を使います（mooncakes 未公開のため、現状は path / git 依存のみ）:

```jsonc
// moon.mod.json
{
  "name": "your/app",
  "version": "0.1.0",
  "source": ".",
  "deps": {
    "nakake/gpui-bindings": { "path": "/path/to/gpui-moonbit/moonbit-bindings" }
  }
}
```

git 依存の場合は `{ "git": { "url": "https://github.com/nakake/gpui-moonbit", "subdir": "moonbit-bindings" } }` の形式を使います（未検証）。

### 2. 実行ファイルの moon.pkg で 2 パッケージを import する

```moonbit nocheck
// exe の moon.pkg
import {
  "nakake/gpui-bindings",       // 高水準 API（CommandBuffer / build_tree / run_window）
  "nakake/gpui-bindings/link",  // Rust staticlib のリンクフラグ伝播を受ける
}

options("is-main": true)
```

`link` パッケージはコードを含まず、prebuild のリンクフラグ伝播の受け口としてだけ存在します。**ライブラリパッケージやテストファイルからは import しないでください**（テスト実行ファイルにフラグが伝播し、tcc リンカが失敗します）。

同じ理由で、**`link` を import したパッケージにはテストを置けません**。`moon test` はそのパッケージのテスト実行ファイルにも伝播したフラグを渡し、moon はテストを tcc でリンクするため `library 'stdc++' not found` で落ちます。ユニットテストを書くなら、アプリ本体を `link` を import しない別のライブラリパッケージに置き、`main` パッケージは `link` の import と `fn main` だけに保つ構成が安全です（[`examples/counter`](../examples/counter) がその形です）。

### 3. main で自分の dispatch を登録する

Rust staticlib が解決するコールバックはライブラリ所有の `dispatch_entry` 1 本で、その中身は起動時に差し替えます。`run_window` より前に、メインスレッドから登録してください（RFC 0004）:

```moonbit nocheck
///|
fn dispatch(
  version : Int,
  kind : Int,
  view : Int,
  data_a : Int,
  data_b : Int,
) -> Int {
  @nakake/gpui-bindings.framework_dispatch(
    ctx,
    version,
    kind,
    view,
    data_a,
    data_b,
    fn(v) { build_tree(v) },
  )
}

///|
fn main {
  @nakake/gpui-bindings.register_dispatch(dispatch)
  ignore(@nakake/gpui-bindings/link.LINK_MARKER)

  // ... アプリ本体（build_tree / run_window）
}
```

再登録は last-wins です（テストが dispatch を差し替える手段を兼ねます）。登録を忘れたままイベントが届いた場合は、`0`（変化なし）が返り初回だけ警告が 1 行出ます。かつて必要だった `let _keep : (Int, Int, Int, Int, Int) -> Int = …` の明示保持は**不要になりました** — `register_dispatch` の呼び出し自体が `dispatch_entry` を dead-code elimination から守ります。

### 4. ビルドして実行する

```bash
moon build --target native
# Linux / WSLg（X11 経路を明示。システムの XCB/XKB が見つからない場合だけ LD_LIBRARY_PATH 指定）
env -u WAYLAND_DISPLAY ./_build/native/debug/build/main/main.exe
```

初回ビルドでは `build.py` が `cargo build` を実行するため時間がかかります（Rust toolchain 必須）。2 回目以降は cargo のインクリメンタルビルドで高速です。

### フォールバック（テンプレートリポジトリ方式）

`--moonbit-unstable-prebuild` は「extremely experimental」で API が予告なく変わり得ます。壊れた場合は、本リポジトリを fork / clone して `build.sh` / `build.ps1` を使うテンプレートリポジトリ方式に退避できます（[スパイレポート](../docs/spikes/2026-07-24-packaging-feasibility.md) の方式 B）。mooncakes 公開は、実験的機能への依存を公開パッケージに固定しないため、意図的に見送っています（[`docs/versioning.md`](../docs/versioning.md) 参照）。

## 使い方

アプリの実装パターンは [`examples/counter`](../examples/counter)（Counter デモ。path 依存の別モジュールとして本モジュールを消費します）が手本です。低水準のコマンドバッファの上に、フレームワーク層（状態・ハンドラ・コンポーネント・イベントループ）を載せます。

### 1. ツリーをコマンドバッファで記述する

`CommandBuffer` はスタックマシンです。`div()` / `text()` はノードを生成してスタックに積み、setter はスタックトップに適用され、`add_child()` は子→親を接続して親をスタックトップに残し、`set_root()` でルートを確定します。

```moonbit nocheck
///|
pub fn build_tree(view : Int) -> Result[Unit, Int] {
  let cb = @nakake/gpui-bindings.CommandBuffer::new()
  cb.div() // ルート
  cb.set_bg(28, 30, 38)
  cb.set_flex_col()
  cb.set_center()
  cb.set_gap(28.0)
  cb.set_padding(32.0)

  cb.text("Counter", 236, 239, 245, 30.0)
  cb.add_child()

  // クリック可能なボタン: HandlerId は dispatch にルーティングされる
  cb.div()
  cb.set_key("btn-increment") // 安定した識別子（任意）
  cb.set_size(92.0, 56.0)
  cb.set_bg(70, 160, 95)
  cb.set_rounded(10.0)
  cb.set_flex_col()
  cb.set_center()
  cb.set_on_click(btn_increment.to_int()) // HandlerRegistry 発行の HandlerId
  cb.text("+1", 255, 255, 255, 18.0)
  cb.add_child() // ラベルをボタンに接続
  cb.add_child() // ボタンをルートに接続

  cb.set_root()
  @nakake/gpui-bindings.build_tree(view, cb)
}
```

利用可能なコマンド: `div` / `text` / `text_input` / `set_size` / `set_bg` / `set_flex_row` / `set_flex_col` / `set_center` / `set_gap` / `set_rounded` / `set_on_click` / `set_key` / `set_scroll_id` / `set_padding` / `set_border` / `add_child` / `set_root`。色成分は 0–255 にクランプされます。テキストは内部で UTF-8 にエンコードされ、明示長で送られます（NUL 終端なし）。繰り返し現れる部分木は**コンポーネント**（`button(cb, props)` / `text_input(cb, props)` など、`components.mbt`）として切り出せます。コンポーネントは `CommandBuffer` に部分木を書き、ルートはスタックに残るので、呼び出し側が `add_child()` で接続します。

**色の渡し方**: alpha 付きの `Color` を取る API（`set_bg_color` / `set_text_color` / `set_shadow` / `set_border_color`）を推奨します。生の `r, g, b` トリプレットを取る `set_bg` / `set_border` / `text` も引き続き利用可能で、wire format は同一です（issue #81 で整合化）。`Color` は `Color::rgb(r, g, b)` / `Color::rgba(r, g, b, a)` で作ります。

**テキスト入力ボックス**（RFC 0003）: `text_input(cb, props)` コンポーネント（`components.mbt`）が編集可能な 1 行ボックスを描きます。`TextInputProps { key, input_id, placeholder, min_width }` を取り、`input_id` は `HandlerRegistry::new_input_id()` で発行します。`min_width`（px）は必須です: 入力欄の中身は枠 div の幅 100% で敷かれるため、`set_center` の列など**枠が内容幅に縮む親**に置くとその 100% が 0px に解決してしまい、枠は padding だけの細い箱になり placeholder が枠外にはみ出し、クリック判定も 0 幅になってフォーカスできません。`min_width` が枠の幅を確定させます（親が十分広ければ親が勝ちます）。編集バッファは Rust 側のテキストモデルが正であり（再構築を跨いで生存）、MoonBit の store には置きません。イベントは **pull 型**です: `EVENT_INPUT_CHANGED`（確定テキストの変化）/ `EVENT_INPUT_SUBMIT`（Enter）は `(4, kind, view, input_id, 0)` で届き、ペイロードを運びません。ハンドラは `input_text(view, input_id)` で現在内容を読み、`input_set_text(view, input_id, text)` で書き換えます。`input_set_text` は IME 合成中に `GPUI_STATUS_BUSY_COMPOSING`（`-13`）で拒否します。登録パターンはクリックハンドラと同じく、id の発行とハンドラ登録を 1 つの top-level let に束ねます（DCE 安全）:

```moonbit nocheck
///|
let prompt_input = {
  let id = handlers.new_input_id()
  handlers.on_submit(fn(view) {
    match input_text(view, id) {
      Ok(text) =>
        // ... text を使って状態を更新
        ignore(input_set_text(view, id, "")) // クリア
      Err(_) => ()
    }
  })
  id
}
```

ツリー側では `text_input(cb, { key: "prompt-input", input_id: prompt_input, placeholder: "...", min_width: 360 })` を呼び `add_child()` で接続します。`on_input_changed(fn(view){…})` も登録可能で、確定テキストのたびに呼ばれます（preedit 更新では呼ばれません）。

### 2. ウィンドウを開く

```moonbit nocheck
match @nakake/gpui-bindings.run_window(0, 600.0, 500.0) {
  Ok(_) => ()
  Err(status) => abort("run_window failed with status \{status}")
}
```

`run_window(view, width, height)` は `view` にコミット済みのツリーを描画し、イベントループでブロックします。`view` は `build_tree` が設定する Rust 側 view slot の index です。`build_tree` / `run_window` は `Result[Unit, Int]` を返し、`Err(status)` の `status` は負の `GPUI_STATUS_*` コードです。

### 3. 状態・ハンドラ・イベントループ（フレームワーク層、RFC 0001）

状態とイベント配送はフレームワーク層が担います。ハンドラは「変わったか」を戻り値で報告せず、signal を `set` するだけ。再構築はフレームワークが store の dirty 判定でスケジュールします。

- **`Store` / `CellId[T]`**（`store.mbt`）: 型付き状態セル。`Store::new()` で作り、`new_cell(initial)` で `CellId[T]` を得ます。`get` / `set` で読み書きし、`cell_for_key(key, initial)` でキー付き共有セルも作れます。
- **`Signal[T]`**（`signal.mbt`）: セルを購読する宣言的プリミティブ。`store.signal(cell)` で作り、`sig.get(store)` / `sig.set(store, value)` で操作します。`set` が store を dirty にします。
- **`HandlerRegistry`**（`handlers.mbt`）: 型付きハンドラの登録と配送。`on_click(fn(view){…})` は `HandlerId` を返し、`on_key` / `on_named_key` / `on_text` も登録できます。`dispatch(event, view)` が `Event` を該当ハンドラへ fan-out します。
- **`RenderCtx`**（`components.mbt`）: `{ view, store, handlers }` を束ね、コンポーネントへ渡す描画コンテキスト。
- **`framework_dispatch`**（`framework.mbt`）: envelope デコード → ハンドラ配送 → dirty 判定 → 再構築を 1 本にまとめたイベントループ接着。

アプリの骨格:

```moonbit nocheck
///|
let store = @nakake/gpui-bindings.Store::new()

///|
let count = store.signal(store.new_cell(0))

///|
let handlers = @nakake/gpui-bindings.HandlerRegistry::new()

///|
let btn_increment = handlers.on_click(fn(_view) {
  count.set(store, count.get(store) + 1)
})

///|
let ctx : @nakake/gpui-bindings.RenderCtx = { view: 0, store, handlers }

///|
pub fn dispatch(
  version : Int,
  kind : Int,
  view : Int,
  data_a : Int,
  data_b : Int,
) -> Int {
  @nakake/gpui-bindings.framework_dispatch(
    ctx,
    version,
    kind,
    view,
    data_a,
    data_b,
    fn(v) { build_tree(v) },
  )
}
```

### 4. イベントを受け取る（callback 契約）

Rust からのイベントは、**ライブラリ所有**のエントリポイント `dispatch_entry`（ルートパッケージ `nakake/gpui-bindings`）に固定の 5×i32 envelope で届きます（RFC 0004）。Rust がリンクする ABI 契約はこの 1 本に固定されており、アプリ側の関数名・パッケージ名は自由です。アプリは起動時（`run_window` より前・メインスレッド）に `register_dispatch(dispatch)` で自分の dispatch を登録し、`dispatch_entry` がそれへ委譲します。再登録は last-wins で、未登録のままイベントが届くと `0`（変化なし）を返して初回だけ警告を 1 行出します。登録する関数のシグネチャは次のとおりで、実体は `framework_dispatch` への 1 行委譲です。

```moonbit nocheck
fn dispatch(version : Int, kind : Int, view : Int, data_a : Int, data_b : Int) -> Int
```

- slot 0 `version`: 常に `ABI_VERSION`（現在は `4`）。不一致なら `framework_dispatch` がハンドラを実行せず `0` を返して古い Rust バイナリを拒否します
- slot 1 `kind`: イベント種別（`EVENT_CLICK` = 1、`EVENT_KEY` = 2、`EVENT_TEXT` = 3、`EVENT_NAMED_KEY` = 4、`EVENT_ASYNC` = 5、`EVENT_INPUT_CHANGED` = 6、`EVENT_INPUT_SUBMIT` = 7、`EVENT_SCROLL` = 8）
- slot 2 `view`: 再構築対象の view id
- slot 3–4 `data_a` / `data_b`: 種別依存
  - `EVENT_CLICK`: `data_a` = click_id（`HandlerId` の raw 値）、`data_b` = 0
  - `EVENT_KEY`: `data_a` = codepoint、`data_b` = modifier bits
  - `EVENT_TEXT`: `data_a` = token、`data_b` = byte 長（ペイロードは `gpui_event_copy_text` でコピー）
  - `EVENT_NAMED_KEY`: `data_a` = named_key id（`KEY_ENTER` / `KEY_ESCAPE` / `KEY_UP` …）、`data_b` = modifier bits
  - `EVENT_ASYNC`: `data_a` = token、`data_b` = byte 長（ペイロードは `copy_async_payload` でコピー。RFC 0002 の非同期注入経路）
  - `EVENT_INPUT_CHANGED` / `EVENT_INPUT_SUBMIT`: `data_a` = input_id（`InputId` の raw 値）、`data_b` = 0。ペイロードはなく、現在内容は `input_text(view, input_id)` で pull する（RFC 0003）
  - `EVENT_SCROLL`: `data_a` = scroll_id（`ScrollId` の raw 値。`new_scroll_id` で発行し `set_scroll_id` でワイヤに書き、`on_scroll` で購読）、`data_b` = 0。ペイロードはなく、現在位置は `scroll_state(view, scroll_id)` で pull する（issue #89）

`dispatch` は状態が変わった場合に `1`、変わらない場合に `0` を返します。`framework_dispatch` は配送の前後で store の dirty を区切り、`set` が 1 度でも起きたときだけ再構築コールバックを呼んで `1` を返します。`1` のときだけ Rust 側が再描画通知（`cx.notify()`）を行います。再構築に失敗しても Rust 側は旧ツリーを保持しているため、dirty に基づき `1` を返して構いません。

`EVENT_ASYNC` は非同期イベント注入（RFC 0002）の配送種別です。外部 native コードが `gpui_post_event(view, ptr, len)`（ラッパー `post_event`）で任意スレッドからペイロードを push すると、メインスレッドが `EVENT_ASYNC` として `dispatch` に届けます。ペイロードは opaque bytes で、解釈（UTF-8 テキストか否か等）はハンドラ側の契約です。消費者例は `examples/stream` を参照してください。

MoonBit native の `Int` は 32-bit であり、この callback とコマンドバッファの境界も **i32** です（`gpui_abi_probe` で機械検証済み）。値は i32 範囲で扱ってください。

`main` 関数では、`register_dispatch` で自分の dispatch を登録します（[`cmd/main/main.mbt`](cmd/main/main.mbt) を参照）。

## Examples

いずれもリポジトリ root の [`examples/`](../examples) にある**独立モジュール**で、path 依存で本モジュールを消費します（`tests/consumer` と同じ経路）。つまり配布された形での呼び出し方をそのまま実演しており、各自が `register_dispatch` で自分の dispatch を登録する実行ファイルです。ビルドと実行は各ディレクトリで `moon build` → `./_build/native/debug/build/main/main.exe`。

- [`examples/counter`](../examples/counter) — interactive Counter（ボタン 4 つ + キー操作 + テキスト入力ボックス）。テキストボックスへの数字入力 + Enter でカウントをセットします（`on_submit` + `input_text` / `input_set_text`、RFC 0003）。ユニットテスト付きで、アプリの実装パターンの手本です。
- [`examples/hello`](../examples/hello) — Counter 以外の最小例。静的なタイトルと ON/OFF が切り替わるステータスカード、1 つのトグルボタン、`space` / `Escape` キー操作を実装しています。
- [`examples/stream`](../examples/stream) — 非同期イベント注入（RFC 0002）の消費者側。外部のネイティブ producer が `gpui_post_event` で流したペイロードを `EVENT_ASYNC` として受け、ログ表示を in-place 更新します。

## API リファレンス

公開 API（`CommandBuffer` の各メソッド、`build_tree` / `run_window` / `input_text` / `input_set_text`、フレームワーク層の `Store` / `CellId` / `Signal` / `HandlerRegistry` / `HandlerId` / `InputId` / `RenderCtx` / `button` / `text_input` / `framework_dispatch`、`Event`、および `abi_constants.mbt` の定数群）には MoonBit の doc comment `///|` が付いています。ソースと併せて参照してください。

- 対象ファイル: [`gpui-bindings.mbt`](gpui-bindings.mbt)（高水準 API）、[`components.mbt`](components.mbt) / [`store.mbt`](store.mbt) / [`signal.mbt`](signal.mbt) / [`event.mbt`](event.mbt) / [`handlers.mbt`](handlers.mbt) / [`framework.mbt`](framework.mbt)（フレームワーク層、RFC 0001）、[`deprecated.mbt`](deprecated.mbt)（非推奨エイリアス）、[`abi_constants.mbt`](abi_constants.mbt)（`gpui-sys/abi.toml` から生成される ABI 定数）
- ドキュメント生成: MoonBit ツールチェーンの標準手段は `moon doc`（`moon doc --serve` でローカルサーバ起動）です。現行ツールチェーン（moon 0.1.20260721 時点）では、パッケージ単位の JSON（`_build/doc/nakake/gpui-bindings/package_data.json` 等、`///|` doc comment を含む）は生成されますが、最終的なドキュメントサイト組み立て段階で moondoc が `moon.mod.json` を要求して例外終了します（本モジュールは新形式の `moon.mod` を使用）。サイト生成は moondoc 側の対応待ちです。それまでは `///|` doc comment とソースが API リファレンスの正本です。

## 制約・注意

- **native バックエンド専用**です。wasm 等の他 target には対応しません。
- **callback は単一固定契約**です。Rust→MoonBit のイベント経路は `dispatch_entry(version, kind, view, data_a, data_b) -> Int`（5×i32 envelope、`ABI_VERSION` = 4）の 1 本だけで、これはライブラリが所有します。アプリは `register_dispatch` でその中身を差し替え、実体は通常 `framework_dispatch` への委譲になります（デコード・型付き配送・dirty 判定・再構築はフレームワーク層の担当）。`dispatch_entry` の名前は `gpui-sys/abi.toml` の `[callback] name` が正本で、改名するとマングルシンボルが変わるため Rust 側と build driver の更新が必要になります。
- **境界の整数は i32** です。MoonBit native の `Int` は 32-bit 2 の補数機械語であり、FFI 境界とコマンドバッファの wire format は i32/u32 little-endian です。この ABI 互換は `gpui_abi_probe` の境界値往復（ビルドのたびに実行）と wbtest で機械検証されています。
- **ツリー更新は dirty 時の再構築**です。状態変化（signal の `set`）のたびにフレームワークがツリーを再構築します。Counter デモは `update_text` でカウント表示だけを書き換えるインクリメンタル経路を試し、キー未登録時は `build_tree` による全再構築へフォールバックします（issue #10）。汎用 vdom diff は意図的に未実装です。
- **opcode と ABI 定数は生成物**です。`gpui-sys/abi.toml` を正本として build driver が生成します。`abi_constants.mbt` と `gpui-bindings-ffi.mbt` は手編集しません。
- 負の status code の意味（無効 handle、バッファの magic/バージョン不一致、未知 opcode、ルート未指定、キー重複など）は [`docs/architecture.md`](../docs/architecture.md) を参照してください。
- **callback はメインスレッド限定・total 関数**です。ランタイムは非アトミック参照カウントのため、`dispatch` はメインスレッドからのみ呼べます。MoonBit の panic は FFI 境界を越えられず process abort になるため、callback は例外を投げない全関数に保ってください。詳細は [`docs/architecture.md`](../docs/architecture.md) §11「MoonBit native 実行時制約」を参照。
- **エラーは構造化できます**。`build_tree` / `run_window` の `Err(status)` は、`classify_status(status)` で `GpuiError` に変換でき、`status_message(status)` / `GpuiError::to_string` で 1 行の診断メッセージを得られます。回復できない失敗には `expect_ok(result, ctx)` が構造化メッセージ付きで abort します。
- **非推奨 API**。`set_absolute(mode)` は `set_position(mode)` に改名しました（実態は position-mode setter のため、issue #81）。旧名は [`deprecated.mbt`](deprecated.mbt) に非推奨エイリアスとして残っており、引き続き動作します。

## ライセンス

Apache-2.0
