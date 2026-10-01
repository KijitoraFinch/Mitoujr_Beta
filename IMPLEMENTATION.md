# Beta v1 実装と検証

目的と方針は [DESIGNDOC.md](DESIGNDOC.md)、確認点は [REVIEW_NOTES.md](REVIEW_NOTES.md) に置く。

## 現在の実装

Link と注釈の正本は、Core が管理する `monika.links.json`（Sidecar）。
一件の Link を ID で識別し、登録時の正規化で名前付き指定・Resource の表記・重複を扱う。
問い合わせは一件の Link に条件を適用し、Region の比較が必要になったときに解決する。

Observation の内容は操作中に保持する。拡張に渡す一時ファイルは、プロセスの終了後に削除する。
応答には使用した Observation のメタデータを返し、取り込み元の記録は Sidecar に保持する。
履歴は VCS や Resource の提供元に任せ、Monika は履歴用に Observation を保存しない。

Resource の観測・Region の解決と比較・注釈検査・取り込み・生成は外部拡張が提供する。
Core は解決・比較を拡張へ要求し、その解決で渡した Observation を Region に記録する。
LS やプロジェクト設定を含む入力の管理と、キャッシュの再利用判断は拡張が受け持つ。
現在の状態を対象とし、操作中は関連ファイルが外部から変更されないことを前提にする。
比較の法則テストでは、繰り返した解決を拡張の比較で照合する。

編集・取り込み・生成は、その場で適用するか、変更案として保存できる。
変更案は照合に必要な情報と適用する内容を持ち、次の CLI 操作で現在の状態と照合して適用する。
Sidecar の書き込みは OS のロックと一時ファイルからの置換を使う。
生成は選択した Link を読み戻して確認し、提供元による条件付き書き込みと Core の再観測で結果を検証する。

## 検証

2026-10-01、macOS で以下を確認した。

- `cargo test --locked --offline`：61 件成功。デモの全 21 操作も CLI 結合テストに含む。
- `cargo clippy --locked --offline --all-targets -- -D warnings`。
- `cargo fmt --all -- --check`、`git diff --check`。

| 確認した性質 | 検証 |
| --- | --- |
| Sidecar が正本 | 拡張なしで追加・置換・削除・問い合わせ。キャッシュ削除後も Link を保持し、Sidecar の欠落・不正はエラー。 |
| 正規化と取り込み | 重複 ID・同一定義・取り込み元の参照を検査。固定済みの Observation から取り込み、競合・不完全な解釈では全体を適用しない。 |
| 現在の状態の解決 | 次の操作で Resource の変更を観測。追加の注釈検査入力も現在の状態から解決。対応外の履歴指定はエラー。 |
| 操作中の入力の保持 | 不在と取得失敗を区別。拡張の終了処理まで入力を保持し、正常終了・解釈失敗・期限切れ・SIGINT 後に一時ファイルを削除。 |
| 保存した変更案 | 取り込み元を削除しても Sidecar の変更案を適用。Observation の保存領域なしでテキスト・バイナリの生成案を適用。 |
| 変更時の照合 | 改変した変更案、古い Sidecar、ロック中の書き込み、生成先の作成・差し替え、生成元の Sidecar の変更を検出。注釈を失う生成や書き込まない提供元を拒否。 |
| 拡張によるキャッシュ | 読み取り集合が変わっても同じ意味の Region を扱う。試験用拡張が Observation 以外の設定変更と内容変更で再計算する。 |
| 問い合わせ | 100・600 件の Link から ID で一件を選ぶ際、Resource 取得・Observation 転送・拡張起動はすべて 0 件。判定不能は結果に残す。 |
| 通信と中断 | 標準入力待ち・送信詰まり・部分応答・提供元待ち・書き込み後の中断と、補助プロセスの終了を検証。 |

主な試験は [CLI](tests/cli.rs)、[外部拡張](tests/external_extensions.rs)、
[Observation の読み取り](tests/observation_reads.rs)、[プロセス](tests/process_lifecycle.rs) にある。

## 提供範囲

形式の対応は外部拡張を登録して使う。実行例には試験用の拡張を用いている。
LS 実装と Windows 上の実行は未検証。
クエリ言語の採用は [CLI_PROPOSAL.md](CLI_PROPOSAL.md) で検討する。
