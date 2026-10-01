# Monika

文書・コード・データなどの間の Link を表示・問い合わせ・検査・編集する CLI。
目的は [基本コンセプト](concept_explain.md)、検証結果は [IMPLEMENTATION.md](IMPLEMENTATION.md) に記載する。

Link と注釈の正本は、Monika が直接管理する **`monika.links.json`（Sidecar）**。
追加・編集・削除と、保存された値による問い合わせは外部拡張なしで使える。
Resource の観測、Observation からの Region の解決・比較、注釈の意味に応じた検査は外部拡張が提供する。
初期状態の拡張は空で、Markdown・コード・バイナリなどへの対応は必要な拡張を登録して使う。

## ビルドと試用

Rust の Cargo でビルドする。結合テストとデモには Python 3 と POSIX 環境が必要。

```sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
python3 examples/demo.py
```

デモは一時ディレクトリを作り、[試験用の外部拡張](tests/fixtures/sets_extension.py)を登録する。
JSON のデータ行を Region として扱い、Sidecar の編集、問い合わせ、検査、変更案の保存と適用、
外部への生成、Observation からの取り込み、保存した Observation を使った解決を検証する。
`--directory /tmp/monika-demo` を渡すと、新規作成した作業ディレクトリを残せる。
試験用拡張は Core に同梱される形式対応ではなく、テストとデモの設定から起動する。

インストールには `cargo install --path . --locked` を使う。
拡張の作成・登録方法と JSON 操作は [EXTENSIONS.md](EXTENSIONS.md) に記載する。

## Sidecar と Link

`monika init` は `monika.json` と空の `monika.links.json` を作る。
Sidecar は `version`、`revision`、`links`、`origins`、`observations` を持つ JSON。
`links` に Link を保存し、取り込み元を `origins` に記録する。
取り込み元の Observation メタデータは `observations` に ID ごとに一度保存する。
`revision` は変更時に増える。Sidecar と設定は Git などで利用者が管理する。

追加・置換に使う Link の例:

```json
{
  "id": "design-to-data",
  "source": {"extension": "sets", "resource": "rows.json", "query": "0"},
  "reference": {"target": {"name": "subset"}},
  "annotation": {"relation": "supports", "review": {"owner": "利用者"}}
}
```

`id` は必須で、一件の Link を識別する。`annotation` は任意の JSON 値で、省略時は `null`。
登録時に名前付き指定を展開し、RegionSelector の `resource` を URI に正規化する。
`query` と注釈の内部の意味は拡張や利用者が定める。
正規化後の起点・参照先・注釈がすべて一致する Link の二重登録と、ID の重複はエラーになる。
Region として同じかどうかの検査には、`check --duplicates` を使う。

## 操作

`--root DIR` はプロジェクトのルート。省略時は現在地から上に `monika.json` または `.git` を探す。
Core が受け取る相対 Resource パスと `--patch-out` の相対パスは、このルートを基準にする。
パスの親ディレクトリは OS に解決させ、URI には `url` クレートを使う。
保存済み URI は、過去の状態での再実行でも同じアドレスとして扱う。

| コマンド | 操作 |
| --- | --- |
| `init` | 設定と空の Sidecar を作る |
| `show TARGET` | Region の内容を表示する |
| `links [--where EXPR]` | Sidecar の Link を選び、取り込み元とともに返す |
| `check [--where EXPR] [--duplicates]` | 選んだ Link の両端と注釈を検査する |
| `link add --link FILE` | Link を登録する |
| `link set --link FILE` | 指定した ID の Link 全体を置換する |
| `link remove ID` | Link とその取り込み元の記録を削除する |
| `import RESOURCE --extension EXT` | Observation から拡張が読み取った Link を登録する |
| `generate RESOURCE --extension EXT [--where EXPR]` | 選んだ Link の外部表現を生成する |
| `apply FILE` | 保存した変更案を照合して適用する |
| `extension list / show NAME` | 拡張の操作・版・比較 domain を確認する |
| `extension test --cases FILE` | RegionSelector 群の解決と比較の法則を検査する |
| `request [FILE]` | JSON で同じ操作を実行する |

入力ファイルの `-` は標準入力。`request` はファイル省略時も標準入力を読む。
入力ファイルの相対パスは、CLI を実行したディレクトリを基準にする。
`link add/set/remove`・`import`・`generate` は通常その場で適用する。
`--patch-out FILE` を付けると適用前の変更案を保存する。既存の保存先は上書きしない。
適用時には元の内容を照合する。Sidecar の書き込みは OS のロックと一時ファイルからの置換を使い、
別の Monika 操作が行った変更を上書きしない。直接編集する場合は Monika の書き込みと同時に行わない。
`.monika-sidecar.lock` は Git の対象から外し、実行中に削除しない。

取り込みは指定した Resource の Observation を入力にする。Link の内容が既存の同じ ID と一致すれば
取り込み元を記録し、食い違う場合や一部の解釈に失敗した場合は全体を適用しない。
同じ内容・状態・拡張の版・位置を再度取り込んでも記録は増えない。
生成したファイルや取り込み元の変化は、明示的な操作で Sidecar へ反映する。

生成は選んだ Link をすべて含む内容への置換を提案する。Core は提案を Observation として固定し、
同じ拡張で読み戻して ID・RegionSelector・注釈の保持を確認する。
書き込みは Resource の提供元が条件付きで行い、Core が再観測して結果を確かめる。
保存した生成案の適用時には、元にした Sidecar の状態も照合する。
生成で Sidecar の Link が変更されることはない。

## 問い合わせ

次の例はデモの拡張とデータで実行できる。

```sh
monika links --where 'id == "design-to-data"'
monika links --where 'annotation.relation == "supports" and target within region("@subset")'
monika links --to @subset --within
monika links --where 'not exists(annotation.review) or annotation.review.owner == "利用者"'
```

条件は Sidecar の一件の Link に対して評価する。`and`・`or`・`not` と括弧を使え、
優先順位は `not`、`and`、`or` の順。存在しない項目と明示的な `null` を区別する。
注釈の値は `annotation["自由な項目"][0]` のようにも指定できる。

`source`・`target` は `region(...)` で解決した Region と比較でき、`within`・`contains` は自分自身を含む。
RegionSelector の指定自体を照合する場合は `selector(...)` と `==`・`!=` を使う。
`--from`・`--to` は Region の同一性を選び、`--within` を付けると指定した Region 内も含む。
複数の短縮オプションと `--where` は AND で結ぶ。

ID・注釈だけの問い合わせでは Resource を取得しない。Region の比較が必要になったときに解決する。
判定できない Link は理由とともに `undetermined` に残す。
偽を含む AND は偽、真を含む OR は真として確定できる。
`check --duplicates` は、選んだ Link と Sidecar の他の Link の間で、両端の Region と注釈が同じかを調べる。
重複した ID の組を返し、Link を自動でまとめない。
クエリ言語自体の変更は [検討案](CLI_PROPOSAL.md) に記載する。

## Observation と結果

現在の状態に対する解決を対象とし、操作中は関連ファイルが外部から変更されないと仮定する。
拡張は Observation に加えて LS やプロジェクト設定なども参照できる。
解決・比較のキャッシュと更新は拡張が管理する。

現行実装では、各操作は使用した Observation と設定を `.monika/` に保存し、`snapshot` ID を返す。
Sidecar を読んだ操作では、その状態も保存する。
`--snapshot ID` は Core が渡す Observation・Sidecar・設定を保存済みのものにする。
Core は未取得の Resource を観測し直さず、現在の Sidecar も読み直さない。
拡張が使う外部状態は拡張自身が扱う。

`.monika/` の削除は Sidecar の Link を変更しない。ただし削除した過去の Observation の内容と snapshot は復元できない。
Sidecar に保持する取り込み元のメタデータは、Observation の内容そのものを複製するものではない。
編集の結果には適用後の `snapshot` と、適用前の `data.base_snapshot` がある。
Resource への生成後の snapshot は生成先の Observation と Sidecar の状態を含む。
保存した変更案の適用には、そこで参照した入力の保存済み内容も必要になる。
履歴を VCS に委ねる設計への変更点は [実装との差分](IMPLEMENTATION.md#設計との差分) に記載する。

`--json` と `request` の応答:

```json
{"schema":1,"ok":true,"complete":true,"snapshot":"...","data":{}}
```

`ok` は操作を結果として返せたか、`complete` は必要な評価・検査を完了できたかを示す。
`links` は `links`・`origins`・取り込み元の `observations`・`undetermined`・`diagnostics` を返す。
`check` はその選択に加えて `findings`・`duplicates`・`valid` を返す。
取得・解釈・比較の失敗は `diagnostics` に残り、検査で確認した不整合と区別する。

| 終了コード | 意味 |
| --- | --- |
| 0 | 操作成功、または検査合格 |
| 1 | 注釈検査の不合格・重複の検出、または拡張の法則違反 |
| 2 | 不正入力、取得・解釈・比較の失敗、判定不能、変更案の適用失敗 |

POSIX で SIGINT・SIGTERM を受けると、拡張のプロセス群を終了してそれぞれ 130・143 を返す。
書き込み中に中断した場合は現在の状態を確認する。書き込みを自動再試行しない。
EOF 待ちや拡張との通信中も中断でき、通常終了時は拡張に EOF を送って終了を確認する。
