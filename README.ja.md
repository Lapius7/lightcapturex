# LightCaptureX

[English](README.md)

Windows 用の超軽量なゲーム録画ソフトです。約 0.5 MB の exe ひとつで動き、ランタイムのインストールはいりません。

画面の取り込み、色の変換、縮小、エンコードをすべて GPU で行うので、CPU はほとんど使いません。VALORANT のように CPU が重いゲームの録画に向いています。

## 機能

- モニター全体か、ウィンドウひとつを録画（Windows.Graphics.Capture）
- ハードウェアエンコード: H.264 / HEVC / AV1（Media Foundation 経由で NVENC、Quick Sync、AMF）
- GPU の選択: 自動（録画するモニターを担当する GPU）か手動。選んだ GPU でエンコードできないときは、ほかの GPU に切り替える
- 画質（4 段階）、30/60 fps、出力解像度（元のまま / 1080p / 720p / 480p）
- 音声は 1 本のトラックにまとめて録音
  - PC の音: 録音しない / すべての音 / **選んだアプリだけ**（VALORANT と Discord だけ、など）
  - マイク
- **F9** で録画の開始と停止（ほかのアプリを操作中でも使える）
- 設定は自動で保存

## 動作環境

- Windows 10 2004 以降（アプリごとの音声の録音に 2004 以降が必要。Windows 11 を推奨）
- 動画のハードウェアエンコーダを持つ GPU

## 使い方

1. `lightcapturex.exe` を起動します。
2. 録画対象、画質、エンコード（コーデックと GPU）、マイク、PC の音を選びます。
3. **F9**（または録画ボタン）を押すと録画が始まり、もう一度押すと止まります。

録画したファイルは `ビデオ\LightCaptureX\LightCaptureX_YYYYMMDD_HHMMSS.mp4` に保存されます。

### 一部のアプリの音だけを録る

「PC の音」を「選んだアプリだけ」にして、横のボタンからアプリを選びます。VALORANT と Discord はいつも一覧に出ます。ほかのアプリは音を出すと一覧に出ます。録画を始めたあとに起動したアプリの音も、自動で録音に加わります。

### ヒント

- 排他フルスクリーンのゲームをウィンドウで録画して黒い画面になるときは、モニターを録画するか、ゲームをボーダーレスにしてください。
- F9 がほかのアプリに使われているときは、ステータス欄にそう表示されます。

## コマンドライン（動作確認用）

```
lightcapturex --list
lightcapturex --source 0 --seconds 10 --out C:\tmp\a.mp4 [--fps 60] [--quality 0-3] [--height 1080]
              [--codec h264|hevc|av1] [--gpu "<名前>"] [--mic N] [--no-sys] [--apps a.exe,b.exe]
```

## ビルド

Linux や WSL から、MinGW でクロスコンパイルします。

```
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64
cargo build --release
```

`target/x86_64-pc-windows-gnu/release/lightcapturex.exe` ができます。

## ライセンス

MIT © Lapius7
