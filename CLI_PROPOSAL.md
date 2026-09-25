# コマンドの検討案

[設計方針](DESIGNDOC.md)を具体化するための案である。

## 操作の案

| コマンド | 操作 |
| --- | --- |
| `show TARGET` | Region の内容を表示する |
| `links [条件]` | リンク宣言を問い合わせる |
| `check [条件]` | 参照先と宣言間の整合性、拡張が解釈する注釈を検査する |
| `link add / set / remove / copy` | 宣言を編集・複写する |
| `apply PATCH` | 保存した編集案を適用する |
| `extension list / show / test` | 拡張の情報と動作を確認する |

観測、抽出、解決は、それぞれの操作に必要な処理として行う。

## 問い合わせの案

一件の Link 宣言値を、注釈の値、保存された対象指定、領域の同一性や包含関係で選ぶ。
条件を AND・OR・NOT で組み合わせる小さな式を用意し、よく使う条件は短いオプションでも渡す。

例えば、利用者が注釈に `relation` という項目を設けた場合、
「A から supported-by、または B から contradicts」を次のように書く案がある。
領域の指定部分は、それぞれの拡張が解釈する。

```sh
monika links --where '
  (source == region("md:docs/a.md#claim") and annotation.relation == "supported-by")
  or
  (source == region("md:docs/b.md#claim") and annotation.relation == "contradicts")
'
```
この場合、Observationの指定が曖昧なので、現実的にはlatest等の指定を簡潔に可能にする、または自動でLatestにするなどすべきだろう。

## JSON 入出力

スクリプトが入力をデータとして組み立て、結果やエラーに応じて処理を分岐できるようにする。
CLI と JSON は同じ操作を扱う。結果の絞り込みや集計には `jq` なども使える。
