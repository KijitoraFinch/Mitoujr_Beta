# 設計

Monika は、Agent がセッションをまたいで設計意図・仕様・根拠と、コードやデータの具体的な部分との対応を Link として記録する CLI である。対応を辿って読み、両端を今も解決できるかと確認時点からの変化を検査し、判断の根拠をその時点のまま引用できる。Link と任意の JSON の注釈は Sidecar（`monika.links.json`）を唯一の正本とする。

展開は名前付きの指定を RegionSelector に置き換え、正規化はその `resource` の表記をそろえる。どちらも Core が内容を取得せずに行う。ローカルの Resource はプロジェクトルートからの相対パスに字句的に正規化し、ルート外への指定をエラーにする。観測時にだけ提供元へ `file:` URI を渡す。観測は提供元の拡張が Resource の状態を取得して Observation にし、解決は解釈の拡張が必要な Resource を Core に要求して Region を作る。Region は拡張が定める `content` を持つ。

Resource ごとの現在の決め方は `monika.json` に持つ。`observe` は操作のたびに観測し、`refreshed` は `refresh` で最後に採用した保存済み Observation を使う。保存がなければエラーにする。一回の操作では同じ Resource に一つの Observation を共有し、解決途中で要求された Resource にも自身の方針を適用する。拡張の観測は Resource を変更しない。Observation の内容は Core が管理する一時ファイルで受け渡す。

Capture は展開・正規化した RegionSelector、解決された Region、使った Observation のメタデータ、捕捉時刻を保存する。捕捉時刻を含む内容から ID を作り、`.monika/captures/` に追加する。読み取りでは保存した解決結果をそのまま返す。Link の各端点は Capture の ID を任意に持ち、`capture --link` で見直した端点を捕捉して記録できる。Sidecar の更新は該当端点の Capture ID のみを編集し、利用者の表記を保つ。

`check` は現在の両端を解決し、Capture があれば `region.content` のみで変化を検査する。端点の指定と保存された selector が一致しなければ捕捉し直しを求めるエラーになる。注釈の検査は拡張の `check` に Link と両端の解決結果を渡す。注釈に書いた主張の正しさは、利用者がテストやレビューを通じて確認する。現在の Region 同士の同一性・包含は拡張の比較による。LS や設定など外部状態は拡張が扱い、過去の解決の再現は保証しない。

`links` は展開・正規化のみを行い、Capture の読み取りも観測しない。`read`・`compare`・`check`・`capture` は解決に必要な Resource の現在を使う。`refresh --all` は名前を展開した Sidecar の端点の `resource` から参照される `refreshed` の Resource を更新する。GC は Sidecar から参照されない Capture と、現在として採用されていない保存済み Observation を削除する。独立した引用用 Capture は明示的に保持できる。

`monika.json`、Sidecar、`.monika/` はリポジトリにコミットして共有する。保存済み Observation は内容とメタデータを持ち、Capture とともに内容に基づく ID で検証する。保存内容には Resource の機密情報も含まれる。保存は VCS に依存しない。

CLI は JSON を入出力し、選択や集計は jq と組み合わせる。終了コードは成功 0、Capture からの変化を含む検査不合格 1、実行失敗 2 とする。Tokio による期限・中断・プロセス終了処理で拡張と一時ファイルを管理する。Link の自動生成は今後別途設計する。
