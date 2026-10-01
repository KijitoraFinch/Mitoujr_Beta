# Beta v1 実装と検証

目的と方針は [DESIGNDOC.md](DESIGNDOC.md)、確認点は [REVIEW_NOTES.md](REVIEW_NOTES.md) に置く。
この文書は現在の実装と検証結果を記録する。過去のレビューの推測を仕様として引き継がない。

## 拡張によるキャッシュ管理（2026-09-28）

Core の RegionSelector ごとの解決結果キャッシュと、観測要求の集合を固定する検査を削除した。
解決・比較は拡張に要求し、各解決で渡した Observation を Region に記録する。
LS や設定ファイルなど、Observation 以外の入力を含めたキャッシュの管理は拡張が受け持つ。
現在の状態を対象とし、操作中は関連ファイルが外部から変更されないことを前提にする。
法則テストでは繰り返した解決を拡張の比較で照合する。読み取り記録の一致は要求しない。

実プロセスの試験で、Core が繰り返しの解決と通常のエラー後の新しい要求を拡張に渡すこと、
読み取りの集合が変わっても同じ意味の Region を扱えることを確認した。
拡張側のキャッシュについて、同じ内容を別の Observation として観測した場合の再利用、
Observation に含めないプロジェクト設定の変更と、対象の内容変更による再計算を確認した。
これは試験用拡張での検証であり、LS 実装の検証ではない。

## 設計との差分

履歴を VCS や Resource の提供元に委ねる設計に対し、現行実装には次の変更が必要である。

- Observation の保持を操作中の入力とキャッシュに整理する。
- snapshot の自動保存と、`--snapshot`・`check_plan.snapshot` による再実行を削除する。
- 保存した変更案の適用を、変更案に保持する情報と現在の状態の照合で完結させる。

## Sidecar の移行（2026-09-26）

Link と注釈の正本を `monika.links.json` に置き、Core が直接管理する。
一件の Link を必須の ID で識別する。外部拡張による宣言の走査、意味上の集約に依存した識別、
元の記載位置を指定する編集を廃止した。互換 API や旧設定の読み替えは設けていない。

Resource の観測と Observation の解釈は外部拡張が提供する。
`import` は指定した Observation から Link を登録し、取り込み元を保存する。
`generate` は選んだ Link の外部表現を提案し、Core が読み戻して保持を確認してから提供元へ適用させる。
Link の編集、取り込み、生成はすべて変更案として確認・保存できる。

Sidecar は Link、取り込み元、共有する Observation メタデータを分けて保持する。
名前付き指定と Resource の表記を登録時に正規化し、ID や定義の重複を検出する。
Sidecar の変更は内容の照合、標準ライブラリの OS ロック、tempfile の置換を通して適用する。
同じ入力の再取り込みは内容も revision も変えない。

問い合わせは一件の Link に対して評価し、Region が必要になったときに解決する。
`check --duplicates` は意味上の重複を明示的に検査し、ID を統合しない。
索引の再構築や Observation の保存領域の削除は、Sidecar の Link を変更しない。

## 検証

macOS で以下が成功した。Windows の実行試験は行っていない。

- `cargo test --locked --offline`：56 件（2026-09-28）。
- `cargo clippy --locked --offline --all-targets -- -D warnings`、`cargo fmt --all -- --check`。
- `cargo build --release --locked --offline` と `python3 examples/demo.py --monika target/release/monika`：全 21 操作が期待する終了コードで完了（2026-09-26）。現在のデモは CLI 結合テストでも確認。
- `git diff --check` と文書のローカル参照先の確認。

| 確認した性質 | 検証 |
| --- | --- |
| Core が正本を管理する | 拡張なしで追加・置換・削除・問い合わせ。保存領域 `.monika/` を削除しても Link を保持。Sidecar 自体の欠落はエラー。 |
| 正規化と整合性 | 名前付き指定と Resource の正規化、重複 ID・同一定義・壊れた取り込み元の参照を検出。 |
| Observation に基づく取り込み | 観測後に元の Resource を別の内容へ変えても、保存した Observation の Link を取り込む。 |
| 取り込みの一括適用と正規化 | 競合・不完全な解釈では全体を適用しない。複数 Link の Observation メタデータを共有し、再取り込みで増殖させない。 |
| 保存した Core 入力の利用（現行実装） | 試験用の Observation のみを解釈する拡張で、Sidecar・Resource・設定変更後に保存入力から同じ結果を取得。 |
| 安全な変更案の適用 | 改変した変更案、古い Sidecar、ロック中の書き込み、生成先の作成・差し替え、生成元の Sidecar の変更を検出。 |
| 生成時の保持と書き込み確認 | JSON と JSON Lines へ生成。注釈を失う生成と、成功を返して書き込まない提供元を拒否。 |
| 問い合わせと重複 | 一件の Link に条件を適用。意味上の重複は ID を保持したまま検出し、絞り込み外の相手も検査。 |
| CLI と拡張のライフサイクル | 標準入力待ち・送信詰まり・部分応答・提供元待ち・書き込み後の中断を検証。補助プロセスの終了も確認。 |

主な試験は [CLI](tests/cli.rs)、[外部拡張](tests/external_extensions.rs)、
[Observation の読み取り](tests/observation_reads.rs)、[プロセス](tests/process_lifecycle.rs) にある。
宣言の走査・集約、記載位置を指定する編集、不要になった比較キーと走査用依存を削除し、
試験用拡張・デモ・README・拡張 API の説明も新しい操作に統一した。

2026-09-26 のリリース版で、存在しない別々の Resource を指す Link から `id == "0"` で一件を選んだ測定:

| Link 数 | Resource 取得 | Observation の転送 | snapshot | 選択と保存の時間 |
| --- | --- | --- | --- | --- |
| 100 | 0 件 | 0 件 | 34,029 bytes | 約 15 ms |
| 600 | 0 件 | 0 件 | 201,529 bytes | 約 15 ms |

拡張プロセスも起動していない。snapshot は Sidecar の状態と設定を含む。
一回の Core 操作の計測であり、CLI の起動時間を含まない。
`cargo test --release --locked --offline --test observation_reads narrow_query_does_not_acquire_resources_or_start_extensions -- --nocapture`
で再現できる。

## 提供範囲

Resource の観測・Region の解釈・生成の実行例は試験用の外部拡張で提供する。
Markdown・コードなどの形式を内蔵する実装はない。
クエリ言語の採用に関する検討は [CLI_PROPOSAL.md](CLI_PROPOSAL.md) に残す。
現行の条件式は Sidecar の一件に適用し、従来の集約結果や複数の記載を条件の対象にしない。
