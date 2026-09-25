# 外部拡張API v1

拡張は、JSON LinesでCoreと通信する独立した実行プログラム。
CLIの一回の実行中は、プロジェクトルート・拡張名・登録設定が同じプロセスを、
過去の観測や書き込み後の観測を扱う場合も再利用する。各メッセージを標準入力・標準出力の
JSON一行でやり取りし、送信時にflushする。ログは標準エラーへ出力する。

観測・書き込み以外の操作は、渡された固定済みの観測・設定・入力から結果を計算する。
資源の現在の内容を読む操作は `observe`、書き込む操作は `commit` が担当する。
同じ解釈の版と入力は同じ結果を返す。意味や解釈が変わる変更では `revision` を更新する。

## 登録

プロジェクトルートの `monika.json` で登録する。
コマンドはシェルを介さず起動し、作業ディレクトリにはルートを使う。

```json
{
  "version": 1,
  "extensions": {
    "sets": {
      "command": ["python3", "/absolute/path/to/sets_extension.py"],
      "revision": "test-v1",
      "schemes": ["file"],
      "timeout_ms": 30000
    }
  },
  "sources": [{"pattern": "**/*.links", "extension": "sets"}],
  "resources": [],
  "names": {},
  "comparators": {},
  "checks": [{"extension": "sets", "options": null}]
}
```

`schemes` は観測を担当するResource URIのschemeで、一つのschemeには一つの提供元を登録する。
登録がなければ、そのschemeの取得はエラーになる。`file` も明示的な登録が必要。
Coreはファイルパスを `file:` URIに変換できるが、その内容の観測と書き込みは拡張に委ねる。
パス指定では親ディレクトリを標準ライブラリで解決し、末尾のファイル名を保持してURIへ変換する。
親ディレクトリは存在している必要がある。URI指定は `url` クレートのURL規則に従い、
再利用時に現在のファイルシステムでアドレスを置き換えない。

`sources` はローカルファイルのglobと抽出拡張を指定する。走査ではプロジェクトのignore規則を使い、
`.git` と `.monika` を対象から除く。`resources` には
`{"resource":"scheme:address","extension":"name"}` の形で抽出元を直接列挙できる。
selectorの解決に必要な資源は、これらの抽出元に含まれなくても観測できる。

`names` は名前からtargetへの対応。targetはselector、または `{"name":"別の名前"}`。
`checks` は注釈検査に使う拡張と、その拡張が解釈する任意のoptions。
`comparators` は領域のdomainから比較を担当する拡張名への対応。
同じdomainを扱う拡張が複数ある場合には、比較の担当をここで明示する。

## 通信

要求の例:

```json
{"protocol":1,"id":1,"workspace":"/project","operation":"describe","input":null}
```

操作の最終応答は同じ `id` を持ち、`result` または `error` のどちらかを返す。
`result:null` も表現できる。`resolve` の途中では、下記の観測要求を送れる。

```json
{"protocol":1,"id":1,"result":{"name":"sets","revision":"test-v1","operations":["observe","resolve","compare","extract","edit","commit","check_plan","check"],"domains":["test.sets.v1"],"description":"JSON row sets and sidecars"}}
```

```json
{"protocol":1,"id":2,"error":"region not found"}
```

`describe` が返す名前・revisionは登録内容と一致すること。regionを扱う拡張は
`resolve`・`compare` を提供し、返す領域のdomainを宣言する。
他の機能も `operations` に列挙した操作だけが呼び出せる。

timeoutは入力送信から応答取得までの全体に適用する。不正な応答、終了、timeout、途中のキャンセルで通信が壊れた場合は
そのプロセスへの呼び出しを失敗させ、自動再起動・再試行をしない。
通常の `error` 応答はその操作の失敗として扱う。
`resolve` の期限には観測の要求と提供元の呼び出しも含まれ、往復のたびに期限を延長しない。

通常終了ではCoreが標準入力を閉じる。拡張はEOFを受けて1秒以内に終了する。
期限超過・中断・通信破損では、Coreが拡張のプロセス群を終了させ、終了結果を回収する。
拡張が起動した補助プロセスも、同じプロセス群に置く。親が先に終了していても残る補助プロセスを終了させる。
通常終了時の非ゼロ終了コードや終了期限の超過は、CLIの失敗として返す。

## 観測

`observe` のinputは `{"resource":"file:///project/a.json"}`。
返す値は実際の内容のbyte配列と、資源の状態。

```json
{"bytes":[91,49,93],"state":{"exists":true,"revision":"provider-owned-version"}}
```

存在しない資源は `bytes:[]`、`state.exists:false` を返す。
認証失敗や取得失敗は `error` を返す。不存在と取得失敗は別の状態として保存する。
`revision` は任意のJSON値で、提供元が条件付き書き込みのために使う情報を保持できる。

Coreは内容を固定して保存し、Observationを作る。
Observationには `id`・`resource`・`content_hash`・`size`・`observed_at`・`provider`・`state` がある。
`observed_at` はUnix epochからのnanosecondsの文字列。
同じ資源は一つのsessionで一度観測し、以後は保存した状態を使う。

解釈操作に渡す観測は次の形になる。

```json
{"observation":{"id":"...","resource":"...","content_hash":"...","size":3,"observed_at":"...","provider":"sets","state":{"exists":true,"revision":"..."}},"content_path":"/project/.monika/blobs/..."}
```

拡張は `content_path` から固定済みのbytesを読む。このファイルは読み取り専用の入力として扱う。
Coreが内容のhashを検証し、大きな内容を呼び出しごとにJSONへ埋め直す必要をなくしている。
`resource` が示す現在のファイルを解釈中に再取得しない。

## 領域の解決と比較

selectorの形は `{"extension":"name","resource":"任意の指定","query":任意のJSON}`。
`resource` は省略でき、`query` の省略値は `null`。
Coreはselectorの内容を解釈せず、指定された拡張へ渡す。

`resolve` のinputはselectorだけを持つ。

```json
{"selector":{"extension":"sets","query":{"resources":["file:///project/a.json"]}}}
```

拡張は必要な観測を、解決要求と同じ `id` の `read` メッセージで要求する。
値はResource URIの配列で、Coreは同じ順序の観測を `read_result` に入れて返す。
一つの観測要求の応答を受け取ってから、次の要求を送る。

```json
{"protocol":1,"id":2,"read":["file:///project/a.json"]}
```

```json
{"protocol":1,"id":2,"read_result":[{"observation":{"id":"...","resource":"file:///project/a.json","content_hash":"...","size":3,"observed_at":"...","provider":"sets","state":{"exists":true,"revision":"..."}},"content_path":"/project/.monika/blobs/..."}]}
```

Coreは要求された観測だけを渡す。取得済みの資源にはそのセッションの固定済み観測を使い、
未取得の資源は登録した提供元の `observe` で観測する。拡張は返された内容を読み、
そこから必要になった次の資源を要求できる。`dependencies` 操作やセッション全体の観測一覧は使わない。

不在も `state.exists:false` の観測として渡す。その資源が必要か、次の候補を調べるかは解決拡張が判断する。
取得や入力検証に失敗した場合は `{"protocol":1,"id":2,"read_error":"理由"}` を返す。
拡張は解決の `error` 応答を返して処理を終える。観測要求の失敗後に領域を返しても、Coreはその解決を成功扱いにしない。

同じ拡張が提供元も兼ねる場合、観測の応答を待つ間にCoreから別の `id` の `observe` 要求が届く。
その要求に応答すると、元の `id` の `read_result` が届き、解決を続けられる。
一つの解決の観測要求は直列で処理し、同じプロセスで別の解決を並行して起動しない。

[Pythonの通信ヘルパー](examples/extension_api.py)は、このやり取りと解決中の再参照のキャッシュを提供する。
`Extension(handler).run()` で起動し、handler内では次のように観測を読む。

```python
def handler(operation, data, context):
    # resolveの処理内。指定の解釈は拡張が担当する。
    item = context.read(resource_uri)
    if item["observation"]["state"]["exists"]:
        content = pathlib.Path(item["content_path"]).read_bytes()
```

`context.read_many(resource_uris)` で複数の観測をまとめて要求できる。
実行可能なhandler全体は [試験用拡張](tests/fixtures/sets_extension.py)にある。

解決が完了したら、`result` として領域の意味を返す。

```json
{"domain":"test.sets.v1","identity":[["observation-id",0]],"content":{"count":1}}
```

`domain` は共通の比較規則を使う領域表現を識別する。
`identity` と `content` は任意のJSONで、Coreは内部の意味を解釈しない。
Coreが読み取りを記録し、完成したRegionに `observations` として依存するObservation IDを付ける。
探索に使った設定や不在の観測も含む。拡張はこのフィールドを返さない。
任意の `equivalence_key` 文字列を付けると、大量の宣言を集約する際の比較を絞り込める。
同じdomain内で同一の領域は、キーを付ける場合には同じ値にする。
異なるキーなら同一ではないと判断でき、同じキーなら実際の比較を行う。
キーのない領域も比較操作で判定する。拡張テストはキーと同一性の整合も検査する。
`show` は `content` を表示する。文字列なら本文、それ以外ならJSONとして表示する。

解決ごとに要求した資源をsnapshotの `resolution_reads` に記録する。観測のメタデータと内容は
セッションに一度保存し、各解決にはその資源の参照だけを残す。後で別の資源が加わっても、
保存したselectorの再解決では元の依存だけを使う。再実行で依存を増減させる解決はエラーになる。
未解決のselectorをsnapshotで解決する場合も、そこに保存された資源だけを要求できる。

`compare` のinputは `{"left":Region,"right":Region}`。
返す値は `"equal"`・`"contains"`・`"within"`・`"incomparable"` のいずれか。
`contains` は左が右を真に包含すること、`within` はその逆。同一性は `equal` で返す。
問い合わせの包含条件では `equal` も含める。失敗は `error` として返す。
異なるdomainの比較規則が共有されていない場合、Coreは比較失敗として扱う。

比較は [拡張機能の法則](extension_concept.md) を満たす。
`extension test --cases FILE` は実際の呼び出しを繰り返し、決定性、反射性、対称性、
反対称性、推移性を検査する。caseはselectorのJSON配列で、失敗箇所は配列のindexで示す。
同じdomainを扱う別拡張のselectorも同じcase群で検査できる。

## 宣言の抽出

`extract` には一つの観測と `content_path` を渡す。返す値:

```json
{
  "occurrences": [{
    "declaration": {
      "id":"example",
      "source":{"extension":"sets","resource":"rows.json","query":"0"},
      "reference":{"target":{"name":"subset"}},
      "annotation":{"review":{"owner":"利用者"}}
    },
    "evidence":{
      "resource":"file:///project/design.links",
      "observation":"observation-id",
      "extension":"sets",
      "location":{"index":0},
      "writable":true
    }
  }],
  "diagnostics":[]
}
```

宣言は起点の意味と記載場所を分ける。evidenceの資源・観測・拡張は入力に対応すること。
`location` の構造は拡張が定め、後の編集にそのまま渡す。
抽出したselectorは記載場所から切り離しても解釈できる指定にする。
例えば文書内の相対リンクは、文書の場所を反映したresourceやqueryとして返す。
同じdeclared IDを持つ記載も、それぞれoccurrenceとして返す。

部分的に抽出できたときは取得できたoccurrencesと、
`{"stage":"extract","subject":"対象","message":"理由"}` のdiagnosticsを返す。
解釈できなかった宣言を、完全な空結果として返さない。

## 注釈の検査

検査をする拡張は `check_plan` と `check` を提供する。
各inputには `link`（集約した宣言・領域・根拠）と、設定の `options` を渡す。
Linkの宣言値は `link.occurrences[*].declaration`、解決済みの領域は
`link.source.region` / `link.target.region` にある。未解決endpointは `status:"unresolved"` と `error` を持つ。

`check_plan` は追加で必要な領域の配列を返す。

```json
[{"name":"baseline","target":{"extension":"sets","resource":"rows.json","query":"2"},"snapshot":"saved-snapshot-id"}]
```

`snapshot` を省略した場合は現在の観測、指定した場合はその保存済み観測を使う。
名前付きtargetは、それぞれのsnapshotの設定で展開する。
追加の領域が不要なら `[]` を返す。
Coreが領域を解決して `check` のinputに `regions:{"baseline":Region}` を加える。
この仕組みで、注釈に保存した過去の基準との比較を行える。

`check` は判定の配列を返す。

```json
[{"code":"count","status":"fail","message":"Expected 2, observed 3"}]
```

statusは `pass`・`fail`・`not_applicable`。Coreは注釈の項目名や意味を決めない。
検査に必要な入力の取得・解釈の失敗は、判定結果と区別してdiagnosticsに残す。

## 編集と書き込み

形式を編集する拡張は `extract` と `edit` を提供する。
`edit` のinputは `observation`、`content_path`、`change`。
新規作成の場合のObservationは `state.exists:false` で、内容は空。

changeの形式:

```json
{"op":"add","declaration":{}}
```

```json
{"op":"set","location":{"index":0},"declaration":{}}
```

```json
{"op":"remove","location":{"index":0}}
```

上の空オブジェクトは宣言値を入れる位置を示す。返す値は
`{"bytes":[変更後のbyte列],"location":追加・置換した記載のlocation}`。
削除の場合のlocationは `null`。editは正本に書き込まず、変更後の内容を返す。
Coreは提案した内容を同じ拡張で再抽出し、対象指定・注釈を保持して要求した宣言が現れることと、
宣言数が要求した操作に対応することを確認する。複写も追加と同じ編集操作を使う。
正本の他の内容を維持する責任は、その形式を扱う編集拡張が持つ。

資源の書き込みを扱う提供元は `observe` と `commit` を提供する。
editorとproviderは別の拡張でもよい。Coreが保存済み内容と再観測した状態を照合した後、
providerへ次を渡す。

```json
{
  "resource":"file:///project/design.links",
  "expected":{"content_hash":"old-sha256","state":{"exists":true,"revision":"old-version"}},
  "replacement":{"content_hash":"new-sha256","bytes":[]},
  "patch_id":"..."
}
```

providerは自身の書き込み時点にもexpectedを確認し、条件が一致した場合に置換する。
例えばサービスの条件付き更新を使う。ファイルを扱う試験用拡張はプロセス間のロックと
再照合を行い、一時ファイルから置換する。外部編集者も含む同時更新の保証は提供元の機構に従う。

成功時は `{"content_hash":"new-sha256","state":{"exists":true,"revision":"new-version"}}` を返す。
Coreはさらに独立した観測で内容と状態を確認する。応答の喪失や異常終了時は書き込みが
実行された可能性があるので、現状を取得して確認する。Coreから自動で再試行しない。

## CLIと同じJSON操作

`monika request` は次の形のオブジェクトを一つ読み、[READMEの応答形式](README.md#観測と結果)で返す。
`--root`・`--snapshot` は通常のCLIと共通。

| op | 追加のフィールド |
| --- | --- |
| `init` | なし |
| `show` | `target` |
| `links` / `check` | 任意の `where` |
| `extension_list` | なし |
| `extension_show` | `name` |
| `extension_test` | `selectors` 配列 |
| `edit` | `destination:{resource,extension}`、`change`、任意の `patch_out` |
| `apply` | 保存した `patch` オブジェクト |

editのchangeは `add`・`set`・`remove`・`copy`。
`add` は `declaration`、`set` は `pick` と `declaration`、`remove` は `pick`、
`copy` は `from:{resource,extension}` と `pick` を持つ。
pickは `{"id":"name"}` または `{"location":任意のJSON}` のいずれか。
patch_outの省略時は適用し、指定時は保存する。

whereの構造化表現は次のとおり。

```json
{"op":"and","args":[
  {"op":"compare","field":"annotation.relation","relation":"equal","value":{"kind":"json","value":"supports"}},
  {"op":"compare","field":"target","relation":"within","value":{"kind":"region","value":{"name":"subset"}}}
]}
```

booleanは `and` / `or` と `args`、`not` と `arg`。
存在判定は `{"op":"exists","field":"annotation.review"}`。
relationは `equal`・`not_equal`・`within`・`contains`、operandのkindは `json`・`region`・`selector`。
CLI文字列の条件式と同じ評価処理を使う。
