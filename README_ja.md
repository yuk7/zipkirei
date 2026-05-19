# zipkirei

展開や再圧縮をせずに、問題のあるZIPアーカイブを修復する高速なCLIツールです。

文字化け、分解されたUnicodeファイル名、OSのジャンクファイルを高速に修正します。

[![GitHub Workflow Status](https://img.shields.io/github/actions/workflow/status/yuk7/zipkirei/ci.yml?style=flat-square)](https://github.com/yuk7/zipkirei/actions/workflows/ci.yml)
[![GitHub Release](https://img.shields.io/github/v/release/yuk7/zipkirei?style=flat-square)](https://github.com/yuk7/zipkirei/releases/latest)
[![Crates.io Version](https://img.shields.io/crates/v/zipkirei?style=flat-square)](https://crates.io/crates/zipkirei)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg?style=flat-square)](http://makeapullrequest.com)
![License](https://img.shields.io/github/license/yuk7/zipkirei.svg?style=flat-square)

[English](README.md)

### [⬇ ダウンロード](https://github.com/yuk7/zipkirei/releases/latest)
[⬇ GitHub Releases](https://github.com/yuk7/zipkirei/releases/latest)
[📦 Crates.io](https://crates.io/crates/zipkirei)

### 対応環境
| OS      | Architecture                        |
|---------|-------------------------------------|
| Windows | x86 (i686), x86_64, aarch64         |
| Linux   | x86_64, armv7 (armeabihf), aarch64     |
| macOS   | x86_64, aarch64 (Apple Silicon)     |

## 背景

ZIPアーカイブには、他のOSでトラブルの原因となるプラットフォーム固有のメタデータやファイル名の形式が含まれていることがあります。

特にmacOSで作成されたアーカイブで、Unicodeの正規化やメタデータの扱いの違いにより、以下のような問題が発生することがあります。

- 分解されたUnicodeファイル名
- 文字化け
- 重複して見える名前
- 不要なメタデータファイル

`zipkirei` は圧縮されたペイロードデータに触れることなく、ZIPメタデータ構造を直接パッチします。

もし、Macユーザーから送られてきたZIPを展開して、ファイル名の文字化けや、謎の `__MACOSX` フォルダに悩まされたことがあるなら、このツールが役立ちます。

## 修正内容

| 現象 | 原因 | zipkireiで修正出来ること |
|---|---|---|
| Unicodeファイル名が分解されて表示される | macOSがファイル名をNFD形式（分解された文字）で保存している | Windows/Linuxのツールでは視覚的に重複したり壊れて表示されることがある |
| ファイル名の文字化け | ZIPのUTF-8フラグが欠落している | 展開ツールが誤ったエンコーディングで名前をデコードする |
| アーカイブ内のジャンクファイル | macOSやWindowsのメタデータファイルが含まれている | 不要なファイルが見えてしまう |

デフォルトで、`zipkirei` は以下の処理を行います。

- 非ASCIIファイル名に ZIP UTF-8フラグ (bit 11) を設定
- 非ASCIIのUTF-8ファイル名を NFD → NFC に正規化
- ASCIIのみのファイル名は変更しない
- `.DS_Store`、`__MACOSX/*`、`Thumbs.db`、`desktop.ini` を削除

## 特徴

- 純粋なインプレースZIP修復
- 展開や再圧縮は不要
- 一時ファイル不要
- 最小限のディスク書き込み
- ZIP64対応
- パスワード付きZIPも、圧縮・暗号化されたペイロードを変更しないためそのまま動作します。
- 圧縮されたペイロードとCRCを保持
- UTF-8 NFC正規化
- ASCIIのみのファイル名ではメタデータ書き込みをスキップ
- `--dry-run` プレビューモード
- `--new` コンパクトな新規ファイル作成モード

## パフォーマンス

5GBのZIPアーカイブ（合計2エントリ）でテスト。

Apple M1 / 内蔵 APFS SSD

### インプレースでクリーンアップ

| ツール | 実行時間 | ディスク書き込み |
|---|---|---|
| **zipkirei** | **23.2ms** | **~100KB** |
| `zip -d` | 7.85s | 5GB |

### 新規ファイル作成 (`--new` モード)

| ツール | 実行時間 |
|---|---|
| **zipkirei --new** | **5.09s** |
| `unzip` + `zip -0` | 21.18s |

## インストール

### バイナリをダウンロード
1. [リリースページ](https://github.com/yuk7/zipkirei/releases/latest)から最新のバイナリをダウンロードします。
2. アーカイブを展開し、実行ファイルに `PATH` を通してください。

### cargo を使用
Rustがインストールされている環境であれば、cargoでインストールできます。
```bash
cargo install zipkirei
```

## 使い方

```bash
zipkirei [OPTIONS] <file.zip>
```

### オプション

| オプション             | 説明                                                         |
| ---------------------- | ------------------------------------------------------------ |
| `--dry-run`            | アーカイブを変更せずに計画された変更を表示する               |
| `--new <outfile>`      | クリーンアップされたアーカイブを新しいファイルに書き出す     |
| `--not-utf-8`          | UTF-8ファイル名の修正をスキップし、除外エントリの削除のみを行う |
| `--no-default-exclude` | `.DS_Store`、`__MACOSX`、`Thumbs.db`、`desktop.ini` を保持する |
| `--exclude <name>`     | `<name>` に一致するエントリも除外する（複数指定可）          |
| `-h`, `--help`         | ヘルプを表示する                                             |

## 使用例

### 変更のプレビュー

```bash
zipkirei --dry-run archive.zip
```

出力例:

```text
[exclude]  .DS_Store  (8192 B)
[exclude]  __MACOSX/._README  (2048 B)
[nfc]      にほんご.txt  →  にほんご.txt
[bit11]    にほんご.txt
[nfc]      한국어.txt  →  한국어.txt
[bit11]    한국어.txt
[bit11]    中文.txt

Summary:
  Excluded:     2 entries
  NFC renamed:  2 entries
  bit11 set:    3 entries
```

### インプレースでクリーンアップ

```bash
zipkirei archive.zip
```

Windowsユーザーは、zipファイルをzipkireiの実行ファイルにドラッグ&ドロップすることでも実行できます。

### クリーンアップされた新しいアーカイブを書き出す

```bash
zipkirei --new archive_clean.zip archive.zip
```

### ジャンクエントリのみを削除

```bash
zipkirei --not-utf-8 archive.zip
```

### カスタム除外を追加

```bash
zipkirei --exclude .gitkeep archive.zip
```

## 制限事項

* 分割（マルチディスク）ZIPアーカイブには対応していません
* ファイル名の正規化には有効なUTF-8名が必要です
* `--exclude` はベースネームにのみ一致します
* インプレースモードはアーカイブを直接変更します

ファイル名が有効なUTF-8でない場合は、`--not-utf-8` を付けて再実行してください。

## 安全性

`zipkirei` はファイルペイロードの展開や再圧縮を一切行いません。

* 圧縮データはそのまま保持されます
* CRCは保持されます
* ZIPのメタデータ構造のみが書き換えられます

インプレースモードはアーカイブを直接変更するため、最初に `--dry-run` を実行することをお勧めします。

元のファイルを変更せずに保持したい場合は `--new` を使用してください。

## 仕組み

`zipkirei` は最初から純粋なインプレースメタデータパッチングを中心に設計されました。2つのフェーズで動作します。

1. セントラルディレクトリを解析し、パッチ計画を作成する
2. 必要なZIPメタデータ構造のみを書き換える

圧縮されたペイロードデータが再圧縮されることはありません。

### NFC正規化

macOSではファイル名がUnicode NFD形式（分解された文字）で保存されることがあります。

例:

```text
か + ゙  →  が
ᄒ + ᅡ + ᆫ → 한
```

`zipkirei` は、プラットフォーム間でのファイル名が分解されて正しく表示されないのを避けるため、ファイル名をNFCに正規化します。

ASCIIのみのファイル名は、UTF-8や一般的な従来のZIPファイル名デコードとバイト列が一致します。そのため `zipkirei` はASCII名には bit 11 を設定せず、インプレースモードで不要なメタデータ書き込みを避けます。

### インプレースパッチ

非ASCIIのUTF-8 NFC正規化により基本的にバイト数が減るため、データは前にずれます。

```text
writer offset < reader offset
```

これによりZIPメタデータ構造内に空きスペースが生まれます。アーカイブを一時ファイルに再書き出しする代わりに、`zipkirei` は以下を行います。

- 構造を前方にのみシフト
- 空いたバイトをZIPのextra field（パディング）として再利用
- オフセットを逐次更新
- 圧縮されたペイロードデータをそのまま保持

これにより、一時ファイルを作成することなく、非常に小さなディスク書き込みでアーカイブを修正できます。

```text
Before (NFD):
[LFH][filename........][extra][data]

After NFC (shorter):
[LFH][filename....][free][data]

zipkirei (with padding):
[LFH][filename][padding extra][data]
```

### エントリ除外

`--new` モードでは、除外されたエントリは完全に削除されます。

インプレースモードでは、除外されたローカルエントリはセントラルディレクトリから切り離された後、到達不能な「孤立データ（orphan data）」として残る場合があります。標準的なZIPリーダーはこれらを無視します。

## 開発

ソースからビルド:

```bash
cargo build --release
```

ローカルで実行:

```bash
cargo run -- --help
```

テスト:

```bash
cargo test --locked
```

## ライセンス

[MIT](LICENSE)
