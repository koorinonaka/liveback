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

release プロファイルは `lto = "fat"` と `codegen-units = 1` です。exe は約 1 割小さくなりますが、
LTO なしの約 2.7 倍の時間がかかります。`lto = "thin"` は LTO なしより exe が大きくなったので
使っていません。

## 検証

```powershell
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test
cargo build --features control
```

`control` と `insight` は既定でオフの feature です。`control` は検証の台本が録画や書き出しを
外から動かすための `--liveback-control` を足します。`insight` は下の計測です。どちらも
配布する exe には入りません。

CI は push では走りません。`cargo fmt --check` と `cargo clippy` を回す `checks.yml` は、
Actions の画面から手で起動します。テストは CI では回らないので、上のコマンドをローカルで
回してください。

実デスクトップに本物のウィンドウを作ってリサイズ・最小化するテスト
（`capture_gpu_automation_parent`）は、走っている間は機械が使えません。ほかに、録画済みの
セッションを環境変数で渡して測るテストもあります。どれも `#[ignore]` 済みなので、素の
`cargo test` は安全です。明示的に走らせるときだけ `-- --ignored` を付けてください。

ハードウェアエンコーダには同時セッション数の上限があるので、既定の並列度では録画系のテストが
1本落ちることがあります。`cargo test -- --test-threads=4` で安定します。

## ビルドがランダムに落ちるとき

`cargo build` が `STATUS_ACCESS_VIOLATION`（`exit code: 0xc0000005`）で落ちたら、多くは
**rustc 自身のスタックオーバーフロー**であってソースの誤りではありません。`.slint` に要素や
バインディングを足して生成コードが深くなると出ます。エラーが巨大なリンカ引数のダンプなので、
読まずに直前の編集を疑うと、時間を無駄にします。

```powershell
$env:RUST_MIN_STACK = "134217728"
```

を立てて同じコマンドをやり直してください。

落ちた後のビルドが `LNK1103`（デバッグ情報が壊れています）で止まるときは、壊れた中間物が
残っています。エラーの行が `liveback-*` の incremental を指すなら
`target\x86_64-pc-windows-msvc\debug\incremental\liveback-*` を消してください。
`liblivia-*.rlib` を指すなら、`cargo clean -p livia` で作り直してください。

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
`DisplayName` と `IconUri` を書きます。そのため一度インストールした機械なら `cargo run` の生
exe でも通知は出ます。まっさらな環境で通知を確かめたいときはインストーラーを通してください。

通知のアイコンは `IconUri` が指す PNG です（インストール先の `liveback-toast-ribbon-256.png`）。
exe を指してもアイコンは出ません。通知の仕組みは exe からアイコンを取り出さないためです。

## 保存先とログ

- 録画バッファ: `%LOCALAPPDATA%\Liveback\buffer\capture-*.lvb`（設定で変更できます）
- クリップ: 既定は録画バッファの下の `Clips`（設定で変更できます）
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

出力は `%LOCALAPPDATA%\com.liveback.desktop\logs\insight.<epoch秒>.log` で、5種類の行が出ます:

```
t=5.001 scope=playback_decode_video n=300 total_ms=1761.00 max_ms=12.40 mean_us=5870.0
t=12.345 slow scope=thumbnail_write ms=41.20
t=1.000 heartbeat cpu_pct=12.3 working_set_mb=180.4 private_mb=150.2
t=1.000 buffers rust_heap_mb=96.2 ui_images=history:12,review:40
t=1.000 threads total_pct=11.8 top=livia-wgc:4.1,playback-engine:2.0 other=1.2
```

- `scope=` は5秒ごとの集計で、重い順。1回ずつ出さないのは、60fps で全スコープを素に出すと
  毎時 200〜400MB になるためです
- `slow` は1回が 8ms を超えた瞬間。スコープごと毎秒1行までに制限してあります。平均に埋もれる
  単発の停止はこちらで見つけます
- `heartbeat` はこのプロセス自身の CPU とメモリ
- `buffers` はメモリのうち Rust 側のヒープの量と、UI が持っているデコード済みの画像の枚数
- `threads` は CPU をスレッドごとに分けたもの。Slint の描画や Media Foundation のスレッドの
  ように、スコープを置けない所の負荷はここで見ます

計測コードは `insight` feature ごと `#[cfg]` で落ちます。インストーラー用の
`cargo build --release` は feature を付けないので、配布される exe には1バイトも入りません。
この範囲を触ったときは `cargo test --features insight` も走らせてください。

## MCP サーバー

**設定 → 一般 → MCP サーバー** をオンにすると、起動中の Liveback が `http://127.0.0.1:47824/mcp` で
MCP（streamable HTTP、JSON 応答）を受け付けます。既定はオフです。

- 待ち受けるのは `127.0.0.1`（ループバック）だけです。他の機械からは繋がりません。ブラウザ由来の
  ループバック以外の `Origin` は 403 で拒みます
- ポートは設定ファイル（`%APPDATA%\com.liveback.desktop\settings.json` の `mcpPort`）で変えられます。
  ポートが使われていて開始できなかったときは、設定の行にその旨が出ます
- ツールは次の 13 本です。消す口はありません。マーカーとコメントの追加、保護、クリップの保存、録画の開始と停止はできます。
  今あるマーカーを変える口と、保護を外す口はありません（保護を外すのはアプリで行います）。時刻は秒（小数可）で、
  原点は確認画面のタイムラインと同じ「残っている最初のセグメントの始まり」です
- 録画中のセッション（`closed` が `false`）は、録画を止めるまで `export_range` で書き出せません

| ツール | 引数 | 返すもの |
| --- | --- | --- |
| `list_sessions` | なし | セッション一覧（新しい順）: `id` / `title` / `game` / `started_at_ms` / `duration_s` / `closed` / `marker_count` / `note` |
| `list_markers` | `session` | そのセッションのマーカー: `time_s` / `time_100ns` / `label` / `comment` |
| `export_range` | `session` / `start_s` / `end_s` / `output_dir`（絶対パス） | 書き出した mp4 の `parts`: `path` / `start_s` / `end_s`。書き終わるまで待ちます。既存のファイルは上書きしません。隙間や解像度の変化をまたぐと複数ファイルになります。他の書き出しの最中は拒みます |
| `get_session` | `session` | 1 本のセッションの詳細: `list_sessions` の項目に加えて `protected` / `comment` / `total_bytes` / `segment_count` / `gaps` / `audio_tracks` / `spans`（隙間で区切った区間ごとの `width` / `height` / `size_changes`） |
| `get_frame` | `session` / `time_s` / `max_width`（省略時 1280） | その時刻以後の最初のフレームの PNG 画像と、そのフレームの `time_s` / `width` / `height`。縮めるだけで拡大はしません。隙間の時刻は拒みます |
| `list_clips` | なし | クリップのフォルダの mp4（新しい順）: `path` / `name` / `created_ms` / `bytes` / `duration_s` / `comment` / `app` |
| `add_marker` | `session` / `time_s` / `label`（省略可） / `comment`（省略可） | 足したマーカー（`list_markers` と同じ形）。録画の外の時刻と、既にマーカーがある時刻は拒みます |
| `set_session_comment` | `session` / `comment` | 書いた `comment`。今のコメントを置き換えます |
| `protect_session` | `session` | `protected: true`。保護したセッションは自動では消えません |
| `save_clip` | `session` / `start_s` / `end_s` / `comment`（省略可） | クリップのフォルダに保存した mp4 の `parts` と `comment_written`。アプリのクリップ一覧に出ます。書き終わるまで待ちます。他の書き出しの最中と録画中は拒みます |
| `get_status` | なし | 録画中のセッション（`session` / `hwnd` / `title` / `executable` / `elapsed_s`）、`stopping`、録画できる窓の `targets`（`hwnd` / `title` / `executable`） |
| `start_recording` | `window`（`0x…` の hwnd か、題名か実行ファイル名の一部） | 始めた録画の `session` と窓の `hwnd` / `title` / `executable`。録画の設定はアプリのものを使います。当たる窓が無いときと 2 つ以上のときは候補を返して拒みます。録画中の窓も拒みます。アプリの画面は切り替わりません |
| `stop_recording` | `session` | 止めた `session` と `closed: true`。閉じ終わるまで最大 10 秒待ち、間に合わなければ `closed: false` と `stopping: true` を返します |

Claude Code から使う例:

```powershell
claude mcp add --transport http liveback http://127.0.0.1:47824/mcp
```

## 対象外

HEVC、FFmpeg、macOS / Linux / ARM、コード署名、テレメトリは対象にしていません。
排他フルスクリーン・モニター全体・仮想ディスプレイの録画も保証外です。

## ライセンス

[MIT License](../LICENSE)

| 主な依存 | ライセンス |
| --- | --- |
| [Slint](https://slint.dev/)（UI、`i-slint-*` 含む） | Slint Royalty-free 2.0 <a href="https://slint.dev/"><img src="https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png" height="24" alt="Made with Slint" align="absmiddle"></a> |
| [wgpu / wgpu-hal](https://wgpu.rs/)（`vendor/wgpu-hal` にパッチ版を同梱） | MIT OR Apache-2.0 |
| [skia-safe](https://github.com/rust-skia/rust-skia)（描画） | MIT |
| [windows / windows-core](https://github.com/microsoft/windows-rs)（Win32 バインディング） | MIT OR Apache-2.0 |
| [signalsmith-stretch](https://github.com/Signalsmith-Audio/signalsmith-stretch)（音声の時間伸縮） | MIT |

そのほかの依存はすべて MIT / Apache-2.0 / Zlib です。
