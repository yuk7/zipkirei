# zipkirei

A fast CLI tool to repair problematic ZIP archives without extraction or recompression.

Fix mojibake, decomposed Unicode filenames, and OS junk entries in milliseconds.

[![GitHub Workflow Status](https://img.shields.io/github/actions/workflow/status/yuk7/zipkirei/ci.yml?style=flat-square)](https://github.com/yuk7/zipkirei/actions/workflows/ci.yml)
[![GitHub Release](https://img.shields.io/github/v/release/yuk7/zipkirei?style=flat-square)](https://github.com/yuk7/zipkirei/releases/latest)
[![Crates.io Version](https://img.shields.io/crates/v/zipkirei?style=flat-square)](https://crates.io/crates/zipkirei)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg?style=flat-square)](http://makeapullrequest.com)
![License](https://img.shields.io/github/license/yuk7/zipkirei.svg?style=flat-square)

[日本語](README_ja.md)

### [⬇ Download](https://github.com/yuk7/zipkirei/releases/latest)
[⬇ GitHub Releases](https://github.com/yuk7/zipkirei/releases/latest)
[📦 Crates.io](https://crates.io/crates/zipkirei)

### Supported Platforms
| OS      | Architecture                        |
|---------|-------------------------------------|
| Windows | x86 (i686), x86_64, aarch64         |
| Linux   | x86_64, armv7 (armeabihf), aarch64     |
| macOS   | x86_64, aarch64 (Apple Silicon)     |

## Why?

ZIP archives can contain platform-specific metadata and filename conventions that become troublesome on other operating systems.

This is especially common with archives created on macOS, where differences in Unicode normalization and metadata handling may cause:

- decomposed Unicode filenames
- mojibake text
- duplicate-looking names
- unwanted metadata files

`zipkirei` patches ZIP metadata structures directly, without touching compressed payload data.

If you've ever received a ZIP from a Mac user and seen garbled filenames or mysterious __MACOSX folders, this tool is for you.

## What it fixes

| What users see | Root cause | Fixed by zipkirei |
|---|---|---|
| Unicode filenames render as decomposed characters | macOS stores filenames in NFD form (decomposed) | Windows/Linux tools may show visually duplicated or broken names |
| Mojibake filenames | ZIP UTF-8 flag is missing | Extractors decode names using the wrong encoding |
| Junk files in archive | macOS / Windows metadata files included | Recipients see unnecessary files |

By default, `zipkirei`:

- sets the ZIP UTF-8 flag (bit 11) for non-ASCII filenames
- normalizes non-ASCII UTF-8 filenames from NFD → NFC
- leaves ASCII-only filenames untouched
- removes `.DS_Store`, `__MACOSX/*`, `Thumbs.db`, and `desktop.ini`

## Features

- Pure in-place ZIP repair
- No extraction or recompression
- No temporary files required
- Extremely small disk writes
- ZIP64 support
- Works with password-protected ZIPs because compressed/encrypted payloads are never modified.
- Preserves compressed payloads and CRCs
- UTF-8 NFC normalization
- Skips metadata writes for ASCII-only filenames
- `--dry-run` preview mode
- `--new` compact rewrite mode

## Performance

Tested with a 5GB ZIP archive (2 entries total).

Apple M1 / Internal APFS SSD

### In-place cleaning

| Tool | Execution Time | Disk Write |
|---|---|---|
| **zipkirei** | **23.2ms** | **~100KB** |
| `zip -d` | 7.85s | 5GB |

### Repacking (`--new` mode)

| Tool | Execution Time |
|---|---|
| **zipkirei --new** | **5.09s** |
| `unzip` + `zip -0` | 21.18s |

## Installation

### Download binary
1. Download the latest binary from [Releases](https://github.com/yuk7/zipkirei/releases/latest).
2. Extract the archive and add the executable to your `PATH`.

### Using cargo
If you have Rust installed, you can install it via cargo:
```bash
cargo install zipkirei
```

## Usage

```bash
zipkirei [OPTIONS] <file.zip>
```

### Options

| Option                 | Description                                                  |
| ---------------------- | ------------------------------------------------------------ |
| `--dry-run`            | Show planned changes without modifying the archive           |
| `--fast`               | Fast in-place mode that rewrites only the Central Directory  |
| `--new <outfile>`      | Write a cleaned archive to a new file                        |
| `--not-utf-8`          | Skip UTF-8 filename fixes and only remove excluded entries   |
| `--no-default-exclude` | Keep `.DS_Store`, `__MACOSX`, `Thumbs.db`, and `desktop.ini` |
| `--exclude <name>`     | Also exclude entries matching `<name>`; repeatable           |
| `-h`, `--help`         | Show help                                                    |

## Examples

### Preview changes

```bash
zipkirei --dry-run archive.zip
```

Example output:

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

### Clean in place

```bash
zipkirei archive.zip
```

Windows users can also run it by dragging and dropping the zip file onto the zipkirei executable.

### Fast Central Directory-only cleanup

```bash
zipkirei --fast archive.zip
```

This mode updates only Central Directory metadata. It is faster on large archives, but local file headers are left unchanged, so compatibility with some ZIP tools may be reduced.

In `--fast --dry-run`, excluded entry sizes are reported as `? B` because local entry spans are not scanned.

### Write a new cleaned archive

```bash
zipkirei --new archive_clean.zip archive.zip
```

### Only remove junk entries

```bash
zipkirei --not-utf-8 archive.zip
```

### Add a custom exclusion

```bash
zipkirei --exclude .gitkeep archive.zip
```

## Limitations

* Multi-disk ZIP archives are not supported
* Filename normalization requires valid UTF-8 names
* `--exclude` matches basenames only
* In-place mode modifies the archive directly

If filenames are not valid UTF-8, rerun with `--not-utf-8`.

## Safety

`zipkirei` never decompresses or recompresses file payloads.

* compressed data remains untouched
* CRCs are preserved
* only ZIP metadata structures are rewritten

In-place mode directly modifies the archive, so `--dry-run` is recommended first.

Use `--new` if you want to keep the original file unchanged.

## How it works

`zipkirei` was designed around pure in-place metadata patching from the beginning. It operates in two phases:

1. Parse the Central Directory and build a patch plan
2. Rewrite only the necessary ZIP metadata structures

Compressed payload data is never recompressed.

### NFC normalization

macOS may store filenames in Unicode NFD form (decomposed characters).

For example:

```text
か + ゙  →  が
ᄒ + ᅡ + ᆫ → 한
```

`zipkirei` normalizes filenames to NFC to avoid decomposed names rendering incorrectly across platforms.

ASCII-only filenames are already byte-for-byte compatible with UTF-8 and common legacy ZIP filename decoding. `zipkirei` leaves them unchanged instead of setting bit 11, which avoids unnecessary metadata writes in in-place mode.

### In-place patching

UTF-8 NFC normalization of non-ASCII characters generally reduces the byte count, causing data to shift forward.
This maintains the invariant `writer offset < reader offset`, enabling safe in-place repair without temporary files.

Furthermore, by filling the freed bytes with ZIP extra fields (padding) whenever possible, `zipkirei` avoids shifting subsequent data blocks, which minimizes disk writes.

- reuses freed bytes as ZIP extra fields (padding)
- shifts structures forward only
- incrementally updates offsets
- preserves compressed payload data verbatim

This allows the archive to be repaired with extremely small disk writes and no temporary files.

```text
Before (NFD):
[LFH][filename........][extra][data]

After NFC (shorter):
[LFH][filename....][free][data]

zipkirei (with padding):
[LFH][filename][padding extra][data]
```

### Excluded entries

In `--new` mode, excluded entries are completely removed.

In in-place mode, excluded local entries may remain as unreachable orphan data after being detached from the Central Directory. Standard ZIP readers ignore them.

## Development

Build from source:

```bash
cargo build --release
```

Run locally:

```bash
cargo run -- --help
```

Tests:

```bash
cargo test --locked
```

## License

[MIT](LICENSE)
