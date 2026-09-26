# 問い合わせの検討案

Sidecar の Link を問い合わせる方針は [設計](DESIGNDOC.md) に、
現在のコマンドと条件式は [README](README.md) に記載する。
以下は、次に検討するクエリ言語と Observation の指定方法の案である。

クエリの構文と評価には既存の軽量な言語を使い、Monika が Link のデータと
Region の解決・比較を提供する。言語の採用は、必要になった Region を非同期に解決する
接続方法、判定不能の扱い、注釈の構造化データを扱う機能を検証して決める。

条件は Sidecar に保存された一件の Link に適用する。
保存された ID・注釈の値と RegionSelector の表記は、その値で評価できる。
Region の意味を使う条件は、指定した Observation に基づく解決と比較を行う。

現行の対象指定では、操作の session または `--snapshot` が Observation を決める。
Observation の指定を条件式へどう表すかは未決定である。
latest 等の指定を簡潔に可能にする、または省略時に現在の状態を観測する、といった案を検討する。
