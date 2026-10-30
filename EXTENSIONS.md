# 外部拡張 API v1

Link と注釈の正本は Core が管理する Sidecar です。外部拡張は Resource の観測、Observation からの Region の解決、Region の比較、注釈の検査を提供します。同梱のファイル・Markdown・コード拡張はまだありません。`tests/fixtures/sets_extension.py` は試験用の作例です。

## 登録と Resource の表記

プロジェクトルートの `monika.json` に登録します。コマンドはシェルを介さず起動し、作業ディレクトリはルートです。

```json
{
  "version": 1,
  "extensions": {
    "sets": {
      "command": ["python3", "extensions/sets.py"],
      "revision": "test-v1",
      "schemes": ["file"],
      "timeout_ms": 30000
    }
  },
  "names": {"all": {"extension":"sets", "resource":"rows.json", "query":"0"}},
  "resources": {"expensive.json":"refreshed", "rows.json":"observe"},
  "comparators": {"test.sets.v1":"sets"},
  "checks": [{"extension":"sets", "options":null}]
}
```

`schemes` は観測を担当する URI scheme です。一つの scheme に一つの提供元を登録し、`file` も明示的に登録します。`names` は名前から RegionSelector または `{"name":"別の名前"}` への対応です。`checks` は注釈検査の拡張と、その拡張が解釈する任意の `options` です。`comparators` は Region の domain から比較担当の拡張への対応で、同じ domain を扱う拡張が複数ある場合に指定します。

展開は名前を RegionSelector に置き換える処理、正規化は `resource` の表記をそろえる処理です。Core はどちらも Resource の内容を取得せずに行います。ローカル Resource はルートからの相対パスを正規形とし、`.`・`..` を字句的に処理します。親ディレクトリの存在やシンボリックリンクを調べません。ルート外のパスはエラーです。絶対パスと `file:` URI も入力できますが、selector・Observation・出力・Capture の `resource` は相対パスになります。先頭の要素に `:` があるローカル名は `./colon:name` と指定します。パスは UTF-8 で表現します。

ローカルパスを `file:` URI に変換するのは提供元へ観測を要求するときだけです。その他の URI は `url` クレートで正規化し、fragment は RegionSelector の `query` に分けます。Core は `query` の内部を解釈しません。複数 Resource を query に含む形式も、拡張側でルート相対パスを使うと共有できます。

## 通信と寿命

拡張は JSON Lines で Core と通信する独立した実行プログラムです。一回の CLI 操作では、ルート・拡張名・登録設定が同じプロセスを再利用します。送信時に flush し、ログは標準エラーへ出します。

```json
{"protocol":1,"id":1,"workspace":"/project","operation":"describe","input":null}
```

最終応答には同じ `id` と `result` または `error` を返します。`result:null` も表現できます。

```json
{"protocol":1,"id":1,"result":{"name":"sets","revision":"test-v1","operations":["observe","resolve","compare","check"],"domains":["test.sets.v1"],"description":"Select numeric rows; query is a minimum value string; content is {count:number}."}}
```

```json
{"protocol":1,"id":2,"error":"region not found"}
```

`describe` の名前と `revision` は登録内容と一致させます。`operations` に宣言できるのは `observe`・`resolve`・`compare`・`check` です。Region を扱う拡張は `resolve` と `compare` を提供し、返す domain を宣言します。注釈の検査だけの拡張は `check` だけを提供できます。`description` に query・content の形と具体例を記します。意味や解釈を変えるときは `revision` を更新します。

期限は入力送信から応答取得までに適用され、解決途中の Resource 要求と別プロセスへの観測も含みます。往復ごとに期限を延長しません。通信破損・期限超過・中断ではプロセス群を終了し、自動再起動・再試行をしません。通常の `error` 応答はその操作の失敗です。

通常終了では Core が標準入力を閉じ、拡張は EOF から 1 秒以内に終了します。補助プロセスも同じプロセス群に置いてください。Core は補助プロセスも終了させてから一時ファイルを削除します。終了失敗は CLI の失敗として返します。[Python の通信ヘルパー](examples/extension_api.py) はこの要求・応答と解決途中の Resource 要求を扱います。

## 観測と現在の Observation

各 Resource の観測方針は `resources` に指定します。省略値は `observe` です。

- `observe`：その操作で最初に必要になったときに観測します。
- `refreshed`：`monika refresh` が最後に採用した保存済み Observation を使います。通常の操作では提供元を呼ばず、未保存ならエラーです。

一操作中の同じ Resource には一つの Observation を共有します。解決中に要求された別の Resource にも、その Resource 自身の方針を適用します。

提供元の `observe` の入力例です。

```json
{"resource":"file:///project/rows.json","content_path":"/tmp/monika-observe-..."}
```

提供元は観測時点の実際の内容を、Core が用意した `content_path` に生の bytes として書き、書き込みを完了してから次を返します。

```json
{"state":{"exists":true,"revision":"provider-owned-version"}}
```

Resource 自体は変更しません。存在しない Resource は空の内容と `state.exists:false` を返します。認証失敗・取得失敗は `error` にします。`revision` は提供元が定める任意の JSON で、必須です。ETag などによる取得の省略は提供元内部で扱います。内容を JSON の整数配列では転送しません。

Core は内容を固定し、`id`・`resource`・`content_hash`・`size`・`observed_at`・`provider`・`state` を持つ Observation を作ります。時刻は Unix epoch からの nanoseconds の文字列です。`refreshed` はメタデータと生の内容を保存します。通常の `observe` は操作中だけ保持します。

## Region の解決

RegionSelector は `{"extension":"name","resource":"rows.json","query":任意のJSON}` です。`resource` は省略でき、`query` の省略値は `null` です。`resolve` の入力は展開・正規化済みの selector です。

```json
{"selector":{"extension":"sets","resource":"rows.json","query":"2"}}
```

拡張は必要な Resource を、解決要求と同じ ID の `read` メッセージで要求します。ルート相対パスと URI を使えます。Core は同じ順序で Observation を返します。

```json
{"protocol":1,"id":2,"read":["rows.json","service:experiment"]}
```

```json
{"protocol":1,"id":2,"read_result":[{"observation":{"id":"...","resource":"rows.json","content_hash":"...","size":7,"observed_at":"...","provider":"sets","state":{"exists":true,"revision":"..."}},"content_path":"/tmp/monika-observation-..."},{"observation":{"id":"...","resource":"service:experiment","content_hash":"...","size":0,"observed_at":"...","provider":"service","state":{"exists":false,"revision":null}},"content_path":"/tmp/monika-observation-..."}]}
```

拡張は `content_path` を読み取り専用の入力として扱います。Core は内容の hash を検証し、拡張終了までファイルを保持します。一回の応答を受けてから次の要求を送ってください。返された内容から必要な次の Resource を見つけて要求できます。失敗は `{"protocol":1,"id":2,"read_error":"理由"}` で返し、拡張がそのエラーを握り潰しても解決全体は失敗します。不在は有効な Observation で、必須か任意かは解決する拡張が判断します。

同じプロセスが解決と観測を提供する場合、`read_result` 待ちの間に別 ID の `observe` 要求が届きます。観測への応答後に、元の解決の応答を待ちます。最終的な `resolve` の結果には次を返します。

```json
{"domain":"test.sets.v1","identity":[["Observation-ID",1]],"content":{"count":1}}
```

`domain`・`identity`・`content` は必須です。`identity` と `content` は任意の JSON です。Core が、その解決で渡した Observation の ID 配列を `observations` に付けます。拡張からこの配列を返すことはできません。

Core は同じ selector も毎回解決を要求します。拡張はキャッシュを使えますが、Observation と LS・設定などの外部状態を含めて再利用を判断します。各操作中は関連する Resource・設定を変更しない前提です。観測要求の集合が変わっても、同じ入力の意味に対して同じ Region を返してください。

Capture はこの解決結果そのものを固定します。後から解き直すことはありません。Core が現在との変化を検査する際には `content` のみを比べます。何を変化とみなすかを踏まえて `content` を設計してください。`identity` は時点をまたいだ比較に使いません。

## 比較と検査

`compare` の入力は `{"left":Region,"right":Region}` です。戻り値は `equal`・`contains`・`within`・`incomparable` のいずれかの JSON 文字列です。`contains` は左が右を真に含み、`within` はその逆です。包含条件を調べるときは `equal` も含めます。比較不能という関係と処理失敗を区別し、失敗には `error` を返します。異なる domain の比較はエラーです。

比較は [拡張機能の法則](extension_concept.md) を満たします。`monika extension test --cases FILE` は RegionSelector または名前付き指定の JSON 配列を展開・正規化し、決定性、反射性、対称性、反対称性、推移性を検査します。同じ domain を扱う別拡張も含められます。失敗箇所は配列の index で示します。繰り返した解決は比較操作で同一性を調べます。キャッシュは有効なままであり、依存関係やキャッシュ更新の完全性を証明する試験ではありません。

注釈検査の `check` の入力は次の形です。

```text
{link: Link, source: {selector: RegionSelector, region: Region},
 target: {selector: RegionSelector, region: Region}, options: JSON}
```

`link` は展開・正規化済みで、注釈は `link.annotation`、端点の任意の Capture ID は `link.source.capture` / `link.target.capture` にあります。両端の解決に失敗すると `check` は呼ばれず、CLI は終了コード 2 になります。追加の解決計画を要求する操作はありません。

結果は次の判定の配列です。`status` は `pass`・`fail`・`not_applicable` です。取得・解釈の失敗は `error` にします。

```json
[{"code":"count","status":"fail","message":"Expected 2, observed 3"}]
```

Core は Capture と現在の `content` の比較も行い、変化または拡張の `fail` を終了コード 1 にします。端点の selector と Capture の selector の不一致は終了コード 2 です。

## 保存内容と費用

`.monika/captures/` は Region と Observation メタデータを、`.monika/observations/` は保存済み Observation のメタデータと生の bytes を持ちます。`.monika/current.json` は Resource から採用中の Observation ID への対応です。設定・Sidecar とともにコミットして共有します。保存内容には Resource の機密情報が含まれるため、コミット・共有前に確認してください。

Core は操作中の内容をメモリと一時ファイルに保持します。大きな Resource の部分取得は提供していません。解決ごとに内容を JSON へ埋め直さず、同じ Observation の入力ファイルを共有します。
