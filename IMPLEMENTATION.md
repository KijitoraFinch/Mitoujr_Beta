# Beta v1 実装と検証

目的と方針は DESIGNDOC.md、実装後の確認点は REVIEW_NOTES.md に置く。
ここは作業状況と完成確認の記録であり、利用者との確認済み仕様を追加する文書ではない。

## 追加レビュー（2026-09-25）

対象は commit `ce1df8f`。`REVIEW_NOTES.md` に沿ってコードを読み、
`cargo test --all-targets` の26件が成功することを確認した。
そのうえで、試験用の外部拡張と一時ディレクトリの実ファイルを使い、次の4件を再現した。
以下はレビュー時点の再現記録。項目1は後続のパス処理修正で解消し、項目2〜4は未修正である。

1. **P1：シンボリックリンクを含むパスで別の正本を編集する。**
   `shortcut -> real/inner` とし、`victim.links` と `real/victim.links` の両方に同じIDの宣言を置く。
   `link remove --in shortcut/../victim.links --extension sets --id victim` は、
   指定先の `real/victim.links` を残し、ルートの `victim.links` から宣言を削除して終了コード0を返す。
   [src/workspace.rs](src/workspace.rs) の `path_resource` が、実際のパス解決を考慮せず `..` を取り除く。
   **修正済み：** 自前の正規化を削除し、親ディレクトリの解決を標準ライブラリの
   `canonicalize`、URIの変換を `url` クレートに委ねた。既存Sidecarの削除と新規作成の回帰テストが成功。

2. **P2：集約した複数の記載から条件を継ぎ合わせて一致させる。**
   データを `[1,2,3]` とし、selectorのquery `0` と `1` が同じ領域になる状態で、
   source/targetがそれぞれ `(0,1)` と `(1,0)` の二つの宣言を置く。
   `source == selector("sets:rows.json#0") and target == selector("sets:rows.json#0")`
   は、両方を満たす記載がないのに1リンクを返す。
   [src/query.rs](src/query.rs) の `evaluate` が、原子条件ごとに別々のoccurrenceを選んでいる。

3. **P2：問い合わせ中の未知の名前・循環参照を成功扱いで隠す。**
   宣言が空の状態で `links --to @missing` を実行すると、終了コード0・`complete:true` になる。
   宣言がある場合も `annotation == null or target == region("@missing")` の左辺が真なら成功する。
   `@missing` を循環する名前に置き換えた場合も同様。
   [src/query.rs](src/query.rs) では名前の展開を候補ごとの条件評価まで遅らせているため、
   候補の有無や短絡評価によって入力の誤りが表に出ない。

4. **P2：比較できない領域を、意味の異なる宣言と確定する。**
   同じ宣言IDで、参照先を異なるcomparison domainの拡張に解釈させると、
   `links` が `complete:true`・`conflicts[].verified:true` を返す。
   同じ領域どうしを問い合わせで比較すると、`no shared comparison domain` で判定不能になる。
   [src/graph.rs](src/graph.rs) の `same_endpoint` はdomainの相違を `false` として返し、
   比較を完了できたか確認せずに競合を確定している。

編集対象以外の内容を維持する責任は、現在の外部拡張APIでは編集拡張に置かれている。
Coreの再抽出検証は対象宣言と件数の確認までであり、今回の4件とは別の実装上の責任分担として確認した。

## パス処理の修正と検証（2026-09-25）

ローカルパスの結合・解決は `std::path` と `canonicalize`、URIの解釈・エンコードは
`url` クレートへ集約した。走査結果は `Path` のまま渡し、ファイル名の文字コードを変換しない。
新規ファイルでは存在する親ディレクトリを解決し、保存済みURIの再利用は現在のファイルシステムに依存しない。
試験用Python拡張も、標準ライブラリでURIとOSのパス表現を変換する。

シンボリックリンクと `..`、新規ファイル、空白・日本語・URI予約文字、不正URI、
元ディレクトリ削除後のsnapshot再利用を確認した。非UTF-8のパスはURI変換の往復を検証し、
実ファイルの試験にはこのmacOS環境で作成可能な名前を使った。
全体試験で見つかった中断テストの起動通知の競合も、通知ファイルの原子的な公開で修正した。

`cargo test --locked --offline` は単体5件・CLI結合16件・外部拡張結合9件、計30件成功。
`cargo clippy --locked --offline --all-targets -- -D warnings` と `cargo fmt --all -- --check` も成功。
この修正ではWindows環境の実行試験は行っていない。

## Tokioへの移行と検証（2026-09-25）

標準入力を開いたまま `monika --json request` を
待機させ、SIGINTを2回送っても終了しない問題を再現した。各送信後400ms待機しても終了せず、
試験の後始末でSIGKILLを送った。同期 `read_to_end` 中には中断フラグが確認されていなかった。
今回、CLIから観測・問い合わせ・編集・外部拡張の呼び出しまでをasyncに変更し、この問題を修正した。
シグナル、プロセスI/O、期限付きの待機はTokio、JSON Linesの読み取りはtokio-util、
プロセス群の管理はprocess-wrapへ移した。自前のシグナルハンドラ、通信スレッドとポーリングを削除した。

一つのCLI操作のHostが拡張プロセスを所有し、過去の観測や書き込み後の観測でも同じ設定のプロセスを使う。
通常終了ではEOFを送り、中断や通信破損ではプロセス群を終了する。どちらも明示的に終了を待つ。
通信途中でfutureが破棄された場合も、その通信路を再利用しない。
ワークスペースのファイルI/OもTokioを使い、ディレクトリ走査と一時ファイルの原子的な公開は
ライブラリの同期処理を `spawn_blocking` で実行する。

Tokioの[標準入力](https://docs.rs/tokio/latest/tokio/io/fn.stdin.html)はキャンセルできないブロッキング処理を使うため、拡張の終了処理を終えてから
`Runtime::shutdown_background` でランタイムを閉じる。CLI終了時に標準入力のEOFを再び待つことを防ぐ。
同期APIを維持するためのアダプターは設けていない。

`tests/process_lifecycle.rs` に11件を追加し、通常のEOF終了、応答途中の期限超過、通信のキャンセル、
プロトコル異常、終了処理の失敗、孫プロセスのパイプ保持、標準入力待ちのSIGINT・SIGTERMを確認した。
Sidecarへ書き込んだ直後の中断では、資源の確認を促す失敗結果になり、書き込みを再試行しないことも確認した。
`cargo test --locked --offline` は既存30件と合わせて41件成功。
`cargo clippy --locked --offline --all-targets -- -D warnings` と `cargo fmt --all -- --check` も成功。
`cargo build --release --locked --offline` が成功し、リリース版でデモの全17操作と終了コードを確認した。
Windows環境の実行試験は行っていない。追加レビューの問い合わせ・競合に関する3件は未修正である。

## 実装と確認結果

2026-09-25 に、次の Beta v1 Core の範囲を実装・確認した。
利用者の指示に従い、初期設定の拡張と抽出元は空。
ファイル観測、Markdown・テキスト・sidecar・コードの扱いはすべて外部拡張が担当する。
Rust を含む言語固有の実装は Core に内蔵していない。

| 完成条件 | 実装・検証 |
| --- | --- |
| 固定済み Observation、領域の解決・同一性・包含 | 観測と解釈設定を保存。資源の変更・削除や現在の設定の破損後も、保存済み観測を再利用する。複数資源の領域も検証済み。 |
| 外部拡張だけで観測・形式処理 | JSON Lines のプロセス API を実装。一回の CLI 操作ではプロセスを再利用し、固定済み内容をファイルで渡す。未登録の形式・scheme はエラー。 |
| 宣言・根拠・名前・集約・競合 | 意味上の起点と記載場所を分離。同一の領域と注釈を持つ宣言を集約し、根拠と競合、未解決状態を保持する。 |
| show・links・check と条件式 | 注釈値、保存された selector、領域の同一性・包含を AND・OR・NOT で問い合わせる。判定不能と不一致、取得失敗と完全な空結果を区別する。 |
| add・set・remove・copy と patch | sidecar の実ファイルで注釈の保存・変更・削除と別形式への複写を確認。変更案の再抽出、適用前の再観測、条件付き書き込み、適用後の再観測を行う。 |
| extension list・show・test と注釈検査 | 拡張情報、比較法則の検査、任意の注釈の検査、過去の snapshot を使う検査を実装。法則違反と実行失敗を区別する。 |
| JSON 入出力・導入・拡張仕様・例 | CLI と JSON request が同じ操作を呼ぶ。[README](README.md)、[API仕様](EXTENSIONS.md)、[実行可能なデモ](examples/demo.py)を用意。 |
| 実ファイル・異常系・法則・CLI の確認 | 下記の26テスト、リリース版のデモ、インストール試験と実用規模の試行を完了。[確認ノート](REVIEW_NOTES.md)の各項目を照合した。 |

## 検証記録

macOS、Rust/Cargo 1.97.1、Python 3 の環境で確認した。

- `cargo test --locked --offline`: 単体3件、CLI結合14件、外部拡張結合9件、計26件成功。
- `cargo clippy --locked --offline --all-targets -- -D warnings`、`cargo fmt --all -- --check`: 成功。
- `cargo build --release --offline`: 成功。リリース版でデモの全17操作と終了コードを確認。
- `cargo install --path . --locked --offline --root <一時ディレクトリ>`: 成功。インストールしたCLIの版表示も確認。
- リリース版と試験用 Python 拡張による合成データ1,000リンクの問い合わせは、約0.72秒、JSON出力約974 KBで全件取得を確認。このデータと環境での測定値であり、他形式の性能を保証する値ではない。

異常系では、拡張の不正応答・版不一致・入力送信と応答待ちのタイムアウト、
SIGINTによる拡張プロセス群の終了、観測とpatchの破損、古いpatch、対象パスの差し替え、
注釈を変更する不正な編集案、書き込みを行わず成功を返す拡張を検証した。
比較の法則は空領域・複数資源・比較不能な組・別拡張間を含めて確認した。

## 提供範囲

実行可能な例は、JSONの行集合とJSON/JSON Linesのsidecarを扱う試験用の外部拡張。
テストとデモが明示的に登録し、通常の初期設定には含めない。
個別言語や外部サービス用の製品拡張は、この実装には含めていない。
サービスの書き込みは同じAPIの条件付き更新で扱えるが、実サービスへの接続試験は行っていない。
外部編集者との同時書き込みの保証は、各providerの実装に従う。
