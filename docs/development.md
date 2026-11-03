# ソースからビルドする

エンドユーザー向けの説明は [README.md](../README.md) にあります。ここはビルドと検証の手順です。

Rust の単一バイナリで、UI は [Slint](https://slint.dev/)、録画は Windows Graphics Capture /
D3D11 / WASAPI / Media Foundation を直接叩いています。外部ランタイムへの依存はありません。

## 必要なもの

- **Windows 11 22H2 以降、x64**。ビルドも実行もこの構成だけを対象にしています
- **Rust ツールチェーン** — バージョンは [`rust-toolchain.toml`](../rust-toolchain.toml) に
  固定してあります（`rustup` が自動で入れます）。`stable` 追従にしていないのは、stable が
  動いた日にコード変更ゼロで clippy が赤くなったことがあるためです
- **Visual Studio C++ Build Tools と Windows SDK** — MSVC リンカに必要
- **LLVM（libclang）** — 音声の時間伸縮に使っている `signalsmith-stretch` が bindgen 経由で
  C++ ライブラリを束ねます。bindgen はディレクトリを指してやらないと LLVM を見つけないので、
  自動で通らない場合は環境変数を立ててください:

  ```powershell
  $env:LIBCLANG_PATH = "$env:ProgramFiles\LLVM\bin"
  ```

- **NSIS**（インストーラーを作る場合のみ）— `winget install NSIS.NSIS`
- **ハードウェア H.264 エンコーダを持つ GPU** — 実行と、録画まわりのテストに要ります

## ビルドと実行

```powershell
cargo run
```

UI は `ui/*.slint` です。`build.rs` が `ui/app.slint` をコンパイルするので、`.slint` を触った
あとも `cargo build` を回すだけで反映されます。

リリースビルドは `target\x86_64-pc-windows-msvc\release\liveback.exe` に出ます。

```powershell
cargo build --release
```

## 検証

```powershell
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test
```

CI が回すのは `cargo fmt --check` と `cargo clippy` の2つだけです。つまり **CI の赤は
ローカルで先に見えますが、テストが緑でも CI が緑とは限りません**（逆もまた然り）。

実デスクトップに本物のウィンドウを作ってリサイズ・最小化するテストが4本あり、走っている間は
機械が使えません。これらは `#[ignore]` 済みなので素の `cargo test` は安全です。明示的に
走らせるときだけ `-- --ignored` を付けてください。

ハードウェアエンコーダには同時セッション数の上限があるので、既定の並列度では録画系のテストが
1本落ちることがあります。`cargo test -- --test-threads=4` で安定します。

## ビルドがランダムに落ちるとき

`cargo build` が `STATUS_ACCESS_VIOLATION`（`exit code: 0xc0000005`）で落ちたら、多くは
**rustc 自身のスタックオーバーフロー**であってソースの誤りではありません。`.slint` に要素や
バインディングを足して生成コードが深くなると出ます。エラーが巨大なリンカ引数のダンプなので、
読まずに直前の編集を疑うと時間を溶かします。

```powershell
$env:RUST_MIN_STACK = "134217728"
```

を立てて同じコマンドをやり直してください。

## インストーラーを作る

```powershell
cargo build --release
makensis installer\liveback.nsi
```

`installer\out\Liveback_<version>_x64-setup.exe` ができます。バージョンは
`Cargo.toml` の `version` が正で、インストーラーの既定値もそこに合わせてあります。別の番号で
作りたいときは `makensis /DVERSION=1.2.3 installer\liveback.nsi` のように渡してください。

`%LOCALAPPDATA%\Liveback` へのユーザー単位インストールで、UAC は出ません。インストール中に
動作中の `liveback.exe` があれば警告して終了を促すので、録画中は先に停止してください。

## OS 通知は AUMID の登録が要る

Windows のトースト通知は、送信元の AUMID（`com.liveback.desktop`）が登録済みのときだけ
表示されます。未登録だと例外もエラーも出さずに捨てられる（`Toast::show` は `Ok` を返す）ので、
ログを見ても原因は分かりません。

登録するのはインストーラーで、`HKCU\Software\Classes\AppUserModelId\com.liveback.desktop` に
`DisplayName` を書きます。そのため一度インストールした機械なら `cargo run` の生 exe でも通知は
出ます。まっさらな環境で通知を確かめたいときはインストーラーを通してください。

## 保存先とログ

- 録画バッファ: `%LOCALAPPDATA%\Liveback\buffer\capture-*.lvb`（設定で変更できます）
- **1 セッション = 1 ファイル**。セグメント・サムネイル・インデックス・タイトル / メモ /
  保護 / マーカーの編集履歴を、すべてその `.lvb` に追記していきます。セッションごとの
  フォルダも `manifest.json` もサイドカーも作りません
- 保持時間を超えた、プレビューや書き出しで使用中でないセグメントは prune されます。削除では
  なく **sparse hole punch** で領域を返すので、ファイルの長さは縮みませんが実割り当ては減ります
- `liveback.exe --liveback-inspect <path.lvb>` でヘッダ・チェックポイント・セグメント一覧を
  ダンプできます
- 設定: `%APPDATA%\com.liveback.desktop\settings.json`
- ログ: `%LOCALAPPDATA%\com.liveback.desktop\logs\liveback.log.*`（日付ごと、7日で自動削除）。
  個人情報・音声内容・任意のファイルパスを書き出さない設計です

強制終了しても、コンテナの末尾に書きかけのレコードが残るだけです。復旧は「最後の健全な
レコードまで切り詰めて閉じる」処理になります。各レコードは CRC を持ち、チェックポイントより
前のものも読み出し時に検証されるので、壊れたバイトが再生に混ざることはありません。

## 処理時間を測る（`--insight`）

「どこが重いか」を数字で見るための計測です。

```powershell
cargo run --features insight -- --insight
```

出力は `%LOCALAPPDATA%\com.liveback.desktop\logs\insight.<epoch秒>.log` で、3種類の行が出ます:

```
t=5.001 scope=playback_decode_video n=300 total_ms=1761.00 max_ms=12.40 mean_us=5870.0
t=12.345 slow scope=thumbnail_write ms=41.20
t=1.000 heartbeat cpu_pct=12.3 working_set_mb=180.4 private_mb=150.2
```

- `scope=` は5秒ごとの集計で、重い順。1回ずつ出さないのは、60fps で全スコープを素に出すと
  毎時 200〜400MB になるためです
- `slow` は1回が 8ms を超えた瞬間。スコープごと毎秒1行までに制限してあります。平均に埋もれる
  単発の停止はこちらで見つけます
- `heartbeat` はこのプロセス自身の CPU とメモリ

計測コードは `insight` feature ごと `#[cfg]` で落ちます。インストーラー用の
`cargo build --release` は feature を付けないので、配布される exe には1バイトも入りません。
この範囲を触ったときは `cargo test --features insight` も走らせてください。

## 対象外

HEVC、FFmpeg、macOS / Linux / ARM、コード署名、テレメトリは対象にしていません。
排他フルスクリーン・モニター全体・仮想ディスプレイの録画も保証外です。
