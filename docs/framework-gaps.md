# フレームワーク化に向けたギャップ分析

本プロジェクトを「ローカル向け実験」から**第三者が利用できるライブラリ/フレームワーク**へ発展させるにあたり、解決すべき内容を洗い出した分析文書。2026-07-23 時点のコード・`docs/architecture.md`・`docs/reviews/2026-07-16-codex-gpt5.6-sol.md` に基づく。各ギャップには後で issue 化しやすいよう安定 ID（`G1`〜）を付す。

**読み方の注意**: 事実（コード/ドキュメントで確認できるもの）と `[推測]` を区別する。特に §1 のパッケージング成立性はツールチェーン調査が未実施であり、本分析最大の不確実性である。

関連ドキュメント: [`architecture.md`](architecture.md)（現行実装）、[`roadmap.md`](roadmap.md)（進捗・現況）、[`reviews/2026-07-16-codex-gpt5.6-sol.md`](reviews/2026-07-16-codex-gpt5.6-sol.md)（codex アーキレビュー）。

---

## 0. 前提: codex レビューの P0/P1 はほぼ消化済み

2026-07-16 の codex レビューが挙げた改善項目は、その後の issue 対応で大部分が解決している。フレームワーク化の議論は、この土台の上に乗る。

| レビュー指摘 | 優先度 | 現状 | 対応 issue |
|---|---|---|---|
| ABI 不一致・stale/wrong callback 選択 | P0 | ✅ `abi.toml` からの定数生成 + drift guard + 最終バイナリの nm 検証 | — |
| C export の panic-safe 化・handle 検証 | P0 | ✅ status code 体系（`GPUI_STATUS_*`）・checked access・atomic commit | — |
| 絶対ビルドパスの除去 | P0 | ✅ 生成 `moon.pkg` + `native-static-libs` 自動取得・正規化 | — |
| builder transaction と明示 root | P1 | ✅ コマンドバッファ + `OP_SET_ROOT` + view 別 `VIEWS` | #5 |
| property-per-call → バッチ化ノード記述 | P1 | ✅ コマンドバッファ（1 FFI でツリー全体） | #5 |
| バージョン付きイベント envelope | P1 | ✅ slot 0 = `ABI_VERSION`、`EVENT_TEXT` は token+copy、named key 対応 | #39 |
| native ライブラリフラグの自動検証 | P1 | ✅ `cargo rustc --print native-static-libs` の捕捉・注入 | — |
| 境界横断統合テスト | P1 | ✅ ヘッドレス往復テスト + 3 OS CI | #34 |
| click ID に依存しない安定 ID | P2 | ✅ `OP_SET_KEY`（重複拒否） | #9 |
| **計測で正当化できるインクリメンタル更新** | P2 | ✅ #10 解決済み: key 指定の in-place text 更新 FFI `gpui_update_text`（保持 `VIEWS` 内の key 付き div を探索し、最初の Text 子を書き換え。再デコード/再確保なし）。汎用 vdom diff は意図的に不採用。ベンチ（24 行 realistic tree）: フルリビルド `11.4 µs` に対し `update_text` `25.9 ns` ≒ **440×**（decode/FFI 経路のみの比較）。ただし state 変化時は `update_text` 経由でも `cx.notify()` により gpui の再レイアウトが走る（`framework.mbt` の `framework_dispatch` は dirty なら常に `1` を返す契約）ため、フレーム全体（`render/headless_layout_24rows` は Linux x86_64 (WSL2) 実測で 約722 µs）に対する end-to-end の削減効果は約1.5%にとどまる。価値は decode/rebuild コストがツリーサイズに線形に伸びるのに対し、`update_text` は対象 key までの走査と1ノードの書き換えで済み、ツリーが育っても Rust 側 FFI コストを頭打ちにできる点（layout コスト自体は gpui 側の責務で本項目の対象外）。key 未命中は `GPUI_STATUS_KEY_NOT_FOUND` を返し、MoonBit 側はフル `build_tree` へフォールバック（opt-in 最適化、フルリビルドは既定の正解パスとして維持） | #10 |
| **text 空白パディングのコンテンツ汚染** | P2 | ✅ #16 解決済み: 空白パディングを撤廃し、paint-time の ¼px 描画オフセット（`TextGlyphInset`）で先頭グリフのサブピクセル欠けを回避 | #16 |

結論: 「正しく動く demo」の土台（ABI 契約・panic 安全性・ビルド再現性・テスト）は固い。残るのは**「第三者が使える」にするための軸**で、これは codex レビューの射程外だった。

---

## 1. パッケージング — ライブラリとして消費できない【最重要】

**現状、これはライブラリではなく「ビルドスクリプト付きリポジトリ」である。** 第三者が依存関係として追加し、自分のアプリからビルドする手段がない。

- **`G1` [完了 2026-07-25] モジュールマニフェスト整備。** `moonbit-bindings/moon.mod` の `description` / `repository` / `keywords` を埋め、`moon check` 通過を確認（#48）。ただし `name` はプレースホルダ `username/gpui-bindings` のまま残っており、`repository`（`nakake/gpui-moonbit`）と名前空間が矛盾していた。#72（2026-08-01）で `name` を `nakake/gpui-bindings` にリネームし、import・コード例・ドキュメントの全 61 箇所を解消。マングル名も再測定済み（`_M0FP36nakake15gpui_2dbindings3app8dispatch`）。
- **`G2` [完了 2026-08-01] prebuild パイプライン実装。** `moonbit-bindings/build.py`（`--moonbit-unstable-prebuild` 登録）が Rust staticlib のビルド・マングルシンボル確定・`native-static-libs` リンクフラグ伝播をパッケージ機構として実行する。コンシューマは `moon.mod.json` の path/git 依存で本モジュールを追加し、exe の `moon.pkg` で `nakake/gpui-bindings/link` を import するだけでリンクが成立する（#93）。Linux x86_64 / macOS arm64 / Windows MSVC x64 の 3 OS で CI 検証済み（#103: `tests/consumer` の最小コンシューマを `moon test` より前に 3 ランナーすべてでビルド・実行する。Windows は MSVC 形式のリンクフラグを絶対パスで渡す実装が必要だった）。`--moonbit-unstable-prebuild` は実験的機能のため、フォールバックとして `build.sh`/`build.ps1` 方式（テンプレートリポジトリ）を維持する。mooncakes 公開は後回し（prebuild の API 安定性を見極めるため）。
- **`G3` [検証済み 2026-07-24] MoonBit native のパッケージ機構で Rust staticlib 依存の配布は原理的に可能。** `cc-link-flags` は依存から伝播しない（[moon#1595](https://github.com/moonbitlang/moon/issues/1595)）。唯一の経路は実験的機能 `--moonbit-unstable-prebuild`（`moon.mod` に登録した JS/Python スクリプトが依存として消費された場合でも実行され、LinkConfig の `link_libs`/`link_search_paths` が dependents へ伝播する）。2 モジュール構成で実機検証済み（[スパイレポート](spikes/2026-07-24-packaging-feasibility.md)）。リスク: API が「extremely experimental」で変更の可能性。フォールバックとしてテンプレートリポジトリ方式（現状の `build.sh`）を併記する。
- **`G4` [完了 2026-07-25] バージョニング方針の整備。** [`versioning.md`](versioning.md) で `ABI_VERSION`（現在 4）とモジュール/クレート semver（現在 0.1.0）の関係・バンプ規則・changelog 方針・リリースチェックリストを定義し、[`CHANGELOG.md`](../CHANGELOG.md) を導入（#48）。
- **`G5` [保留] macOS 配布用の署名・entitlement・icon・パッケージングがない。** codex §3。現状の `.app` バンドルは開発専用（`build.sh` の `--bundle`、issue #40）。保留理由: 署名・配布整備は別トラックで扱う。

---

## 2. API 表現力 — widget / style 表面

フレームワークとしての最大の実機能ギャップ。現状の描画要素は **div と text のみ**（`moonbit-bindings/gpui-bindings.mbt`）。

- **`G6` widget 種の不足。** image / text input（編集可能ボックス）/ scroll / list（仮想化）/ stack（z-index）/ absolute 配置 / checkbox・toggle 等。✅ #51 part 3 で gpui 0.2.2 で実現可能な範囲を充足: **scroll** — `OP_SET_OVERFLOW` の `SCROLL` 軸が実際のスクロールになり、`render_node` が保持された `ScrollHandle` を割り当てる。`set_key` 付き div は再構築をまたいでスクロール位置を維持（ハンドルは `Rc` ベースで `Send` でないため、`Mutex` 下の `VIEWS` ではなく view ごとの `FfiView.scroll_handles` に保持。キーなしスクロール div は毎再構築で先頭に戻る）。**checkbox** — `moonbit-bindings/widgets.mbt` の合成ヘルパー `checkbox`（☐/☑ グリフ＋ラベル＋`set_on_click`、既存 opcode のみで ABI 変更なし）。`labeled_row` も追加。**stack（z-index）** — 新規 opcode 不要: gpui は子をペイント順に描画し、絶対配置の兄弟はペイント順で重なるため、`set_absolute`＋`set_inset`（part 1）＋子の追加順がそのまま z-index 相当になる。**absolute 配置** — part 1 で充足（`OP_SET_POSITION`/`OP_SET_INSET`）。✅ **#88（RFC 0003）で text input（編集可能）を実装**: `OP_TEXT_INPUT`(38) の leaf ノード＋ Rust 側テキストモデル（`FfiView.inputs`、再構築を跨いで生存）＋ `EntityInputHandler` 実装による IME preedit（下線描画・候補ウィンドウ位置）＋ `EVENT_INPUT_CHANGED`/`EVENT_INPUT_SUBMIT`（pull 型）＋ `gpui_input_*` 3 本の pull ABI。MoonBit 側は `CommandBuffer::text_input` / `input_text` / `input_set_text` ラッパーと `text_input` コンポーネント（`components.mbt`）。✅ **#103 で image を実装**: `OP_IMAGE`(42) の leaf ノード＋ gpui のアセットパイプライン直結（`http(s)://` は `ReqwestHttpClient` 経由で非同期ダウンロード、`data:` は `image_source_from_data_uri` が base64 復号、それ以外はファイルパスとして読み込み）。`max_w`/`max_h` は上界で、内在サイズは復号後に gpui が与える。読み込み中/失敗は `with_loading`/`with_fallback` のプレースホルダカード。MoonBit 側は `CommandBuffer::image(source, width, height, fit)`、`IMAGE_FIT_*` 定数、`GpuiError::InvalidImageFit`（`-17`）。SVG は gpui のアセットパイプラインが `ImageFormat::Svg` として扱う（`resvg` 依存は gpui 側に既存）。未実装: コマンドバッファからの生バイト列 `ImageSource`（呼び出し側がメモリ上のバイト列を直接渡す経路）。**list（仮想化）** — gpui の `list`/`uniform_list` は Rust の render callback を要求し、スカラのコマンドバッファではブリッジ不能なため恒久的に見送り。
- **`G7` style 表面の不足。** 現状は `size / bg / flex(row|col) / center / gap / rounded / padding / border` のみ。margin、辺別 padding/border、min/max/auto サイズ、flex-grow/shrink/basis、align/justify、overflow、opacity、shadow、transform、cursor 指定がない。✅ #51 part 1 で margin・min/max サイズ・flex-grow/shrink/basis・align/justify・overflow・opacity・shadow・cursor・absolute 配置+inset を追加（`OP_SET_MARGIN`/`OP_SET_MIN_SIZE`/`OP_SET_MAX_SIZE`/`OP_SET_FLEX_ITEM`/`OP_SET_ALIGN`/`OP_SET_OVERFLOW`/`OP_SET_OPACITY`/`OP_SET_SHADOW`/`OP_SET_CURSOR`/`OP_SET_POSITION`/`OP_SET_INSET`）。未実装: 辺別 padding/border（均一のみ）、transform（gpui 0.2.2 に Style レベルの transform フィールドが無いため意図的に見送り）。
- **`G8` typography の不足。** text は単一 size + 単一 color のみ。weight / line-height / align / wrap 制御 / font family / rich text（部分装飾）がない。✅ #51 part 2 で font size・text color（alpha 付き）・font weight・line height・text align・whitespace（折り返し制御）・font family を追加（`OP_SET_TEXT_SIZE`/`OP_SET_TEXT_COLOR`/`OP_SET_FONT_WEIGHT`/`OP_SET_LINE_HEIGHT`/`OP_SET_TEXT_ALIGN`/`OP_SET_WHITESPACE`/`OP_SET_FONT_FAMILY`）。いずれも div の `Style.text` に設定され子孫テキストへ継承される。制限: gpui 0.2.2 の enum 制約で `TEXT_ALIGN_JUSTIFY` は左揃えに、`WHITESPACE_PRE`/`PRE_WRAP` は NOWRAP/NORMAL にフォールバック。✅ rich text（部分装飾）は #91 で解決: `OP_TEXT_RUN`（固定 22 バイトレコード、UTF-8 バイト範囲 + `RUN_STYLE_*` フラグで color / weight / italic / underline / strikethrough / background を run 単位に上書き）を追加し、Rust 側は `StyledText::with_highlights`（遅延解決、ベーススタイルは従来の `Style.text` 継承）で描画する。不正な範囲（境界外・`char` 境界違反・重複）はデコード時に `GPUI_STATUS_INVALID_TEXT_RUN` でバッファごと拒否。MoonBit 側はオフセット計算を隠蔽する `CommandBuffer::rich_text(segments)` と低レベル `text_run` を公開。
- **`G9` 色の抽象がない。** 全域が生の RGB `Int`（`set_bg(r, g, b)` 等）。alpha 通道なし、`Color` 型なし、テーマ/デザイントークンなし。✅ #51 part 1 で `Color` 型（`rgb`/`rgba` + 命名プリセット）と alpha 付きの `set_bg_color`（`OP_SET_BG_COLOR`）を追加。既存の `set_bg(r, g, b)` は後方互換で維持。未実装: テーマ/デザイントークン。
- **`G10` text 空白パディング hack。** #16 解決済み: `render_node` の `format!(" {content} ")` を撤廃し、paint-time 専用の `TextGlyphInset`（prepaint 原点を ¼px 右オフセット、レイアウト・コンテンツは不変）で先頭グリフのサブピクセル欠けを回避（`docs/troubleshooting.md` §2）。

---

## 3. コンポーネントモデルと状態管理

- **`G11` コンポーネント抽象がない。** ✅ #86 解決済み（RFC 0001 Phase A–D）: `components.mbt` に `RenderCtx`（`{ view, store, handlers }`）と再利用可能コンポーネント `button(cb, props)` を導入。アプリは `build_tree` をコンポーネント呼び出しの列として記述し、各コンポーネントが `CommandBuffer` に部分木を書いてルートをスタックに残す。click_id の手配線は `HandlerId` に置換。
- **`G12` イベントルーティングが手動 int switch。** ✅ #86 解決済み（RFC 0001 Phase A）: `event.mbt` の型付き `Event` enum と `decode_event`、`handlers.mbt` の `HandlerRegistry`（`on_click` / `on_key` / `on_named_key` / `on_text`、`dispatch` で fan-out）を導入。int 定数・int switch は撤廃。MoonBit native の callback 制約（スカラーのみ）は、envelope を型付き `Event` にデコードしレジストリで配送することで回避。
- **`G13` 状態がグローバル可変配列。** ✅ #86 解決済み（RFC 0001 Phase B）: `store.mbt` に型付き状態ストア `Store` / `CellId[T]` を導入。`Array[Int]` / `Array[Bool]` のグローバル可変配列は `CellId[Int]` / `CellId[Bool]` に置換。`cell_for_key` でキー付き共有セルも利用可能。
- **`G14` reactive ループが `dispatch` 内にハードコード。** ✅ #86 解決済み（RFC 0001 Phase D）: `signal.mbt` の `Signal[T]` と `framework.mbt` の `framework_dispatch` を導入。ハンドラは signal の `set` のみを行い、フレームワークが store の dirty 追跡で再構築をスケジュールする（dirty のときだけ rebuild + `1`）。`changed` 戻り値の報告は撤廃。dirty は現状グローバル（全ツリー再構築）。サブツリー単位の dirty は #10 のサブツリー patch opcode 待ち（Signal API は不変）。

---

## 4. マルチウィンドウ / アプリライフサイクル

- **`G15` 単一ウィンドウ・永久ブロック実行。** `run_window` は 1 ウィンドウを開きイベントループでブロックする（`moonbit-bindings/cmd/main/main.mbt:14`）。複数ウィンドウ、非ブロッキング実行、アプリ級ループ、quit 処理がない。
- **`G16` ウィンドウ/アプリイベントの欠如。** resize / close / focus / menu / tray 等のイベント経路がない。
- **`G17` ~~イベントが view 単位でルーティングされない。~~** ✅ #49 解決済み（2026-07-25）: dispatch envelope を 5 スロット `(abi_version, event_kind, view, data_a, data_b)` に拡張（`ABI_VERSION=4`）。slot 2 が view id を運び、コールバック（現在は `dispatch_entry`、#125）は view 単位で rebuild をルーティングする。

---

## 5. 堅牢性 / 本番品質

- **`G18` アクセシビリティ（a11y）。** ✅ #52 でキーボードナビゲーションを実装（`OP_SET_FOCUSABLE`/`OP_SET_TAB_INDEX`/`OP_SET_TAB_STOP` + Tab/Shift+Tab トラバース、`window.rs:1413/1424`）。role/label/スクリーンリーダーは gpui 0.2.2 に API がなく上流ブロック（`element.rs:51` の Element trait は id/layout/paint のみ）。フォーカス可視化は `.focus` スタイルで対応可（`G7` の style 拡張、未配線）。詳細は [`docs/a11y-ime.md`](a11y-ime.md)。
- **`G19` IME 合成。** ✅ #88（RFC 0003）で実装済み: テキスト入力 widget（`OP_TEXT_INPUT`）の Rust 側テキストモデルが `EntityInputHandler`（`input.rs:10`）を実装し、preedit（合成中）の下線描画・候補ウィンドウ位置（`bounds_for_range` ← `x_for_index`）・確定テキストの `EVENT_INPUT_CHANGED` 通知と `gpui_input_*` pull ABI を提供する。確定テキストは従来どおり `EVENT_TEXT`（`key_char` 経路）でも動作する。詳細は [`docs/a11y-ime.md`](a11y-ime.md) §2.2 と [`docs/rfc/0003-text-input-ime.md`](rfc/0003-text-input-ime.md)。
- **~~`G20` エラーのアプリ側露出が貧弱。~~** ✅ #54 解決済み: `GpuiError` enum + `classify_status` / `status_message` / `expect_ok` を追加し、demo の `println`・`abort` を構造化メッセージに置換。
- **~~`G21` logging / diagnostics API がない。~~** ✅ #54 解決済み: `status_message`（コードごとの 1 行診断）と `debug_dump_text(view)` ラッパを公開。
- **~~`G22` MoonBit native の実行時制約を API が強制も文書化もしていない。~~** ✅ #54 解決済み: `docs/architecture.md`「MoonBit native 実行時制約」と `moonbit-bindings/README.md` 制約・注意に文書化（codex §2 を引用）。
- **~~`G23` MoonBit `Int` == `i32` の ABI 互換が実験的前提。~~** ✅ #54 解決済み: `gpui_abi_probe` の境界値往復（round-trip、ビルドのたび実行）+ 32-bit wrap セマンティクスの wbtest で機械的に検証。

---

## 6. テスト / QA（フレームワーク規模向け）

- **~~`G24` ヘッドレスなレイアウト/描画検証がない。~~** ✅ #53 解決済み: `gpui-sys/src/headless.rs` ハーネス（`test-support` 時のみコンパイル、staticlib には漏れない）が実デコーダ → 実 `render_node` で headless `TestAppContext` に描画し、gpui の `debug_bounds` でジオメトリを読み戻す。`headless_tests.rs` に golden layout テスト（sized div / flex-row+gap / padding+border / テキストノードの既知サイズ）を配置。`NoopTextSystem`（advance 0.6·size、line height `phi()`）で決定的に再現。
- **~~`G25` コマンドバッファデコーダの体系的ファジングがない。~~** ✅ #53 解決済み: `fuzz_tests.rs` にシード固定 xorshift64* PRNG による決定的ファジング（ランダム 10k + 構造的にもっともらしいバッファ 10k + エッジケース）。デコーダが panic せず `GPUI_STATUS_*` のみを返すことを検証。nightly 向け `cargo-fuzz` スキャフォールド（`gpui-sys/fuzz/`、ASan + カバレッジ誘導）も追加（メインビルドからは隔離）。
- **~~`G26` 性能ベンチ/回帰 harness がない。~~** ✅ #53 解決済み: `gpui-sys/benches/decode_bench.rs`（criterion、`[[bench]] harness = false`）。現実的なツリーのデコード + headless レイアウトを計測。issue #10（インクリメンタル更新）の着手前提を満たした。

---

## 7. ドキュメント / DX

- **`G27` 消費者向け getting-started がない。** 🟡 部分解決（#125、RFC 0004）: `examples/` の counter / hello / stream がライブラリを path 依存で消費する**実消費者モジュール**になり、`register_dispatch` で自分の dispatch を登録する経路が動く形で示された（`tests/consumer` が 3 OS CI でその経路を回している）。残るのは導線: `README.md` は依然リポジトリビルダー向けで、消費者が最初に読む単独のチュートリアルがない。
- **`G28` API リファレンスがない。** `///|` doc comment はあるが生成ドキュメントサイトがない。`architecture.md` は AI 向け内部文書。
- **~~`G29` `moonbit-bindings/README.md` が空（0 バイト）。~~** ✅ 解決済み: `README.mbt.md`（`README.md` はそこへの symlink）が消費者向けの使い方を持つ。#125 で `_keep` 手順を `register_dispatch` に差し替え済み。

---

## クリティカルパス（推奨順序）

「ライブラリ/フレームワークを目指す」なら、機能追加より**消費可能性の確立**が先。

1. **~~【調査・最優先】`G3` パッケージングの成立性。~~** ✅ 検証済み（2026-07-24）。`--moonbit-unstable-prebuild` で成立。[スパイレポート](spikes/2026-07-24-packaging-feasibility.md)参照。次のアクション: 実際の gpui-sys で prebuild スクリプトのプロトタイプ実装（#48 の G2 に接続）。
2. **~~`G17` マルチ view/ウィンドウのイベントルーティング。~~** ✅ 完了（#49、2026-07-25）。5 スロット envelope（`ABI_VERSION=4`）で確定済み。
3. **~~`G11`〜`G14` コンポーネント/状態抽象の設計。~~** ✅ #86 完了（RFC 0001 Phase A–D）。型付きハンドラレジストリ・状態ストア・signal・コンポーネント・フレームワーク dispatch を導入。click_id int 配線とグローバル可変状態を再利用可能な層へ置換。
4. **`G6`〜`G10` API 表現力の拡充** と **`G18`/`G19` a11y/IME。** 3 の抽象の上に乗せる。
5. **~~`G24`〜`G26` テスト基盤~~** ✅ #53 完了（ヘッドレス layout 検証・ファジング・ベンチ）。4 と #10 の安全網。
6. **`G1`/`G2`/`G4`/`G5`/`G27`〜`G29` 配布整備**（マニフェスト・署名・semver・docs・example）。

既存の残り issue は自然に合流する: ~~**#10（インクリメンタル更新）は 5 のベンチ後**~~ ✅ #10 解決済み（key 指定 in-place text 更新、decode/FFI 経路のみでフルリビルド比 ≒440×、`G26` ベンチで計測。end-to-end の文脈は上記 G10 行を参照）、~~**#16（text padding = `G10`）は 4 のテキスト API 設計時**~~ ✅ #16 解決済み。

---

## 起票の目安

各 `G*` を issue 化する際の粒度案:

- 単独 issue 向き: `G1`（マニフェスト整備）✅、~~`G3`（パッケージング調査・スパイク）~~ ✅ 完了（#47）、`G17`（view ルーティング ABI）✅ 完了（#49）、`G29`（空 README）✅。
- 設計 RFC 向き（単一 issue では大きすぎる）: `G11`〜`G14`（コンポーネントモデル）、`G6`〜`G9`（widget/style 体系）。
- 既存 issue に統合: `G10` → #16、~~`G26` → #10 の前提~~ ✅ `G26` 完了（#53）で #10 の前提充足。
