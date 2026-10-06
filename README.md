# freee-printer

印刷すると freee 会計のファイルボックスにアップロードされる仮想プリンター。

IPP プリンター（IPP Everywhere）として振る舞い、受け取った文書を `POST /api/1/receipts` で
アップロードする。印刷する側の OS にドライバーは要らない。

```text
印刷する端末 ──IPP──▶ freee-printer ──HTTPS──▶ freee ファイルボックス
```

| 受け取る形式 | アップロードされる形式 |
| --- | --- |
| PDF | そのまま |
| JPEG / PNG | そのまま |
| PWG Raster / Apple Raster (URF) | 1 ページ 1 画像の PDF に変換（300 dpi） |

ファイル名とメモ欄には印刷ジョブ名（多くの場合は文書のタイトル）が入る。

## 使い方

### 1. freee にアプリを登録する

freee-printer は freee アプリストアでは配布していない。使う人が自分用のアプリを登録し、
その Client ID と Client Secret でログインする。
`~/.config/freee/credentials` に別のツールが書いた `CLIENT_ID`、`CLIENT_SECRET`、
`ACCESS_TOKEN`、`REFRESH_TOKEN`、`COMPANY_ID` があれば、そのまま使うので登録もログインも要らない。

[freee アプリ管理](https://app.secure.freee.co.jp/developers/applications)でアプリを作る。

- アプリタイプ: プライベート
- コールバック URL: `urn:ietf:wg:oauth:2.0:oob`（既定値のまま）
- 権限: 会計の「ファイルボックス」の更新と「事業所」の参照

### 2. 実行する

```sh
cargo run
```

プリンターが起動し、`http://localhost:7933/` に状態と設定のページが開く。
初回はセットアップのページが開くので、画面の案内に沿って進める。

1. freee アプリ管理へのリンクと設定値が表示されるので、登録して Client ID と Client Secret を入力する。
2. 「freee を開いて許可する」で freee を開いて許可し、表示された認可コードを貼り付ける。
3. 事業所が複数あれば選ぶ。

ターミナル側では、Linux と macOS なら CUPS にプリンター「freee」を登録するか聞いてくる
（`lpadmin` に管理者権限が要るときは `sudo` を使う）。Windows では UAC の確認のあと、管理者権限で `Add-Printer` を実行する。

```text
プリンター「freee ファイルボックス」が動いています。状態と設定: http://localhost:7933/
このマシンの印刷ダイアログに「freee」を追加しますか? [Y/n]
追加しました。印刷ダイアログで「freee」を選ぶと、ファイルボックスにアップロードされます。
```

あとはどのアプリからでも「freee」を選んで印刷すればよい。
2 回目以降の `cargo run` は、すぐにプリンターとして動く。

### 3. Web ページ

`http://localhost:7933/` で見られる。

- **状態**: アップロード先の事業所、ジョブの一覧と失敗理由、アップロードできなかった文書の送り直しと削除。
- **セットアップ**: freee へのログイン。
- **設定**: ログイン時の自動起動、印刷ダイアログに出る名前、書類の種類（領収書 / 請求書 / その他 / OCR 任せ）、事業所の切り替え。

セットアップと設定、送り直しは、このマシン（localhost）からの接続にだけ応じる。LAN に
開けたときに他の端末から見えるのは状態だけで、ログインや設定の変更はできない。

### 4. 自動で起動させる

設定ページの「ログイン時に自動で起動する」を入れる。`cargo run` で動かしている最中に入れると、
そのプロセスは終了してサービスが引き継ぐ。コマンドなら `freee-printer service install`。

| OS | 仕組み |
| --- | --- |
| Linux | systemd のユーザーサービス `~/.config/systemd/user/freee-printer.service`。ログは `journalctl --user -u freee-printer -f`。ログインしていない間も動かすなら `loginctl enable-linger` |
| macOS | launchd のエージェント `~/Library/LaunchAgents/io.signal-slot.freee-printer.plist`。ログは `~/Library/Logs/freee-printer.log` |
| Windows | レジストリの Run キー。PowerShell 経由でウィンドウを出さずに起動する |

登録するのは実行したバイナリそのものなので、`cargo install --path cli` で入れた `freee-printer` から行うこと。

## コマンド

| コマンド | 内容 |
| --- | --- |
| `freee-printer` | 未ログインならログインし、そのままプリンターとして動かす |
| `freee-printer login` | freee へのログインをターミナルで行う（Web ページを使わない場合） |
| `freee-printer company [ID]` | アップロード先の事業所を選び直す（ログインし直さずに） |
| `freee-printer serve` | プリンターとして動かす（`--listen`、`--advertise` を指定するとき） |
| `freee-printer install` | この OS の印刷システムにプリンター「freee」を登録する（`serve` を動かした状態で） |
| `freee-printer service install` | ログイン時に自動起動するよう登録する（systemd / launchd / Run キー）。`service uninstall` で外す |
| `freee-printer uninstall` | 登録を外す |
| `freee-printer upload <file>...` | プリンターを介さずにアップロードする |
| `freee-printer status` | 設定とログイン状態を表示する |

`upload` は `serve` を動かしたままでも使える。

## 他の端末から使う

### LAN で共有する

```sh
freee-printer serve --listen 0.0.0.0:7933 --advertise
```

他の PC やスマートフォンのプリンター一覧に現れる。認証はないので、このアドレスに届く人は
誰でもファイルボックスにアップロードできる。信頼できるネットワークでだけ使うこと。

## 設定

状態は `~/.config/freee/credentials` に `KEY=値` の形で入る（場所は `freee-printer status` が表示する）。
シェルスクリプトが `source` できる形式で、freee の API を使う他のツールと共有する前提である。
トークンの更新はファイルロックの下で行い、更新後のトークンはその場で書き戻すので、
同じファイルを読む他のツールは次の実行から新しいトークンを使う。
ただし他のツールが同時にトークンを更新すると、どちらかの更新が失敗する（リフレッシュトークンは一度しか使えない）。

freee の Client Secret とトークンを含むので、所有者だけが読める権限で保存される。
次の項目は設定ページで変えられる（手で書いてもよい）。

| キー | 内容 |
| --- | --- |
| `pr_name` | 印刷ダイアログに出る名前（既定: freee ファイルボックス） |
| `pr_doc_type` | `receipt` / `invoice` / `other`。省略時は freee の OCR 任せ |
| `COMPANY_NAME` | 表示用の事業所名 |

## 失敗したとき

アップロードできなかった文書は捨てずに保存され、状態ページから送り直せる（場所は `freee-printer status` に表示）。
ジョブは「中止」として印刷元に報告され、理由は `serve` のログと状態ページに出る。

- 一時的なエラー（5xx、429、通信断）は 3 回まで再試行する。
- リフレッシュトークンは 90 日で失効する。失効したら `freee-printer login` をやり直す。
- ファイルボックスの上限は 1 ファイル 64 MB。
- 部数、ページ範囲、両面などの印刷オプションは無視する。
- アップロードが始まったジョブは取り消せない。

## 検証状況

確認済み:

- Linux: CUPS に登録したプリンター「freee」からのテストページ印刷と、本物の freee ファイルボックスへのアップロード
  （日本語のジョブ名がそのままファイル名になる）。
- macOS 26（arm64、VM）: `cargo install`、管理者ユーザーでの `sudo` なしの `lpadmin` 登録、`lp` での印刷、
  launchd エージェント（登録、解除、設定ページからの引き継ぎ）。freee はモックサーバー。
- Windows 11（VM）: MSVC Build Tools での `cargo install`、UAC 経由で昇格した `Add-Printer -IppURL`
  （Microsoft IPP Class Driver のキューになり、PDF で送ってくる）、`Out-Printer` での印刷、Run キーのサービス。
  freee はモックサーバー。
- Linux でモックサーバー相手に: `ipptool` の IPP 1.1 / 2.0 テスト、PDF・JPEG・ラスター、Create-Job + Send-Document、
  gzip、トークン更新、アップロードの再試行、失敗した文書の保存と送り直し、`serve` 実行中の `upload`。

未確認:

- iOS と Android からの印刷。
- freee の Web 画面での OCR 結果の見え方。

## 構成

| パス | 役割 |
| --- | --- |
| `crates/core` | プリンター本体。IPP、HTTP/1.1、ラスター変換、freee API、ログイン手順。スレッドとブロッキング I/O だけで書いてあり、通信と保存は trait 越しなので、専用ハードウェアのファームウェアにも載せられる作りにしてある。 |
| `cli` | Linux / macOS / Windows 用のコマンド。rustls の TLS、認証情報ファイル、DNS-SD、OS への登録。 |
