# RFC 0004: dispatch のランタイム登録(ライブラリ所有エントリポイント)

| 項目 | 内容 |
|---|---|
| ステータス | 設計確定(2026-08-05)。実装は #125 の管轄。実装後の権威は [`architecture.md`](../architecture.md) |
| 作成日 | 2026-08-05 |
| 対象 | Rust→MoonBit コールバックの束縛機構(公開 ABI のリンク時契約) |
| 関連 issue | #125(本 RFC・実装)、#92(ライブラリ方針)、#93(prebuild、前提)、#76(abi.toml 単一情報源)、#107(LinkConfig 二重適用 — cmd 再設計の別 issue が絡む) |
| 前提ドキュメント | [`architecture.md`](../architecture.md) §4b(envelope)、[`0001-component-model.md`](0001-component-model.md)(framework_dispatch)、[`../moonbit-bindings/README.md`](../../moonbit-bindings/README.md)(ABI 契約) |

本 RFC は、Rust staticlib が解決する MoonBit コールバックシンボルを **Counter デモ(`app.dispatch`)からライブラリ所有の固定エントリポイント(`dispatch_entry`)へ移し、アプリ本体は起動時にクロージャとして登録する**方式(#125 の A-2)を定める。これにより消費者は build driver をフォークせずに自分のアプリを書けるようになり、`app/` はライブラリの公開モジュールツリーから `examples/` へ退去できる。

---

## 1. 背景と動機

Rust staticlib はコールバックを**マングルシンボル名**で解決する: `build.sh`([2/5])が `PKG_FN_SUFFIX="3app8dispatch"` で生成 C からシンボルを抽出し、`gpui-sys/build.rs` が `#[link_name = …] fn mb_dispatch(version, kind, view, data_a, data_b) -> i32` を生成する。その実体は Counter デモ(`moonbit-bindings/app/app.mbt`)であり、消費者は依存グラフに Counter を抱え込んだうえ、自分の dispatch に差し替える公式な手段がない(#125)。

代替案とその不採用理由:

- **A-1(C 関数ポインタ登録)**: `gpui_register_dispatch(fn_ptr)` を C エクスポートする。抽出機構ごと消えるが、MoonBit native のクロージャを plain な C 関数ポインタへ落とせるかが未検証で、GC との相互作用という不確実性も抱える。
- **B(シンボルパスをビルド設定化)**: `PKG_FN_SUFFIX` を prebuild のパラメータにする。変更は最小だが、消費者に MoonBit のマングリング規約を意識させ続ける。

**A-2 は関数ポインタが FFI 境界を越えない**(登録は MoonBit 内で完結し、C から見えるシンボルはライブラリ所有の 1 本のまま)ため、A-1 の不確実性なしにライブラリと具体アプリを分離できる。

## 2. 制約(動かせない前提)

1. **戻り値は changed フラグである。** `mb_dispatch` の全呼び出し箇所(`lib.rs`)は戻り値を `changed != 0` で読み、再描画通知の要否にだけ使う。`0` は「状態変化なし」という合法な応答であり、これが未登録時デフォルトの設計余地になる(§3.3)。
2. **`mb_dispatch` はメインスレッド限定**(RFC 0002 §2-1)。登録クロージャの呼び出しもメインスレッドに限られるため、`Ref` 保持にロックは不要。
3. **EVENT_ASYNC は dispatch 復帰後に `EVENT_QUEUE` を clear する**(`lib.rs:372`、RFC 0002 §3.4)。未登録のまま届いた非同期ペイロードは黙って消える — 未登録を完全無音にできない理由(§3.3)。
4. **staticlib の未解決参照は 1 本のリンク名だけ**を持つ。エントリポイントをライブラリが所有すれば、同一モジュール内の複数実行ファイル(cmd/main・roundtrip・examples)がそれぞれ別のアプリを登録して共存できる。

## 3. 設計

### 3.1 公開 API(トップレベルパッケージ `nakake/gpui-bindings`)

```moonbit nocheck
///| Rust が唯一リンクする C 可視エントリポイント。純粋委譲で、ポリシーを持たない。
pub fn dispatch_entry(version : Int, kind : Int, view : Int,
                      data_a : Int, data_b : Int) -> Int

///| アプリの dispatch を登録する。run_window より前にメインスレッドで呼ぶ。
pub fn register_dispatch(f : (Int, Int, Int, Int, Int) -> Int) -> Unit
```

- 保持は `Ref[(Int, Int, Int, Int, Int) -> Int]`(既定値は §3.3 の no-op)。`dispatch_entry` は呼び出しのたびに Ref を読んで委譲する。
- **version チェックや kind の解釈はエントリでやらない。** それらは登録側(通常は `framework_dispatch`)の責務のままにし、エントリの仕事を「シンボルを安定させること」1 つに保つ。

### 3.2 登録規約

- **last-wins**: 再登録は上書き。テストが dispatch を差し替える正規の手段であり、専用の reset API は設けない。
- **タイミング**: 「`run_window` より前・メインスレッドで呼ぶ」を README / `architecture.md` に文書化する。実行時ガードは付けない(dispatch はメインスレッドからしか呼ばれないため、run 前の登録なら競合が構造的に起きない)。
- 型付きの糖衣(`run_app(registry, …)` 等)は本 RFC のスコープ外とし、必要になった時点で別 issue を切る。

### 3.3 未登録時の挙動

既定クロージャは **`0`(変化なし)を返す no-op + 初回のみ警告 1 行**。

- `0` は §2-1 のとおり ABI 的に合法で、Rust 側は単に再描画しない。abort にしないので、dispatch を必要としないビルド(cmd/roundtrip のヘッドレス検証、リンク確認)は登録なしでそのまま動く。
- 完全無音にしないのは §2-3 のため: EVENT_ASYNC はキューが clear されるので、register 忘れは「イベントが消える」として現れ、無音だとデバッグ困難になる。警告は初回のみ(イベント毎に出すとストリーミングで洪水になる)。
- **出力先は stdout(`println`)**(§8-1 の結論)。当初は stderr を想定していたが、MoonBit core に stderr へ書く API が無く(`println` は `%println` 経由で stdout 固定、runtime.c の stderr ヘルパは `static`)、fd 2 に届かせるには新しい C エクスポート(§5 で不採用)かルートパッケージへの native stub(全消費者がコンパイルすることになる)が要る。警告 1 行のために公開ビルド面を増やす取引には見合わないと判断した。

### 3.4 DCE — 消費者の `_keep` 呪文の撤廃(目標)

現行の消費者は `let _keep : (Int, Int, Int, Int, Int) -> Int = @….app.dispatch` を main に書く義務がある(README §3)。本設計では **`register_dispatch` の実装内部から `dispatch_entry` を参照させる**ことで、登録した時点でシンボルが到達可能になり、消費者側の呪文を不要にすることを狙う。

- 検証: 別モジュール + path 依存(tests/consumer 経路)からのリンクで、`_keep` なしに `dispatch_entry` のシンボルが残ることを確認する。
- フォールバック: 成立しない場合は従来どおり `_keep` を要求する(対象がライブラリ所有シンボルになるだけで手数は現状と同じ)。README の記載は検証結果で確定する。
- `link.LINK_MARKER` の ignore は本 RFC の対象外(現状維持)。

### 3.5 ビルド機構への影響

- **抽出機構は温存する。** `PKG_FN_SUFFIX` の指す先が「ライブラリ所有の二度と動かないシンボル」になるだけで、build.sh / build.ps1 の [2/5](生成 C からの抽出 → `mb_symbol.txt` → `build.rs` の `#[link_name]` 生成)はそのまま生きる。消費者からは完全に不可視になる。
- **`abi.toml` の `[callback]` name を `dispatch_entry` に改名**し、`PKG_FN_SUFFIX` は name から導出する(#76 で params は導出済み。name も揃えることで、次に改名が起きても abi.toml 1 箇所で済む)。
- `ABI_VERSION = 4` は**据え置き**。5×i32 envelope・イベント種別・status code は一切変わらず、変わるのはリンク時契約(シンボル名)のみ。外部消費者ゼロの現時点が唯一の壊しどきである。

## 4. リポジトリ構成の変更

- **`app/` → `examples/counter`**: Counter デモを**別モジュール + path 依存**(tests/consumer と同じ経路)へ移す。「配布された形での呼び出し方」を examples 自体が実演する形になる。
- **`examples/hello` / `examples/stream`**: 同様に実消費者モジュール化し、実行ファイルにする(各自が自分の dispatch を register する。§2-4 により共存可能。hello 冒頭コメントの future work の解消)。
- **`cmd/main`**: 自前のミニ dispatch を register する最小ランナーに書き換えて温存する。build.sh のシンボル抽出元([2/5])と実行段([4/5][5/5])のパスが無変更で済む。cmd/roundtrip は登録不要(§3.3)。
- **cmd/{main,roundtrip} のテンプレート方式 → prebuild 方式への再設計は別 issue**とし、#125 完了後に着手する(#107 の LinkConfig 二重適用が絡むため切り離す)。

## 5. ABI 影響

| 要素 | 変更 | 後方互換性 |
|---|---|---|
| コールバックシンボル | `…3app8dispatch` → ライブラリ所有 `dispatch_entry` に**移設** | リンク時契約の破壊的変更。外部消費者ゼロの今のみ許容 |
| envelope / `[callback]` params | **変更なし**(5×i32) | `ABI_VERSION = 4` 据え置き |
| `[events]` / status code / opcode | **変更なし** | — |
| C export | **追加なし**(登録は MoonBit 内で完結) | — |
| MoonBit 公開 API | `dispatch_entry` / `register_dispatch` を**追加**、`app` パッケージを**削除**(examples へ) | モジュール利用者は register 呼び出しの追加が必要 |
| README ABI 契約 | 「変えない」対象を `dispatch_entry` に**書き換え** | `_keep` 節は §3.4 の検証結果で確定 |

## 6. 検証計画

1. **tests/consumer の昇格**: Counter import を廃し、自前の最小アプリ(state + dispatch)を register → `dispatch_entry` 経由でイベントを流す → 自前 state の遷移を assert。3 OS CI で実行し、「消費者がアプリを書ける」ことを smoke でなく実証する。
2. **DCE リンクテスト**: §3.4(consumer で `_keep` なしリンク)。
3. **registry 依存スパイク(並行)**: 0.0.x を mooncakes へ試験公開し、`.mooncakes/` にフェッチされた依存でも prebuild(`--moonbit-unstable-prebuild`)が走るかを確認する。実績は path 依存のみのため、公開段階で設計に跳ね返る前に潰す。結果は本 RFC に追記する。

### 6-3 の結果(2026-08-06)

**試験公開の前に、公開しても動かないことが判明した**ため、公開せずに構造の可否を先に検証した。

**ブロッカー**: `moonbit-bindings/build.py` は Rust ソースを `<module_root>/../gpui-sys` に探す。mooncakes 経由だとモジュールは `.mooncakes/nakake/gpui-bindings/` に展開され、その親に `gpui-sys` は無い(`.linux-libs` も同様)。したがって「MoonBit モジュールだけを公開する」形は成立しない。

検討した 3 案のうち **C(crates.io 経由)** を採る:

| 案 | 中身 | 判断 |
|---|---|---|
| A | `include` でモジュール外の `gpui-sys/` を同梱 | moon が module_root 外を含められるか不明。tarball も肥大 |
| B | `gpui-sys/` を `moonbit-bindings/` 配下へ移す | リポジトリ構造の大手術。build.sh / CI / 本 RFC に波及 |
| **C** | `gpui-sys` を crates.io に公開し、同梱の wrapper crate から引く | **採用**。既存のディレクトリ構造を変えない |

**C の技術的前提を実測で確認した**(Linux x86_64):

- `crate-type = ["staticlib"]` の wrapper crate に `extern crate gpui_sys;` の 1 行を書くだけで、依存 rlib の `#[no_mangle]` エクスポート 11 個が**すべて staticlib に残る**。`-C link-dead-code` も re-export も不要だった。MoonBit callback への未解決参照も直接ビルドと同一
- end-to-end も通る。`build.py` の `-lgpui_sys` を `-lgpui_sys_wrapper` に差し替えて `tests/consumer` をビルド・実行 → PASS(イベント注入 8 ステップ含む)
- 依存の形(path / crates.io)はコード生成とリンクに影響しないので、path 依存での実測で足りる
- `gpui-sys` は git 依存を持たない(`gpui = "0.2"` 等すべて registry)ため crates.io 公開が可能。名前も空き。`gpui` 本体は 0.2.2 が公開済み

**publish 前に必要と判明した修正**(本 RFC の範囲で実施):

1. `mb_symbol.txt` は `.gitignore` にあり `cargo package` に含まれない。一方 `build.rs` はそれを必須として `panic!` していたため、公開クレートは消費者環境で即死する。→ §3.5 でシンボルが固定・決定的になったことを使い、`abi.toml` の `[callback]` から `build.rs` が**自分で計算して既定値にする**。`mb_symbol.txt` は「必須」から「`build.sh` 用の上書き」へ降格。両者が食い違ったときは `cargo:warning` で報告する(ハードコードしていた name ガードの tripwire を、より正確な形で置き換え)
2. `gpui-sys/Cargo.toml` に crates.io 必須の `description` / `license` が無かった → 追加
3. モジュールパスの導出元として `abi.toml` の `[callback]` に `module` を追加。`build.rs` と `build.py` が各自ハードコードせず 1 箇所から導く

**残件**: `build.py` を registry 消費に対応させる(wrapper crate の生成と置き場所)のは build driver の再設計であり、`gpui-sys` を publish するまで end-to-end 検証もできないため**別 issue**とする。#126(cmd の prebuild 方式への再設計)と範囲が重なるので、そちらと合わせて設計する。macOS / Windows も未検証。

## 7. 実装計画(#125 のスコープ)

1. `dispatch_entry` / `register_dispatch` + 未登録 no-op + 警告(トップレベルパッケージ)。
2. `abi.toml` `[callback]` name 改名と build.sh / build.ps1 の suffix 導出、`mb_symbol.txt` 経路の追従。
3. `app/` の examples/counter(別モジュール)への移動、cmd/main の最小ランナー化、hello / stream の実行ファイル化。
4. tests/consumer の昇格(§6-1)と DCE 検証(§6-2)。
5. ドキュメント: README(§3 の書き換え)、`architecture.md` §4b、`framework-gaps.md`。
6. 並行: registry スパイク(§6-3)。

各段は独立に PR にできる。3 は 1・2 の後、4 は 3 の後。

## 8. 未決事項

1. ~~**警告の文言と出力先**~~ 解決(§3.3): MoonBit から stderr へ書く手段が無いため stdout(`println`)1 行、初回のみ。文言は `[gpui-bindings] warning: no dispatch registered; …`。
2. ~~**suffix の具体値**~~ 解決: `_M0FP26nakake15gpui_2dbindings15dispatch__entry`(2026-08-06 実測)。導出元は `abi.toml` の `[callback]` の `name` と `module` に一本化され、build.sh / build.ps1 は suffix を、build.py と `gpui-sys/build.rs` は完全なシンボルを、いずれもそこから導く。
3. **`_keep` 撤廃の成否**(§3.4): 検証結果で README の消費者手順が変わる。
4. **cmd 再設計 issue の起票内容**: §4 のとおり別 issue(このリポジトリの issue トラッカー参照)。
