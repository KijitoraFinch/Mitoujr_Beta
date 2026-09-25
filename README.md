# Monika

文書・コード・データなどの間のリンクを、根拠のある宣言として表示・問い合わせ・検査・編集するCLI。
目的は [基本コンセプト](concept_explain.md)、開発状況は [IMPLEMENTATION.md](IMPLEMENTATION.md) に記載する。

**形式の対応はすべて外部拡張が提供する。** 初期状態の拡張と抽出元は空。
ファイルの観測も、Markdown・コード・sidecarの解釈も、必要な拡張を明示的に登録して使う。
正本は利用者が管理する資源であり、sidecarを使う場合には起点・参照先・任意の注釈をsidecarに保存する。

## ビルドと試用

RustのCargoでビルドする。結合テストとデモにはPython 3とPOSIX環境が必要。

```sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
python3 examples/demo.py
```

デモは一時ディレクトリを作り、[試験用の外部拡張](tests/fixtures/sets_extension.py)を登録する。
JSONのデータ行を領域として扱い、別のsidecarにリンクと注釈を保存する。
表示・問い合わせ・検査・追加・置換・複写・削除・patch適用・観測の再利用を実行し、結果を検証する。
`--directory /tmp/monika-demo` を渡すと、新規作成した作業ディレクトリを残して確認できる。
この拡張はCoreに組み込まれず、デモやテストの設定からのみ呼び出される。

通常のCLIとしてインストールする場合は `cargo install --path . --locked` を使う。
外部拡張の作成・登録方法は [EXTENSIONS.md](EXTENSIONS.md) を参照。

## 操作

`--root DIR` はプロジェクトのルートを指定する。省略時は現在地から上に
`monika.json` または `.git` を探す。selector内の相対表記は拡張が解釈する。
変更案の保存先を相対パスで指定した場合はプロジェクトルートが基準になる。

| コマンド | 操作 |
| --- | --- |
| `init` | 拡張を含まない `monika.json` を作る |
| `show TARGET` | 領域の内容を表示する |
| `links [--where EXPR]` | 宣言・記載の根拠・未解決状態・競合を取得する |
| `check [--where EXPR]` | 起点・参照先・競合と、設定した拡張による注釈検査を行う |
| `link add --in RESOURCE --extension EXT --declaration FILE` | 宣言を追加する |
| `link set --in RESOURCE --extension EXT --id ID --declaration FILE` | 宣言全体を置換する |
| `link remove --in RESOURCE --extension EXT --id ID` | 宣言の記載を削除する |
| `link copy --from RESOURCE --from-extension EXT --id ID --in DEST --extension DEST_EXT` | 別の資源・形式へ複写する |
| `apply FILE` | 保存した変更案を照合して適用する |
| `extension list / show NAME` | 外部拡張の操作・版・比較領域を確認する |
| `extension test --cases FILE` | selector群を実際に解決し、決定性と比較の法則を検査する |
| `request [FILE]` | JSONで同じ操作を実行する |

入力ファイルに `-` を指定すると標準入力を読む。`request` はファイル省略時も標準入力を読む。
`--id` の代わりに `--at '{"index":0}'` のように、取得した根拠の `location` を渡せる。
同じIDの記載が資源内に複数ある場合は `--at` で選ぶ。locationの内容は各拡張が定める。

各 `link` 操作はその場で適用する。`--patch-out FILE` を付けると、正本への書き込みを行わず
確認可能な変更案を保存する。保存先の既存ファイルは上書きしない。
適用では元の観測・内容・資源の版を照合し、書き込み後にも読み戻して結果を確認する。
失敗した書き込みを自動再試行することはない。

## 宣言と対象指定

宣言のJSONの例。sidecarの外形と記法は、その形式を扱う拡張が定める。

```json
{
  "id": "design-to-data",
  "source": {"extension": "sets", "resource": "rows.json", "query": "0"},
  "reference": {"target": {"name": "subset"}},
  "annotation": {"relation": "supports", "review": {"owner": "利用者"}}
}
```

`annotation` は任意のJSON値。項目名や構造にCore固有の意味を付けない。
省略時は `null` となる。`id` は任意で、同じ宣言を複数箇所に記載したときの競合の識別に使う。
起点・参照先・注釈が同じ意味の記載は一つのリンクに集約し、元の記載をすべて `occurrences` に残す。
同じIDが異なる宣言値に現れる場合は `conflicts` に示す。
解決や比較が完了せず、意味の違いを確認できない競合候補には `verified:false` を付ける。

CLIの対象指定にはJSON、`EXTENSION:RESOURCE#QUERY`、または `@NAME` を使う。
`query` の意味は拡張が決める。名前付き指定は `monika.json` の `names` から展開する。
複数資源の領域は、拡張が定めたJSON queryで指定できる。

## 問い合わせ

次の例はデモの拡張とデータで実行できる。

```sh
monika links --where 'annotation.relation == "supports" and target within region("@subset")'
monika links --to @subset --within
monika links --where 'not exists(annotation.review) or annotation.review.owner == "利用者"'
```

`and`・`or`・`not` と括弧を使える。優先順位は `not`、`and`、`or` の順。
`annotation` の値はドットまたは `annotation["自由な項目"][0]` で選び、`==`・`!=` でJSON値と比較する。
存在しない項目と明示的な `null` は区別する。`id == "名前"` も指定できる。

`source`・`target` は `region(...)` で解決した領域と比較でき、`within`・`contains` は自分自身を含む。
保存されたselectorの表記を選ぶ場合は `selector(...)` と `==`・`!=` を使う。
`--from`・`--to` は領域の同一性を選び、`--within` を付けると指定領域内も含む。
複数の短縮オプションと `--where` はANDで結ぶ。

参照や比較が解決できず条件を判定できないリンクは `undetermined` に残す。
偽が含まれるANDは偽、真が含まれるORは真として確定できる。
抽出に失敗した資源も `diagnostics` に残り、完全な空結果と区別できる。

## 観測と結果

各操作は、使用した観測を `.monika/` に保存して `snapshot` IDを返す。
`--snapshot ID` を指定すると保存済み内容と当時の設定を使い、資源を再取得しない。
そのsnapshotに含まれない資源は解決できない。抽出元の一覧も保存するため、
`links` の再実行時に追加・削除されたファイルへ勝手に切り替わらない。
拡張のrevisionが保存した版と異なる場合はエラーになる。

編集成功時の `snapshot` は書き込み後の観測で、`data.base_snapshot` は編集前の観測。
書き込み後のsnapshotは対象資源を観測したもので、プロジェクト全体の走査結果ではない。
`.monika/` は派生物として削除・再構築できる。ただし削除した過去のsnapshotや
その観測を参照する保存済みpatchは、元の観測がないため再利用できなくなる。

`--json` と `request` は次の形で結果を返す。

```json
{"schema":1,"ok":true,"complete":true,"snapshot":"...","data":{}}
```

`ok` は操作を結果として返せたか、`complete` は必要な走査・抽出・問い合わせ・検査を完了できたかを示す。
`links` の未解決endpointは各リンクに残る。条件判定に不要なら、それだけで走査の `complete` を偽にはしない。
`links` の領域情報にはdomainと観測IDを返し、内容と内部表現は `show` で取得する。
`check` は未解決endpointも検査失敗として示す。注釈は `checks` に設定した拡張が検査する。

| 終了コード | 意味 |
| --- | --- |
| 0 | 操作成功、または検査合格 |
| 1 | `check` で問題を検出、または `extension test` が法則違反を検出 |
| 2 | 不正入力、取得・解釈・比較などの失敗、判定不能な問い合わせ、変更案の適用失敗 |

POSIXでSIGINT・SIGTERMを受けた場合は、拡張のプロセス群を終了してそれぞれ130・143を返す。
書き込み中に中断した場合は、資源の現在の状態を確認してから次の操作を行う。

失敗結果は `ok:false` と `error` を含む。部分的な結果が返るときは `ok:true, complete:false` と
`diagnostics` / `undetermined` を確認する。検査で問題を検出した場合も、検査結果自体は `ok:true` になる。

JSON操作と拡張の入出力の詳細は [EXTENSIONS.md](EXTENSIONS.md) に記載する。
