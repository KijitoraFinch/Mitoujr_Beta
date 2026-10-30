# Monika のロゴ案

[色を調整できる比較ページ](preview.html) をブラウザで開く。
利用者が気に入った `[=]` の原案と五つの派生案を比較する。
書体はいったん **IBM Plex Mono Medium** を基準にする。意匠の採用案は未決定。

基調色とアクセント色の OKLCH を、スライダーと数値入力で調整できる。
色は全案と小サイズ表示に同時に反映される。指定値は URL に残り、再読み込みでも復元する。
配色（アクセントあり／単色）、背景、書体も比較できる。
各案のボタンから、現在の色・配色・書体の透明背景 SVG／PNG を保存できる。
SVG の metadata には指定した OKLCH、表示用の sRGB、書体を含める。

[六案の台紙](brackets.png) は初期配色での比較。
初期配色の素材は `exports/brackets/` に収録する。

| 案 | 意匠 | 透明背景のロゴ |
| --- | --- | --- |
| C1 | 原案 | [SVG](exports/brackets/c1-accent.svg) / [PNG](exports/brackets/c1-accent.png) |
| C2 | 軽やか。線を細くし、括弧の端を短く。 | [SVG](exports/brackets/c2-accent.svg) / [PNG](exports/brackets/c2-accent.png) |
| C3 | 面取り。括弧の角と二本線の端を斜めに。 | [SVG](exports/brackets/c3-accent.svg) / [PNG](exports/brackets/c3-accent.png) |
| C4 | やわらか。角と線の端を丸く。 | [SVG](exports/brackets/c4-accent.svg) / [PNG](exports/brackets/c4-accent.png) |
| C5 | 横にひらく。括弧を広げ、二本線を長く。 | [SVG](exports/brackets/c5-accent.svg) / [PNG](exports/brackets/c5-accent.png) |
| C6 | 太い印。括弧を厚くして輪郭を強く。 | [SVG](exports/brackets/c6-accent.svg) / [PNG](exports/brackets/c6-accent.png) |

## 初回の三案

[初回の比較ページ](round1.html)、[比較台紙](comparison.png)、[書体比較](typography.png)。
初回の三案は IBM Plex Sans Medium の組み方を保存している。

| 案 | 意匠 | 透明背景のロゴ |
| --- | --- | --- |
| A | かがり目。二つの縁を渡る糸を M の形にする。 | [SVG](exports/a-stitch-accent.svg) / [PNG](exports/a-stitch-accent.png) |
| B | 組み合う帯。二本の帯が交互に重なり合う。 | [SVG](exports/b-weave-accent.svg) / [PNG](exports/b-weave-accent.png) |
| C | 照合する印。括弧と二本の線で対応を示す。 | [SVG](exports/c-compare-accent.svg) / [PNG](exports/c-compare-accent.png) |

## 素材と色

フォントの輪郭を SVG に収録している。
`exports/` の `-mono` は黒一色、`-mark-` はマーク単独。
SVG と PNG はすべて透明背景で、マークの交差部分も透過する。
PNG は SVG の基準寸法の 4 倍で書き出す。
比較台紙だけは背景色を持ち、日本語の説明に閲覧環境のフォントを使う。

基調色は `#303234`。アクセントは指定の `oklch(85.4% 0.2441 147.4)` を
sRGB の 8 bit 値に丸めた `#22F765`。指定色は sRGB の色域内に収まる。
数値は [colors.json](colors.json) に記録する。

調整ページも sRGB で表示・保存する。色域外の指定は L と h を保ち、
C を sRGB の境界まで下げる。調整が入る場合は、表示用の C を操作欄に示す。
変換の行列は [Oklab の原著者の実装](https://bottosson.github.io/posts/oklab/#converting-from-linear-srgb-to-oklab)に基づく。

## 編集と再生成

マークの編集元は `source/*.svg`、組み方と台紙は `build.py`。
`preview-data.js` も `build.py` から生成する。比較ページは `preview.html`、
`preview.css`、`preview.js`、色変換は `color.js`。
IBM Plex の配布元と版は [fonts/SOURCE.md](fonts/SOURCE.md)、ライセンスは
[fonts/OFL.txt](fonts/OFL.txt) を参照。

Python 3.9 以降と `rsvg-convert` を使う。作業用環境に `requirements.txt` の依存を入れ、
プロジェクト直下から `python assets/logo/build.py` を実行する。
再生成はネットワーク接続やフォントのシステムへのインストールを必要としない。

HarfBuzz の字間処理を適用した実フォントの輪郭を使用する。
書体比較は Sans Medium・Sans SemiBold・Mono Medium の文字の見かけの高さを揃える。

## 確認

原案と五つの派生案、Mono Medium の初期表示、色のスライダー・数値入力・URL からの復元を確認した。
125 色をブラウザのネイティブな OKLCH 描画と照合し、8 bit で各チャンネルの差が 1 以下になることを確認した。
色域外の指定、無彩色、明度の両端、単色と書体の切り替えも確認した。

変更した配色で SVG／PNG を保存し、輪郭化した書体・色の値・背景の透過が保たれることを確認した。
収録した 36 組の SVG／PNG も、背景の透過、輪郭化した文字、端の欠けがないことを確認した。
比較ページは幅 1560・980・768・390・320 px で横にはみ出さず表示できる。
