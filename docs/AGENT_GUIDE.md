# Agent が開発中に Monika を使う

Monika は、将来の自分や別の Agent が、設計意図・仕様・実装・根拠を辿れるようにする CLI です。短い文書に残した知識と具体的な部分を Link で結び、関係の理由を注釈に記録します。確認した時点の内容を Capture に保存すると、次のセッションで変化を検査し、過去の根拠をそのまま引用できます。

## 対象に合う拡張を用意する

既存のプロジェクトでは `monika.json` と `monika extension list` / `show NAME` を確認してください。Resource の観測も拡張の役割で、ローカルファイルにも提供元の登録が必要です。ファイル・Markdown・コードの同梱拡張は今後別途作成します。対象に合う拡張を Agent 自身が作って登録できます。

RegionSelector の `query`、Region の `content`、同一性と包含の判定は拡張が定めます。解決中の拡張は必要な Resource を Core に要求し、Core が選んだ Observation を解釈します。現在と Capture の変化の判定には `content` だけを使うため、見直す必要がある情報をそこに含めます。LS など既存の仕組みも利用できます。

[拡張 API](../EXTENSIONS.md)、[Python の通信ヘルパー](../examples/extension_api.py) と `monika extension test --help` が実装の入口です。`extension test` の cases には RegionSelector と名前付き指定の両方を使えます。

## 記録し、辿り、確かめる

新しいプロジェクトは `monika init` で設定と Sidecar を作ります。Link と任意の JSON の注釈の正本は `monika.links.json`（Sidecar）で、通常の編集ツールで更新します。端点 `source`・`target` には RegionSelector または `{"name":"登録した名前"}` を指定します。ローカルの `resource` はプロジェクトルートからの相対パスで記録します。

対応を確認したら、その Link の端点を捕捉します。端点の `capture` に ID が記録され、他の手書き部分の表記は保たれます。

```sh
monika capture --link design-to-code --endpoint target
# 両端を見直した場合
monika capture --link design-to-code --endpoint both
```

変更する前には関係する知識を読みます。選択・変換・集計には jq を使い、パイプでは `pipefail` を有効にします。

```sh
set -o pipefail
monika links | jq -c 'select(.id == "design-to-code")' | monika read --at /source
monika links | jq -c 'select(.id == "design-to-code")' | monika read --at /target --captured
monika links | monika check
```

通常の `read` は現在の Region、`read --captured` は保存した解決結果を返します。`links` と Capture の読み取りは観測しません。`compare` は現在の二つの Region の同一・包含を返します。

`check` は両端の解決、Capture からの `content` の変化、設定した拡張の注釈検査を確認します。結果の `captures.source` / `captures.target` に `{id, changed}` が入り、変化は検査不合格です。selector と Capture の selector が一致しない場合はエラーになり、捕捉し直しが必要です。実装が仕様を満たすことは、テストやレビューも使って判断します。

終了コードは成功 0、検査不合格 1、実行失敗 2 です。標準出力は JSON、診断は標準エラーです。変わった Link ごとに対応を見直し、成立する場合だけ対象端点を捕捉し直します。全 Link の確認基準を一括で現在に置き換えません。

## 高価な Resource と保存データ

`monika.json` の `resources` で Resource ごとの現在の決め方を指定します。

```json
{"resources":{"src/auth.rs":"observe","service:requirements":"refreshed"}}
```

省略時の `observe` は操作ごとに観測します。`refreshed` は `refresh` で最後に採用した保存済み Observation を使い、未保存ならエラーになります。同じ操作中には同じ Resource の Observation を共有し、解決中の追加要求にも各 Resource の方針を適用します。

```sh
monika refresh service:requirements
monika refresh --all
```

`--all` は名前を展開した Sidecar の端点の `resource` のうち、`refreshed` に設定したものを更新します。拡張独自の query や内容から見つかる Resource は、直接指定して更新してください。実行中は関連する Resource・設定を変更せず、Sidecar 更新・refresh・GC を並行して実行しません。

`monika.json`・Sidecar・`.monika/` をコミットして共有します。Capture と保存済み Observation には Resource の内容が入り、機密情報も保存されます。コミット・共有前に内容を確認してください。

Link なしの引用用 Capture も作れます。

```sh
printf '%s\n' '{"name":"evidence"}' | monika capture
printf '%s\n' '"CAPTURE_ID"' | monika read --captured
monika gc --dry-run
monika gc --keep CAPTURE_ID
```

GC は Sidecar の端点から参照されない Capture と、現在として採用されていない Observation を削除します。文書などに ID を書いた独立した Capture は、GC のたびに `--keep ID` で保持します。複数ある場合はオプションを繰り返します。`.monika/` は共有する保存データなので、キャッシュとして削除しないでください。

具体的な入出力は [実行可能なデモ](../examples/demo.py) と各コマンドの `--help` を参照してください。
