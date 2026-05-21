use super::bytes::{
    read_u16, read_u32, read_u64, write_u16, write_u32, write_u32_slice, write_u64_slice,
};
use super::cd_entry::build_cd_entry;
use super::copy::{copy_within_file, stream_copy};
use super::eocd::{find_archive_info, parse_eocd, write_eocd, write_zip64_eocd, ArchiveInfo};
use super::local_header::LocalHeader;
use super::options::Options;
use super::plan::{build_plans, normalize_for_test, resolve_lfh_offset, EntryPlan};
use super::{
    dry_run_report, process_file, process_new, BIT11, CENTRAL_DIR_SIG, EOCD_SIG,
    LOCAL_FILE_HEADER_SIG, ZIP64_EXTRA_FIELD_ID,
};
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_temp_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("zipkirei-{nanos}-{name}"))
}

fn assert_unzip_test_accepts(path: &Path) {
    let output = Command::new("unzip")
        .arg("-t")
        .arg(path)
        .output()
        .expect("failed to launch unzip");
    if output.status.success() {
        return;
    }

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));

    let only_name_warning = output.status.code() == Some(1)
        && text.contains("mismatching \"local\" filename")
        && text.contains("At least one warning-error was detected")
        && !text.contains("bad CRC")
        && !text.contains("invalid")
        && !text.contains("error detected");
    assert!(
        only_name_warning,
        "unzip -t failed for {}\n{}",
        path.display(),
        text
    );
}

#[test]
fn copy_within_file_handles_overlapping_forward_copy() {
    let path = unique_temp_path("overlap.bin");
    let original: Vec<u8> = (0..=255).cycle().take(400_000).collect();
    fs::write(&path, &original).unwrap();

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let src = 131u64;
    let dst = 19u64;
    let len = 300_000u64;

    copy_within_file(&mut file, src, dst, len).unwrap();

    file.seek(SeekFrom::Start(0)).unwrap();
    let mut actual = Vec::new();
    file.read_to_end(&mut actual).unwrap();

    let mut expected = original.clone();
    expected[dst as usize..(dst + len) as usize]
        .copy_from_slice(&original[src as usize..(src + len) as usize]);

    assert_eq!(actual, expected);

    let _ = fs::remove_file(path);
}

fn make_options() -> Options {
    Options {
        dry_run: false,
        fast: false,
        not_utf8: false,
        no_default_exclude: false,
        extra_excludes: Vec::new(),
    }
}

fn make_eocd(
    disk: u16,
    cd_disk: u16,
    entries_this: u16,
    total_entries: u16,
    cd_size: u32,
    cd_offset: u32,
    comment_len: u16,
) -> [u8; 22] {
    let mut eocd = [0u8; 22];
    write_u32(&mut eocd, 0, EOCD_SIG);
    write_u16(&mut eocd, 4, disk);
    write_u16(&mut eocd, 6, cd_disk);
    write_u16(&mut eocd, 8, entries_this);
    write_u16(&mut eocd, 10, total_entries);
    write_u32(&mut eocd, 12, cd_size);
    write_u32(&mut eocd, 16, cd_offset);
    write_u16(&mut eocd, 20, comment_len);
    eocd
}

fn make_zip64_extra(uncompressed: u64, compressed: u64, lhf_offset: u64) -> Vec<u8> {
    let mut extra = vec![0u8; 4 + 24];
    write_u16(&mut extra, 0, ZIP64_EXTRA_FIELD_ID);
    write_u16(&mut extra, 2, 24);
    write_u64_slice(&mut extra, 4, uncompressed);
    write_u64_slice(&mut extra, 12, compressed);
    write_u64_slice(&mut extra, 20, lhf_offset);
    extra
}

fn make_extra_field(id: u16, data: &[u8]) -> Vec<u8> {
    let mut extra = vec![0u8; 4];
    write_u16(&mut extra, 0, id);
    write_u16(&mut extra, 2, data.len() as u16);
    extra.extend_from_slice(data);
    extra
}

fn make_cd_entry_raw(
    name: &[u8],
    extra: &[u8],
    comment: &[u8],
    flags: u16,
    compressed_size: u32,
    uncompressed_size: u32,
    lhf_offset: u32,
) -> Vec<u8> {
    let mut raw = vec![0u8; 46];
    write_u32_slice(&mut raw, 0, CENTRAL_DIR_SIG);
    write_u16(&mut raw, 8, flags);
    write_u32_slice(&mut raw, 20, compressed_size);
    write_u32_slice(&mut raw, 24, uncompressed_size);
    write_u16(&mut raw, 28, name.len() as u16);
    write_u16(&mut raw, 30, extra.len() as u16);
    write_u16(&mut raw, 32, comment.len() as u16);
    write_u32_slice(&mut raw, 42, lhf_offset);
    raw.extend_from_slice(name);
    raw.extend_from_slice(extra);
    raw.extend_from_slice(comment);
    raw
}

fn append_lfh(zip: &mut Vec<u8>, name: &[u8], payload: &[u8]) -> u64 {
    let offset = zip.len() as u64;
    let mut header = [0u8; 30];
    write_u32_slice(&mut header, 0, LOCAL_FILE_HEADER_SIG);
    write_u32_slice(&mut header, 18, payload.len() as u32);
    write_u32_slice(&mut header, 22, payload.len() as u32);
    write_u16(&mut header, 26, name.len() as u16);
    zip.extend_from_slice(&header);
    zip.extend_from_slice(name);
    zip.extend_from_slice(payload);
    offset
}

fn make_entry_plan(
    orig_name: &[u8],
    new_name: &[u8],
    cd_entry_raw: Vec<u8>,
    lhf_offset_in_zip64_extra: bool,
    new_bit11_set: bool,
) -> EntryPlan {
    let mut cd_header = [0u8; 46];
    cd_header.copy_from_slice(&cd_entry_raw[..46]);
    let fname_len = read_u16(&cd_header, 28) as usize;
    let extra_len = read_u16(&cd_header, 30) as usize;
    let comment_len = read_u16(&cd_header, 32) as usize;
    let extra_start = 46 + fname_len;
    let comment_start = extra_start + extra_len;
    let comment_end = comment_start + comment_len;
    EntryPlan {
        cd_index: 0,
        lhf_offset: 0,
        excluded: false,
        orig_fname: orig_name.to_vec(),
        new_fname: new_name.to_vec(),
        needs_bit11: new_bit11_set,
        new_bit11_set,
        span_size: 0,
        lhf_header_size: 0,
        lhf_extra_len: 0,
        lhf_offset_in_zip64_extra,
        cd_header,
        cd_extra: cd_entry_raw[extra_start..comment_start].to_vec(),
        cd_comment: cd_entry_raw[comment_start..comment_end].to_vec(),
    }
}

#[test]
fn find_archive_info_rejects_too_small_file() {
    let mut cursor = Cursor::new(vec![0u8; 21]);
    let err = find_archive_info(&mut cursor, 21).unwrap_err();
    assert_eq!(err, "file is too small to be a valid ZIP archive");
}

#[test]
fn find_archive_info_rejects_missing_eocd() {
    let bytes = vec![0u8; 64];
    let mut cursor = Cursor::new(bytes.clone());
    let err = find_archive_info(&mut cursor, bytes.len() as u64).unwrap_err();
    assert!(err.contains("End of Central Directory record not found"));
}

#[test]
fn find_archive_info_accepts_max_comment_length() {
    let comment = vec![0xA5; 65_535];
    let eocd = make_eocd(0, 0, 0, 0, 0, 0, comment.len() as u16);
    let mut bytes = eocd.to_vec();
    bytes.extend_from_slice(&comment);

    let mut cursor = Cursor::new(bytes.clone());
    let info = find_archive_info(&mut cursor, bytes.len() as u64).unwrap();

    assert_eq!(info.archive_comment, comment);
    assert_eq!(info.total_entries, 0);
}

#[test]
fn find_archive_info_ignores_eocd_signature_inside_comment() {
    let mut comment = b"before".to_vec();
    comment.extend_from_slice(&EOCD_SIG.to_le_bytes());
    comment.extend_from_slice(b"after");
    let eocd = make_eocd(0, 0, 0, 0, 0, 0, comment.len() as u16);
    let mut bytes = eocd.to_vec();
    bytes.extend_from_slice(&comment);

    let mut cursor = Cursor::new(bytes.clone());
    let info = find_archive_info(&mut cursor, bytes.len() as u64).unwrap();

    assert_eq!(info.archive_comment, comment);
}

#[test]
fn parse_eocd_rejects_multi_disk_archives() {
    let eocd = make_eocd(1, 0, 1, 1, 32, 64, 0);
    let mut cursor = Cursor::new(eocd.to_vec());
    let err = parse_eocd(&mut cursor, &eocd, 0, eocd.len() as u64).unwrap_err();
    assert_eq!(err, "multi-disk ZIP archives are not supported");
}

#[test]
fn parse_eocd_reads_archive_comment() {
    let comment = b"hello-comment";
    let eocd = make_eocd(0, 0, 2, 2, 0, 0, comment.len() as u16);
    let mut bytes = eocd.to_vec();
    bytes.extend_from_slice(comment);

    let mut cursor = Cursor::new(bytes.clone());
    let info = parse_eocd(&mut cursor, &eocd, 0, bytes.len() as u64).unwrap();

    assert_eq!(info.cd_offset, 0);
    assert_eq!(info.total_entries, 2);
    assert_eq!(info.archive_comment, comment);
    assert!(!info.is_zip64);
}

#[test]
fn parse_eocd_rejects_cd_range_overlapping_eocd() {
    let eocd = make_eocd(0, 0, 1, 1, 32, 10, 0);
    let mut cursor = Cursor::new(eocd.to_vec());
    let err = parse_eocd(&mut cursor, &eocd, 22, 44).unwrap_err();

    assert!(err.contains("Central Directory overlaps end records"));
}

#[test]
fn parse_eocd_reads_zip64_metadata() {
    let comment = b"zip64-comment";
    let mut cursor = Cursor::new(Vec::new());
    let mut pos = 0u64;
    write_zip64_eocd(&mut cursor, &mut pos, 7, 0, 0, comment).unwrap();
    let bytes = cursor.into_inner();
    let eocd_offset = 56 + 20;
    let mut eocd = [0u8; 22];
    eocd.copy_from_slice(&bytes[eocd_offset..eocd_offset + 22]);

    let mut reader = Cursor::new(bytes.clone());
    let info = parse_eocd(&mut reader, &eocd, eocd_offset as u64, bytes.len() as u64).unwrap();

    assert!(info.is_zip64);
    assert_eq!(info.total_entries, 7);
    assert_eq!(info.cd_offset, 0);
    assert_eq!(info.archive_comment, comment);
}

#[test]
fn parse_eocd_rejects_missing_zip64_locator_signature() {
    let eocd = make_eocd(0, 0, 0xFFFF, 0xFFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, 0);
    let mut bytes = vec![0u8; 20];
    bytes.extend_from_slice(&eocd);

    let mut cursor = Cursor::new(bytes);
    let err = parse_eocd(&mut cursor, &eocd, 20, 42).unwrap_err();
    assert_eq!(err, "ZIP64 EOCD locator signature not found");
}

#[test]
fn parse_eocd_rejects_zip64_locator_multi_disk() {
    let mut cursor = Cursor::new(Vec::new());
    let mut pos = 0u64;
    write_zip64_eocd(&mut cursor, &mut pos, 1, 0, 0, b"").unwrap();
    let mut bytes = cursor.into_inner();
    write_u32_slice(&mut bytes, 56 + 16, 2);

    let eocd_offset = 56 + 20;
    let mut eocd = [0u8; 22];
    eocd.copy_from_slice(&bytes[eocd_offset..eocd_offset + 22]);

    let mut reader = Cursor::new(bytes.clone());
    let err = parse_eocd(&mut reader, &eocd, eocd_offset as u64, bytes.len() as u64).unwrap_err();
    assert_eq!(err, "multi-disk ZIP64 archives are not supported");
}

#[test]
fn parse_eocd_rejects_invalid_zip64_eocd_signature() {
    let mut cursor = Cursor::new(Vec::new());
    let mut pos = 0u64;
    write_zip64_eocd(&mut cursor, &mut pos, 1, 0, 0, b"").unwrap();
    let mut bytes = cursor.into_inner();
    write_u32_slice(&mut bytes, 0, 0);

    let eocd_offset = 56 + 20;
    let mut eocd = [0u8; 22];
    eocd.copy_from_slice(&bytes[eocd_offset..eocd_offset + 22]);

    let mut reader = Cursor::new(bytes.clone());
    let err = parse_eocd(&mut reader, &eocd, eocd_offset as u64, bytes.len() as u64).unwrap_err();
    assert_eq!(err, "invalid ZIP64 EOCD signature");
}

#[test]
fn parse_eocd_rejects_zip64_entry_count_mismatch() {
    let mut cursor = Cursor::new(Vec::new());
    let mut pos = 0u64;
    write_zip64_eocd(&mut cursor, &mut pos, 1, 0, 0, b"").unwrap();
    let mut bytes = cursor.into_inner();
    write_u64_slice(&mut bytes, 32, 2);

    let eocd_offset = 56 + 20;
    let mut eocd = [0u8; 22];
    eocd.copy_from_slice(&bytes[eocd_offset..eocd_offset + 22]);

    let mut reader = Cursor::new(bytes.clone());
    let err = parse_eocd(&mut reader, &eocd, eocd_offset as u64, bytes.len() as u64).unwrap_err();
    assert!(err.contains("entry count mismatch in ZIP64 EOCD"));
}

#[test]
fn parse_eocd_rejects_too_small_zip64_eocd_record_size() {
    let mut cursor = Cursor::new(Vec::new());
    let mut pos = 0u64;
    write_zip64_eocd(&mut cursor, &mut pos, 1, 0, 0, b"").unwrap();
    let mut bytes = cursor.into_inner();
    write_u64_slice(&mut bytes, 4, 43);

    let eocd_offset = 56 + 20;
    let mut eocd = [0u8; 22];
    eocd.copy_from_slice(&bytes[eocd_offset..eocd_offset + 22]);

    let mut reader = Cursor::new(bytes.clone());
    let err = parse_eocd(&mut reader, &eocd, eocd_offset as u64, bytes.len() as u64).unwrap_err();
    assert_eq!(err, "ZIP64 EOCD record is too small");
}

#[test]
fn parse_eocd_rejects_zip64_eocd_record_overlapping_locator() {
    let mut cursor = Cursor::new(Vec::new());
    let mut pos = 0u64;
    write_zip64_eocd(&mut cursor, &mut pos, 1, 0, 0, b"").unwrap();
    let mut bytes = cursor.into_inner();
    write_u64_slice(&mut bytes, 4, 45);

    let eocd_offset = 56 + 20;
    let mut eocd = [0u8; 22];
    eocd.copy_from_slice(&bytes[eocd_offset..eocd_offset + 22]);

    let mut reader = Cursor::new(bytes.clone());
    let err = parse_eocd(&mut reader, &eocd, eocd_offset as u64, bytes.len() as u64).unwrap_err();
    assert_eq!(err, "ZIP64 EOCD record overlaps ZIP64 locator");
}

#[test]
fn resolve_lfh_offset_reads_zip64_field() {
    let extra = make_zip64_extra(0x11, 0x22, 0x33);
    let resolved = resolve_lfh_offset(0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, &extra, 1).unwrap();

    assert_eq!(resolved, (0x33, true));
}

#[test]
fn resolve_lfh_offset_skips_unknown_extra_and_uses_sentinel_layout() {
    let mut extra = make_extra_field(0xCAFE, b"skip");
    let mut zip64_data = vec![0u8; 16];
    write_u64_slice(&mut zip64_data, 0, 0x11);
    write_u64_slice(&mut zip64_data, 8, 0x44);
    extra.extend_from_slice(&make_extra_field(ZIP64_EXTRA_FIELD_ID, &zip64_data));

    let resolved = resolve_lfh_offset(0x22, 0xFFFF_FFFF, 0xFFFF_FFFF, &extra, 3).unwrap();

    assert_eq!(resolved, (0x44, true));
}

#[test]
fn resolve_lfh_offset_rejects_zip64_extra_without_offset_slot() {
    let extra = make_extra_field(ZIP64_EXTRA_FIELD_ID, &[0u8; 16]);
    let err = resolve_lfh_offset(0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, &extra, 5).unwrap_err();

    assert_eq!(err, "ZIP64 extra too short in CD entry 5");
}

#[test]
fn resolve_lfh_offset_rejects_truncated_extra_field() {
    let extra = vec![1, 0, 8, 0, 1, 2, 3];
    let err = resolve_lfh_offset(10, 20, 0xFFFF_FFFF, &extra, 4).unwrap_err();
    assert_eq!(err, "truncated extra field in CD entry 4");
}

#[test]
fn resolve_lfh_offset_rejects_missing_zip64_extra() {
    let err = resolve_lfh_offset(10, 20, 0xFFFF_FFFF, &[], 2).unwrap_err();
    assert_eq!(err, "ZIP64 extra field missing for CD entry 2");
}

#[test]
fn build_plans_rejects_trailing_cd_bytes_after_expected_entries() {
    let mut zip = Vec::new();
    let lhf_offset = append_lfh(&mut zip, b"entry.txt", b"payload");
    let cd_offset = zip.len() as u64;
    let cd = make_cd_entry_raw(b"entry.txt", &[], &[], 0, 7, 7, lhf_offset as u32);
    zip.extend_from_slice(&cd);
    zip.push(0xAA);

    let info = ArchiveInfo {
        cd_offset,
        cd_size: cd.len() as u64 + 1,
        total_entries: 1,
        is_zip64: false,
        archive_comment: Vec::new(),
    };

    let err = build_plans(&mut Cursor::new(zip), &info, &make_options()).unwrap_err();
    assert!(err.contains("trailing bytes after expected entries"));
}

#[test]
fn build_plans_rejects_truncated_cd_entry() {
    let info = ArchiveInfo {
        cd_offset: 0,
        cd_size: 45,
        total_entries: 1,
        is_zip64: false,
        archive_comment: Vec::new(),
    };

    let err = build_plans(&mut Cursor::new(vec![0u8; 45]), &info, &make_options()).unwrap_err();
    assert!(err.contains("unexpected EOF reading CD entry header"));
}

#[test]
fn build_plans_rejects_invalid_lfh_signature() {
    let mut zip = Vec::new();
    let lhf_offset = append_lfh(&mut zip, b"entry.txt", b"payload");
    write_u32_slice(&mut zip, lhf_offset as usize, 0);
    let cd_offset = zip.len() as u64;
    let cd = make_cd_entry_raw(b"entry.txt", &[], &[], 0, 7, 7, lhf_offset as u32);
    zip.extend_from_slice(&cd);

    let info = ArchiveInfo {
        cd_offset,
        cd_size: cd.len() as u64,
        total_entries: 1,
        is_zip64: false,
        archive_comment: Vec::new(),
    };

    let err = build_plans(&mut Cursor::new(zip), &info, &make_options()).unwrap_err();
    assert!(err.contains("invalid LFH signature at entry 1"));
}

#[test]
fn build_plans_rejects_truncated_lfh_filename() {
    let cd = make_cd_entry_raw(b"abcdefghij", &[], &[], 0, 0, 0, 56);
    let mut zip = cd.clone();
    let lhf_offset = zip.len() as u64;
    assert_eq!(lhf_offset, 56);
    let mut header = [0u8; 30];
    write_u32_slice(&mut header, 0, LOCAL_FILE_HEADER_SIG);
    write_u16(&mut header, 26, 10);
    zip.extend_from_slice(&header);
    zip.extend_from_slice(b"abc");

    let info = ArchiveInfo {
        cd_offset: 0,
        cd_size: cd.len() as u64,
        total_entries: 1,
        is_zip64: false,
        archive_comment: Vec::new(),
    };

    let err = build_plans(&mut Cursor::new(zip), &info, &make_options()).unwrap_err();
    assert!(err.contains("unexpected EOF reading LFH filename at entry 1"));
}

#[test]
fn build_plans_rejects_overlapping_lfh_offsets() {
    let mut zip = Vec::new();
    let lhf_offset = append_lfh(&mut zip, b"entry.txt", b"payload");
    let cd_offset = zip.len() as u64;
    let first_cd = make_cd_entry_raw(b"entry.txt", &[], &[], 0, 7, 7, lhf_offset as u32);
    let second_cd = make_cd_entry_raw(b"entry.txt", &[], &[], 0, 7, 7, lhf_offset as u32);
    zip.extend_from_slice(&first_cd);
    zip.extend_from_slice(&second_cd);

    let info = ArchiveInfo {
        cd_offset,
        cd_size: (first_cd.len() + second_cd.len()) as u64,
        total_entries: 2,
        is_zip64: false,
        archive_comment: Vec::new(),
    };

    let err = build_plans(&mut Cursor::new(zip), &info, &make_options()).unwrap_err();
    assert!(err.contains("entry 1 has an invalid physical span"));
}

#[test]
fn build_plans_rejects_lfh_cd_filename_length_mismatch() {
    let mut zip = Vec::new();
    let lhf_offset = append_lfh(&mut zip, b"long-name.txt", b"payload");
    let cd_offset = zip.len() as u64;
    let cd = make_cd_entry_raw(b"short.txt", &[], &[], 0, 7, 7, lhf_offset as u32);
    zip.extend_from_slice(&cd);

    let info = ArchiveInfo {
        cd_offset,
        cd_size: cd.len() as u64,
        total_entries: 1,
        is_zip64: false,
        archive_comment: Vec::new(),
    };

    let err = build_plans(&mut Cursor::new(zip), &info, &make_options()).unwrap_err();
    assert!(err.contains("LFH filename length mismatch at entry 1"));
    assert!(err.contains("LFH has 13, CD has 9"));
}

#[test]
fn build_plans_rejects_lfh_cd_filename_bytes_mismatch() {
    let mut zip = Vec::new();
    let lhf_offset = append_lfh(&mut zip, b"same-len-a.txt", b"payload");
    let cd_offset = zip.len() as u64;
    let cd = make_cd_entry_raw(b"same-len-b.txt", &[], &[], 0, 7, 7, lhf_offset as u32);
    zip.extend_from_slice(&cd);

    let info = ArchiveInfo {
        cd_offset,
        cd_size: cd.len() as u64,
        total_entries: 1,
        is_zip64: false,
        archive_comment: Vec::new(),
    };

    let err = build_plans(&mut Cursor::new(zip), &info, &make_options()).unwrap_err();
    assert_eq!(err, "LFH filename bytes mismatch at entry 1");
}

#[test]
fn build_plans_computes_spans_in_lfh_offset_order_not_cd_order() {
    let mut zip = Vec::new();
    let first_offset = append_lfh(&mut zip, b"first.txt", b"one");
    let second_offset = append_lfh(&mut zip, b"second.txt", b"two!!");
    let cd_offset = zip.len() as u64;

    let second_cd = make_cd_entry_raw(b"second.txt", &[], &[], 0, 5, 5, second_offset as u32);
    let first_cd = make_cd_entry_raw(b"first.txt", &[], &[], 0, 3, 3, first_offset as u32);
    zip.extend_from_slice(&second_cd);
    zip.extend_from_slice(&first_cd);

    let info = ArchiveInfo {
        cd_offset,
        cd_size: (second_cd.len() + first_cd.len()) as u64,
        total_entries: 2,
        is_zip64: false,
        archive_comment: Vec::new(),
    };

    let plans = build_plans(&mut Cursor::new(zip), &info, &make_options()).unwrap();

    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].orig_fname, b"first.txt");
    assert_eq!(plans[1].orig_fname, b"second.txt");
    assert_eq!(plans[0].span_size, second_offset - first_offset);
    assert_eq!(plans[1].span_size, cd_offset - second_offset);
    assert_eq!(plans[0].cd_index, 1);
    assert_eq!(plans[1].cd_index, 0);
}

#[test]
fn process_new_preserves_original_cd_order() {
    let mut zip = Vec::new();
    let first_offset = append_lfh(&mut zip, b"first.txt", b"one");
    let second_offset = append_lfh(&mut zip, b"second.txt", b"two");
    let cd_offset = zip.len() as u64;

    let second_cd = make_cd_entry_raw(b"second.txt", &[], &[], 0, 3, 3, second_offset as u32);
    let first_cd = make_cd_entry_raw(b"first.txt", &[], &[], 0, 3, 3, first_offset as u32);
    zip.extend_from_slice(&second_cd);
    zip.extend_from_slice(&first_cd);
    zip.extend_from_slice(&make_eocd(
        0,
        0,
        2,
        2,
        (second_cd.len() + first_cd.len()) as u32,
        cd_offset as u32,
        0,
    ));

    let mut input = Cursor::new(zip.clone());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        zip.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    let out = output.into_inner();
    let mut file = Cursor::new(out);
    let len = file.get_ref().len() as u64;
    let info = find_archive_info(&mut file, len).unwrap();
    file.seek(SeekFrom::Start(info.cd_offset)).unwrap();

    let mut cd_header = [0u8; 46];
    file.read_exact(&mut cd_header).unwrap();
    let first_name_len = read_u16(&cd_header, 28) as usize;
    let mut first_name = vec![0u8; first_name_len];
    file.read_exact(&mut first_name).unwrap();

    assert_eq!(first_name, b"second.txt");
}

#[test]
fn build_cd_entry_updates_32bit_offset_name_and_comment() {
    let extra = vec![0xAA, 0xBB];
    let comment = b"note";
    let cd_raw = make_cd_entry_raw(b"old-name.txt", &extra, comment, 0, 10, 20, 0x01020304);
    let plan = make_entry_plan(b"old-name.txt", b"new.txt", cd_raw, false, true);

    let out = build_cd_entry(&plan, 0x11223344).unwrap();

    assert_eq!(read_u32(&out, 0), CENTRAL_DIR_SIG);
    assert_eq!(read_u16(&out, 8), BIT11);
    assert_eq!(read_u16(&out, 28) as usize, b"new.txt".len());
    assert_eq!(read_u32(&out, 42), 0x11223344);
    assert_eq!(&out[46..46 + b"new.txt".len()], b"new.txt");
    assert_eq!(
        &out[46 + b"new.txt".len()..46 + b"new.txt".len() + extra.len()],
        extra.as_slice()
    );
    assert_eq!(&out[out.len() - comment.len()..], comment);
}

#[test]
fn build_cd_entry_updates_zip64_extra_offset() {
    let extra = make_zip64_extra(0x11, 0x22, 0x33);
    let cd_raw = make_cd_entry_raw(
        b"entry.bin",
        &extra,
        b"",
        0,
        0xFFFF_FFFF,
        0xFFFF_FFFF,
        0xFFFF_FFFF,
    );
    let plan = make_entry_plan(b"entry.bin", b"entry.bin", cd_raw, true, false);

    let out = build_cd_entry(&plan, 0x0102_0304_0506_0708).unwrap();
    let extra_start = 46 + b"entry.bin".len();

    assert_eq!(read_u32(&out, 42), 0xFFFF_FFFF);
    assert_eq!(read_u64(&out, extra_start + 4 + 16), 0x0102_0304_0506_0708);
}

#[test]
fn build_cd_entry_rejects_large_offset_without_zip64_extra() {
    let cd_raw = make_cd_entry_raw(b"name", &[], b"", 0, 1, 1, 0x10);
    let plan = make_entry_plan(b"name", b"name", cd_raw, false, false);

    let err = build_cd_entry(&plan, 0x1_0000_0000).unwrap_err();
    assert!(err.contains("no ZIP64 extra field present"));
}

#[test]
fn build_cd_entry_rejects_short_zip64_extra_when_patching_offset() {
    let extra = make_extra_field(ZIP64_EXTRA_FIELD_ID, &[0u8; 16]);
    let cd_raw = make_cd_entry_raw(
        b"entry.bin",
        &extra,
        b"",
        0,
        0xFFFF_FFFF,
        0xFFFF_FFFF,
        0xFFFF_FFFF,
    );
    let plan = make_entry_plan(b"entry.bin", b"entry.bin", cd_raw, true, false);

    let err = build_cd_entry(&plan, 0x0102_0304_0506_0708).unwrap_err();

    assert!(err.contains("ZIP64 extra too short to hold LFH offset for CD entry 1"));
}

#[test]
fn dry_run_report_summarizes_changes() {
    let plans = vec![
        EntryPlan {
            cd_index: 0,
            lhf_offset: 0,
            excluded: true,
            orig_fname: b"__MACOSX/ghost".to_vec(),
            new_fname: b"__MACOSX/ghost".to_vec(),
            needs_bit11: false,
            new_bit11_set: false,
            span_size: 120,
            lhf_header_size: 30,
            lhf_extra_len: 0,
            lhf_offset_in_zip64_extra: false,
            cd_header: [0u8; 46],
            cd_extra: Vec::new(),
            cd_comment: Vec::new(),
        },
        EntryPlan {
            cd_index: 1,
            lhf_offset: 120,
            excluded: false,
            orig_fname: "e\u{301}.txt".as_bytes().to_vec(),
            new_fname: "é.txt".as_bytes().to_vec(),
            needs_bit11: true,
            new_bit11_set: true,
            span_size: 50,
            lhf_header_size: 30,
            lhf_extra_len: 0,
            lhf_offset_in_zip64_extra: false,
            cd_header: [0u8; 46],
            cd_extra: Vec::new(),
            cd_comment: Vec::new(),
        },
    ];

    let mut out = Vec::new();
    dry_run_report(&plans, &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();

    assert!(out.contains("[exclude]  __MACOSX/ghost  (120 B)"));
    assert!(out.contains("[nfc]      e\u{301}.txt  →  é.txt  (1 B shorter)"));
    assert!(out.contains("[bit11]    e\u{301}.txt"));
    assert!(out.contains("Excluded:     1 entries (orphan data: 120 B)"));
    assert!(out.contains("NFC renamed:  1 entries (total saved: 1 B)"));
    assert!(out.contains("bit11 set:    1 entries"));
}

#[test]
fn dry_run_report_marks_unknown_excluded_size() {
    let plans = vec![EntryPlan {
        cd_index: 0,
        lhf_offset: 0,
        excluded: true,
        orig_fname: b"__MACOSX/ghost".to_vec(),
        new_fname: b"__MACOSX/ghost".to_vec(),
        needs_bit11: false,
        new_bit11_set: false,
        span_size: 0,
        lhf_header_size: 0,
        lhf_extra_len: 0,
        lhf_offset_in_zip64_extra: false,
        cd_header: [0u8; 46],
        cd_extra: Vec::new(),
        cd_comment: Vec::new(),
    }];

    let mut out = Vec::new();
    dry_run_report(&plans, &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();

    assert!(out.contains("[exclude]  __MACOSX/ghost  (? B)"));
    assert!(out.contains("Excluded:     1 entries (orphan data: ? B)"));
}

#[test]
fn default_excludes_match_basename_and_macosx_prefix() {
    let opts = make_options();

    assert!(opts.is_excluded(b".DS_Store"));
    assert!(opts.is_excluded(b"nested/.DS_Store"));
    assert!(opts.is_excluded(b"Thumbs.db"));
    assert!(opts.is_excluded(b"dir/Thumbs.db"));
    assert!(opts.is_excluded(b"__MACOSX"));
    assert!(opts.is_excluded(b"__MACOSX/path/file.txt"));

    assert!(!opts.is_excluded(b"dir/__MACOSX/file.txt"));
    assert!(!opts.is_excluded(b"notes/.DS_Store.backup"));
}

#[test]
fn default_excludes_include_desktop_ini() {
    let opts = make_options();
    assert!(opts.is_excluded(b"desktop.ini"));
    assert!(opts.is_excluded(b"subdir/desktop.ini"));
}

#[test]
fn no_default_exclude_disables_builtin_filters() {
    let mut opts = make_options();
    opts.no_default_exclude = true;

    assert!(!opts.is_excluded(b".DS_Store"));
    assert!(!opts.is_excluded(b"nested/Thumbs.db"));
    assert!(!opts.is_excluded(b"__MACOSX/file.txt"));
}

#[test]
fn extra_excludes_match_basename_only() {
    let mut opts = make_options();
    opts.no_default_exclude = true;
    opts.extra_excludes = vec!["keep.out".to_string()];

    assert!(opts.is_excluded(b"keep.out"));
    assert!(opts.is_excluded(b"nested/keep.out"));
    assert!(!opts.is_excluded(b"keep.out/child.txt"));
    assert!(!opts.is_excluded(b"nested/keep.out.backup"));
}

fn manifest_archive(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("files")
        .join(name)
}

fn create_simple_zip(zip_path: &Path, filename: &str, contents: &[u8]) {
    let dir = unique_temp_path("zipdir");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(filename), contents).unwrap();

    let status = Command::new("zip")
        .arg("-q")
        .arg(zip_path)
        .arg(filename)
        .current_dir(&dir)
        .status()
        .expect("failed to launch zip");
    assert!(status.success(), "zip failed for {}", zip_path.display());

    let _ = fs::remove_dir_all(dir);
}

fn add_archive_comment(path: &Path, comment: &[u8]) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let eocd_offset = find_eocd_offset_for_test(path);
    file.seek(SeekFrom::Start(eocd_offset + 20)).unwrap();
    file.write_all(&(comment.len() as u16).to_le_bytes())
        .unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    file.write_all(comment).unwrap();
}

fn read_archive_info(path: &Path) -> ArchiveInfo {
    let mut file = File::open(path).unwrap();
    let file_len = fs::metadata(path).unwrap().len();
    find_archive_info(&mut file, file_len).unwrap()
}

fn find_eocd_offset_for_test(path: &Path) -> u64 {
    let bytes = fs::read(path).unwrap();
    for pos in (0..=bytes.len() - 22).rev() {
        if read_u32(&bytes, pos) == EOCD_SIG {
            let comment_len = read_u16(&bytes, pos + 20) as usize;
            if pos + 22 + comment_len == bytes.len() {
                return pos as u64;
            }
        }
    }
    panic!("EOCD not found in {}", path.display());
}

fn first_lfh_extra(path: &Path) -> Vec<u8> {
    lfh_extra_at(path, 0)
}

fn lfh_name_and_flags_at(path: &Path, offset: u64) -> (Vec<u8>, u16) {
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();

    let mut lhf = [0u8; 30];
    file.read_exact(&mut lhf).unwrap();
    assert_eq!(read_u32(&lhf, 0), LOCAL_FILE_HEADER_SIG);

    let flags = read_u16(&lhf, 6);
    let name_len = read_u16(&lhf, 26) as usize;
    let mut name = vec![0u8; name_len];
    file.read_exact(&mut name).unwrap();

    (name, flags)
}

fn lfh_extra_at(path: &Path, offset: u64) -> Vec<u8> {
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut lhf = [0u8; 30];
    file.read_exact(&mut lhf).unwrap();
    assert_eq!(read_u32(&lhf, 0), LOCAL_FILE_HEADER_SIG);

    let fname_len = read_u16(&lhf, 26) as u64;
    let extra_len = read_u16(&lhf, 28) as usize;
    file.seek(SeekFrom::Current(fname_len as i64)).unwrap();

    let mut extra = vec![0u8; extra_len];
    file.read_exact(&mut extra).unwrap();
    extra
}

fn has_extra_field(extra: &[u8], field_id: u16) -> bool {
    let mut cursor = 0usize;
    while cursor + 4 <= extra.len() {
        let id = read_u16(extra, cursor);
        let sz = read_u16(extra, cursor + 2) as usize;
        cursor += 4;
        if cursor + sz > extra.len() {
            return false;
        }
        if id == field_id {
            return true;
        }
        cursor += sz;
    }
    false
}

fn read_first_entry_flags(path: &Path) -> (u16, u16) {
    let mut file = File::open(path).unwrap();

    let mut lhf = [0u8; 30];
    file.read_exact(&mut lhf).unwrap();
    assert_eq!(read_u32(&lhf, 0), LOCAL_FILE_HEADER_SIG);
    let lhf_flags = read_u16(&lhf, 6);

    let file_len = fs::metadata(path).unwrap().len();
    let info = find_archive_info(&mut file, file_len).unwrap();
    file.seek(SeekFrom::Start(info.cd_offset)).unwrap();

    let mut cd = [0u8; 46];
    file.read_exact(&mut cd).unwrap();
    assert_eq!(read_u32(&cd, 0), CENTRAL_DIR_SIG);
    let cd_flags = read_u16(&cd, 8);

    (lhf_flags, cd_flags)
}

fn cd_names_and_flags(path: &Path) -> Vec<(Vec<u8>, u16)> {
    let mut file = File::open(path).unwrap();
    let file_len = fs::metadata(path).unwrap().len();
    let info = find_archive_info(&mut file, file_len).unwrap();
    file.seek(SeekFrom::Start(info.cd_offset)).unwrap();

    let mut entries = Vec::new();
    for _ in 0..info.total_entries {
        let mut cd = [0u8; 46];
        file.read_exact(&mut cd).unwrap();
        assert_eq!(read_u32(&cd, 0), CENTRAL_DIR_SIG);
        let flags = read_u16(&cd, 8);
        let name_len = read_u16(&cd, 28) as usize;
        let extra_len = read_u16(&cd, 30) as usize;
        let comment_len = read_u16(&cd, 32) as usize;

        let mut name = vec![0u8; name_len];
        file.read_exact(&mut name).unwrap();
        file.seek(SeekFrom::Current((extra_len + comment_len) as i64))
            .unwrap();
        entries.push((name, flags));
    }

    entries
}

#[test]
fn inplace_process_fixture_archive_stays_valid() {
    let src = manifest_archive("test.zip");
    assert!(src.exists(), "fixture missing: {}", src.display());
    let src_len = fs::metadata(&src).unwrap().len();

    let dst = unique_temp_path("test-copy.zip");
    fs::copy(&src, &dst).unwrap();

    let mut output = Vec::new();
    process_file(dst.to_str().unwrap(), &make_options(), &mut output).unwrap();

    assert_unzip_test_accepts(&dst);

    let file_len = fs::metadata(&dst).unwrap().len();
    let mut file = File::open(&dst).unwrap();
    let info = find_archive_info(&mut file, file_len).unwrap();
    let plans = build_plans(&mut file, &info, &make_options()).unwrap();
    assert!(plans.iter().all(|p| !p.excluded));
    assert!(plans.iter().all(|p| p.span_size >= p.lhf_header_size));
    assert!(plans.iter().all(|p| p.lhf_offset < info.cd_offset));
    assert!(file_len <= src_len);

    let _ = fs::remove_file(dst);
}

#[test]
fn fast_mode_rewrites_only_central_directory() {
    let kept_nfd = "e\u{301}.txt".as_bytes();
    let excluded = b"__MACOSX/ghost";
    let later = b"later.txt";

    let mut zip = Vec::new();
    let kept_offset = append_lfh(&mut zip, kept_nfd, b"");
    let excluded_offset = append_lfh(&mut zip, excluded, b"");
    let later_offset = append_lfh(&mut zip, later, b"");
    let cd_offset = zip.len() as u64;

    let mut cd = Vec::new();
    cd.extend_from_slice(&make_cd_entry_raw(
        kept_nfd,
        &[],
        &[],
        0,
        0,
        0,
        kept_offset as u32,
    ));
    cd.extend_from_slice(&make_cd_entry_raw(
        excluded,
        &[],
        &[],
        0,
        0,
        0,
        excluded_offset as u32,
    ));
    cd.extend_from_slice(&make_cd_entry_raw(
        later,
        &[],
        &[],
        0,
        0,
        0,
        later_offset as u32,
    ));
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 3, 3, cd.len() as u32, cd_offset as u32, 0));

    let src = unique_temp_path("fast-cd-only.zip");
    fs::write(&src, &zip).unwrap();

    let mut opts = make_options();
    opts.fast = true;
    process_file(src.to_str().unwrap(), &opts, &mut Vec::new()).unwrap();

    let after = fs::read(&src).unwrap();
    assert_eq!(&after[..cd_offset as usize], &zip[..cd_offset as usize]);

    let entries = cd_names_and_flags(&src);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "é.txt".as_bytes());
    assert_ne!(entries[0].1 & BIT11, 0);
    assert_eq!(entries[1].0, later);
    assert_eq!(entries[1].1 & BIT11, 0);

    assert_unzip_test_accepts(&src);

    let _ = fs::remove_file(src);
}

#[test]
fn inplace_exclude_shifts_orphan_to_absorb_unabsorbed_gap() {
    let root = unique_temp_path("zip-tree");
    fs::create_dir_all(root.join("__MACOSX")).unwrap();
    fs::write(root.join("e\u{301}.txt"), b"rename me").unwrap();
    fs::write(root.join("__MACOSX").join("ghost.txt"), b"junk").unwrap();
    fs::write(root.join("later.txt"), b"keep later").unwrap();

    let src = unique_temp_path("exclude-source.zip");
    let status = Command::new("zip")
        .arg("-q")
        .arg(&src)
        .arg("e\u{301}.txt")
        .arg("__MACOSX/ghost.txt")
        .arg("later.txt")
        .current_dir(&root)
        .status()
        .expect("failed to launch zip");
    assert!(status.success(), "zip failed for {}", src.display());

    let dst = unique_temp_path("exclude-cd-only.zip");
    fs::copy(&src, &dst).unwrap();

    let mut before = File::open(&dst).unwrap();
    let before_len = fs::metadata(&dst).unwrap().len();
    let before_info = find_archive_info(&mut before, before_len).unwrap();
    let before_plans = build_plans(&mut before, &before_info, &make_options()).unwrap();

    let renamed_before = before_plans
        .iter()
        .find(|p| p.orig_fname == "e\u{301}.txt".as_bytes())
        .unwrap();
    let carry_before_exclude = renamed_before.fname_delta();

    let excluded_spans: Vec<(u64, u64)> = before_plans
        .iter()
        .filter(|p| p.excluded)
        .map(|p| (p.lhf_offset, p.span_size))
        .collect();
    assert!(!excluded_spans.is_empty());
    assert!(carry_before_exclude > 0);
    let excluded_max = before_plans
        .iter()
        .filter(|p| p.excluded)
        .map(|p| p.lhf_offset)
        .max()
        .unwrap();
    let later_before = before_plans
        .iter()
        .find(|p| p.orig_fname == b"later.txt")
        .map(|p| p.lhf_offset)
        .unwrap();
    assert!(later_before > excluded_max);

    let mut output = Vec::new();
    process_file(dst.to_str().unwrap(), &make_options(), &mut output).unwrap();

    assert_unzip_test_accepts(&dst);

    let mut after = File::open(&dst).unwrap();
    let after_len = fs::metadata(&dst).unwrap().len();
    let after_info = find_archive_info(&mut after, after_len).unwrap();
    let after_plans = build_plans(&mut after, &after_info, &make_options()).unwrap();

    assert!(after_plans.iter().all(|p| !p.excluded));

    let later_after = after_plans
        .iter()
        .find(|p| p.orig_fname == b"later.txt")
        .map(|p| p.lhf_offset)
        .unwrap();
    assert_eq!(later_after, later_before);

    for &(offset, _) in &excluded_spans {
        if offset + 4 <= after_len {
            let mut sig = [0u8; 4];
            after.seek(SeekFrom::Start(offset)).unwrap();
            after.read_exact(&mut sig).unwrap();
            assert_ne!(read_u32(&sig, 0), LOCAL_FILE_HEADER_SIG);
        }
    }

    let mut shifted_orphan_offset = None;
    for shift in 1..=carry_before_exclude {
        let candidate = excluded_spans[0].0 - shift;
        let mut sig = [0u8; 4];
        after.seek(SeekFrom::Start(candidate)).unwrap();
        after.read_exact(&mut sig).unwrap();
        if read_u32(&sig, 0) == LOCAL_FILE_HEADER_SIG {
            shifted_orphan_offset = Some(candidate);
            break;
        }
    }
    assert!(shifted_orphan_offset.is_some());

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(src);
    let _ = fs::remove_file(dst);
}

#[test]
fn inplace_carries_small_filename_delta_across_large_payload() {
    let root = unique_temp_path("large-small-delta-tree");
    fs::create_dir_all(&root).unwrap();
    let nfd_name = "e\u{301}.txt";
    fs::write(root.join(nfd_name), vec![0x42; 11 * 1024 * 1024]).unwrap();
    fs::write(root.join("later.txt"), b"later").unwrap();

    let src = unique_temp_path("large-small-delta.zip");
    let status = Command::new("zip")
        .arg("-q")
        .arg("-0")
        .arg(&src)
        .arg(nfd_name)
        .arg("later.txt")
        .current_dir(&root)
        .status()
        .expect("failed to launch zip");
    assert!(status.success(), "zip failed for {}", src.display());

    let mut output = Vec::new();
    process_file(src.to_str().unwrap(), &make_options(), &mut output).unwrap();

    assert_unzip_test_accepts(&src);

    let mut after = File::open(&src).unwrap();
    let after_len = fs::metadata(&src).unwrap().len();
    let after_info = find_archive_info(&mut after, after_len).unwrap();
    let after_plans = build_plans(&mut after, &after_info, &make_options()).unwrap();

    assert!(after_plans
        .iter()
        .any(|p| p.orig_fname == "é.txt".as_bytes()));
    assert!(after_plans.iter().any(|p| p.orig_fname == b"later.txt"));

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(src);
}

#[test]
fn inplace_writes_padding_extra_when_absorb_reaches_minimum() {
    let src = unique_temp_path("padding-extra.zip");
    let nfd_name = "e\u{301}e\u{301}e\u{301}e\u{301}.txt".as_bytes();
    let mut zip = Vec::new();
    let lhf_offset = append_lfh(&mut zip, nfd_name, b"");
    let cd_offset = zip.len() as u64;
    let cd = make_cd_entry_raw(nfd_name, &[], &[], 0, 0, 0, lhf_offset as u32);
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_offset as u32, 0));
    fs::write(&src, zip).unwrap();

    let mut output = Vec::new();
    process_file(src.to_str().unwrap(), &make_options(), &mut output).unwrap();

    let extra = first_lfh_extra(&src);
    assert!(has_extra_field(&extra, 0xFFFF));

    let mut file = File::open(&src).unwrap();
    let file_len = fs::metadata(&src).unwrap().len();
    let info = find_archive_info(&mut file, file_len).unwrap();
    let plans = build_plans(&mut file, &info, &make_options()).unwrap();
    let (lfh_name, lfh_flags) = lfh_name_and_flags_at(&src, plans[0].lhf_offset);
    assert_eq!(lfh_name, plans[0].new_fname);
    assert_eq!(lfh_name, "éééé.txt".as_bytes());
    assert_ne!(lfh_flags & BIT11, 0);
    assert_ne!(read_u16(&plans[0].cd_header, 8) & BIT11, 0);

    assert_unzip_test_accepts(&src);

    let _ = fs::remove_file(src);
}

#[test]
fn inplace_accumulates_small_carry_until_padding_is_possible() {
    let src = unique_temp_path("carry-accumulate.zip");
    let names: [&[u8]; 3] = [
        "e\u{301}.txt".as_bytes(),
        "e\u{301}.dat".as_bytes(),
        "e\u{301}e\u{301}.bin".as_bytes(),
    ];

    let mut zip = Vec::new();
    let offsets: Vec<u64> = names
        .iter()
        .map(|name| append_lfh(&mut zip, name, b""))
        .collect();
    let cd_offset = zip.len() as u64;
    let mut cd = Vec::new();
    for (name, offset) in names.iter().zip(offsets) {
        cd.extend_from_slice(&make_cd_entry_raw(name, &[], &[], 0, 0, 0, offset as u32));
    }
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 3, 3, cd.len() as u32, cd_offset as u32, 0));
    fs::write(&src, zip).unwrap();

    let mut output = Vec::new();
    process_file(src.to_str().unwrap(), &make_options(), &mut output).unwrap();

    let mut file = File::open(&src).unwrap();
    let file_len = fs::metadata(&src).unwrap().len();
    let info = find_archive_info(&mut file, file_len).unwrap();
    let plans = build_plans(&mut file, &info, &make_options()).unwrap();

    assert_eq!(plans.len(), 3);
    assert!(!has_extra_field(
        &lfh_extra_at(&src, plans[0].lhf_offset),
        0xFFFF
    ));
    assert!(!has_extra_field(
        &lfh_extra_at(&src, plans[1].lhf_offset),
        0xFFFF
    ));
    assert!(has_extra_field(
        &lfh_extra_at(&src, plans[2].lhf_offset),
        0xFFFF
    ));

    for plan in &plans {
        let (lfh_name, lfh_flags) = lfh_name_and_flags_at(&src, plan.lhf_offset);
        assert_eq!(lfh_name, plan.new_fname);
        assert_ne!(lfh_flags & BIT11, 0);
        assert_ne!(read_u16(&plan.cd_header, 8) & BIT11, 0);
    }

    assert_unzip_test_accepts(&src);

    let _ = fs::remove_file(src);
}

#[test]
fn not_utf8_mode_preserves_bit11_flags() {
    let src = unique_temp_path("not-utf8-src.zip");
    let dst = unique_temp_path("not-utf8-out.zip");
    create_simple_zip(&src, "hello.txt", b"hello\n");

    let mut opts = make_options();
    opts.not_utf8 = true;

    let mut input = File::open(&src).unwrap();
    let file_len = fs::metadata(&src).unwrap().len();
    let mut output = File::create(&dst).unwrap();
    process_new(&mut input, file_len, &mut output, &opts, &mut Vec::new()).unwrap();

    assert_eq!(read_first_entry_flags(&src), read_first_entry_flags(&dst));

    let _ = fs::remove_file(src);
    let _ = fs::remove_file(dst);
}

#[test]
fn process_new_normalizes_sets_bit11_excludes_and_preserves_comment() {
    let comment = b"new archive comment";
    let kept_nfd = "e\u{301}.txt".as_bytes();
    let excluded = b"__MACOSX/ghost";

    let mut zip = Vec::new();
    let kept_offset = append_lfh(&mut zip, kept_nfd, b"");
    let excluded_offset = append_lfh(&mut zip, excluded, b"");
    let cd_offset = zip.len() as u64;
    let mut cd = Vec::new();
    cd.extend_from_slice(&make_cd_entry_raw(
        kept_nfd,
        &[],
        &[],
        0,
        0,
        0,
        kept_offset as u32,
    ));
    cd.extend_from_slice(&make_cd_entry_raw(
        excluded,
        &[],
        &[],
        0,
        0,
        0,
        excluded_offset as u32,
    ));
    zip.extend_from_slice(&cd);
    let eocd = make_eocd(
        0,
        0,
        2,
        2,
        cd.len() as u32,
        cd_offset as u32,
        comment.len() as u16,
    );
    zip.extend_from_slice(&eocd);
    zip.extend_from_slice(comment);

    let mut input = Cursor::new(zip.clone());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        zip.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();
    let out = output.into_inner();

    let dst = unique_temp_path("process-new-full.zip");
    fs::write(&dst, &out).unwrap();

    let mut file = File::open(&dst).unwrap();
    let info = find_archive_info(&mut file, out.len() as u64).unwrap();
    let plans = build_plans(&mut file, &info, &make_options()).unwrap();

    assert_eq!(info.archive_comment, comment);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].orig_fname, "é.txt".as_bytes());
    assert!(read_u16(&plans[0].cd_header, 8) & BIT11 != 0);

    assert_unzip_test_accepts(&dst);

    let _ = fs::remove_file(dst);
}

#[test]
fn preserves_archive_comment_when_rewriting() {
    let src = unique_temp_path("comment-src.zip");
    let dst = unique_temp_path("comment-out.zip");
    let comment = b"archive comment";

    create_simple_zip(&src, "hello.txt", b"hello\n");
    add_archive_comment(&src, comment);
    assert_eq!(read_archive_info(&src).archive_comment, comment);

    let mut input = File::open(&src).unwrap();
    let file_len = fs::metadata(&src).unwrap().len();
    let mut output = File::create(&dst).unwrap();
    process_new(
        &mut input,
        file_len,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    assert_eq!(read_archive_info(&dst).archive_comment, comment);

    let _ = fs::remove_file(src);
    let _ = fs::remove_file(dst);
}

#[test]
fn rejects_non_utf8_names_without_not_utf8_mode() {
    let err = normalize_for_test(b"\xff", 1).unwrap_err();
    assert!(err.contains("--not-utf-8"));
}

#[test]
fn preserves_already_nfc_names_and_sets_bit11() {
    let name = "é.txt".as_bytes(); // Already NFC (U+00E9)
    let mut zip = Vec::new();
    let off = append_lfh(&mut zip, name, b"data");
    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(name, &[], &[], 0, 4, 4, off as u32);
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_off as u32, 0));

    let mut input = Cursor::new(zip.clone());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        zip.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    let out = output.into_inner();
    let mut file = Cursor::new(out);
    let len = file.get_ref().len() as u64;
    let info = find_archive_info(&mut file, len).unwrap();
    let plans = build_plans(&mut file, &info, &make_options()).unwrap();

    assert_eq!(plans[0].new_fname, name);
    assert!(plans[0].new_bit11_set);
}

#[test]
fn process_new_leaves_ascii_names_without_bit11() {
    let name = b"ascii.txt";
    let mut zip = Vec::new();
    let off = append_lfh(&mut zip, name, b"data");
    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(name, &[], &[], 0, 4, 4, off as u32);
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_off as u32, 0));

    let mut input = Cursor::new(zip.clone());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        zip.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    let out = output.into_inner();
    let mut file = Cursor::new(out);
    let len = file.get_ref().len() as u64;
    let info = find_archive_info(&mut file, len).unwrap();
    let plans = build_plans(&mut file, &info, &make_options()).unwrap();

    assert_eq!(plans[0].new_fname, name);
    assert!(!plans[0].new_bit11_set);
}

#[test]
fn process_new_handles_empty_archive() {
    let eocd = make_eocd(0, 0, 0, 0, 0, 0, 0);
    let mut input = Cursor::new(eocd.to_vec());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        eocd.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    let out = output.into_inner();
    assert_eq!(out, eocd);
}

#[test]
fn process_new_handles_filename_collisions() {
    // Both normalize to "é.txt"
    let name1 = "é.txt".as_bytes();
    let name2 = "e\u{301}.txt".as_bytes();

    let mut zip = Vec::new();
    let off1 = append_lfh(&mut zip, name1, b"one");
    let off2 = append_lfh(&mut zip, name2, b"two");
    let cd_off = zip.len() as u64;
    let cd1 = make_cd_entry_raw(name1, &[], &[], 0, 3, 3, off1 as u32);
    let cd2 = make_cd_entry_raw(name2, &[], &[], 0, 3, 3, off2 as u32);
    zip.extend_from_slice(&cd1);
    zip.extend_from_slice(&cd2);
    zip.extend_from_slice(&make_eocd(
        0,
        0,
        2,
        2,
        (cd1.len() + cd2.len()) as u32,
        cd_off as u32,
        0,
    ));

    let mut input = Cursor::new(zip.clone());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        zip.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    let out = output.into_inner();
    let mut file = Cursor::new(out);
    let len = file.get_ref().len() as u64;
    let info = find_archive_info(&mut file, len).unwrap();
    let plans = build_plans(&mut file, &info, &make_options()).unwrap();

    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].new_fname, "é.txt".as_bytes());
    assert_eq!(plans[1].new_fname, "é.txt".as_bytes());
}

#[test]
fn process_new_preserves_data_descriptor() {
    let name = b"data_descriptor.txt";
    let payload = b"compressed-data";
    let mut zip = Vec::new();
    let off = zip.len() as u64;

    // LFH with Bit 3 (0x0008)
    let mut lhf = [0u8; 30];
    write_u32_slice(&mut lhf, 0, LOCAL_FILE_HEADER_SIG);
    write_u16(&mut lhf, 6, 0x0008); // Flags: Bit 3
    write_u16(&mut lhf, 26, name.len() as u16);
    zip.extend_from_slice(&lhf);
    zip.extend_from_slice(name);
    zip.extend_from_slice(payload);

    // Data Descriptor: Sig(4) + CRC(4) + CompSize(4) + UncompSize(4)
    let dd_sig = 0x08074b50u32;
    zip.extend_from_slice(&dd_sig.to_le_bytes());
    zip.extend_from_slice(&[0u8; 12]); // CRC, sizes

    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(
        name,
        &[],
        &[],
        0x0008,
        payload.len() as u32,
        payload.len() as u32,
        off as u32,
    );
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_off as u32, 0));

    let mut input = Cursor::new(zip.clone());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        zip.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    let out = output.into_inner();
    // The output should contain the Data Descriptor signature
    assert!(out.windows(4).any(|w| w == dd_sig.to_le_bytes()));
}

#[test]
fn preserves_directory_trailing_slash_after_normalization() {
    let name = "e\u{301}/".as_bytes();
    let expected = "é/".as_bytes();

    let mut zip = Vec::new();
    let off = append_lfh(&mut zip, name, b"");
    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(name, &[], &[], 0, 0, 0, off as u32);
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_off as u32, 0));

    let mut input = Cursor::new(zip.clone());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        zip.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    let out = output.into_inner();
    let mut file = Cursor::new(out);
    let len = file.get_ref().len() as u64;
    let info = find_archive_info(&mut file, len).unwrap();
    let plans = build_plans(&mut file, &info, &make_options()).unwrap();

    assert_eq!(plans[0].new_fname, expected);
}

#[test]
fn build_cd_entry_preserves_external_attributes() {
    let mut cd_raw = make_cd_entry_raw(b"test.txt", &[], &[], 0, 0, 0, 0);
    // External attributes at offset 38 (4 bytes)
    let attr = 0x81ED0000u32; // -rw-r--r-- in Unix
    write_u32_slice(&mut cd_raw, 38, attr);

    let plan = make_entry_plan(b"test.txt", b"test.txt", cd_raw, false, false);
    let out = build_cd_entry(&plan, 100).unwrap();

    assert_eq!(read_u32(&out, 38), attr);
}

#[test]
fn process_new_preserves_zip64_archive_comment() {
    let comment = b"zip64 archive comment";
    let mut input_cursor = Cursor::new(Vec::new());
    let mut pos = 0u64;
    // Force many entries to trigger Zip64
    write_zip64_eocd(&mut input_cursor, &mut pos, 0, 0, 0, comment).unwrap();
    let input_bytes = input_cursor.into_inner();

    let mut input = Cursor::new(input_bytes.clone());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        input_bytes.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    let out = output.into_inner();
    let mut file = Cursor::new(out);
    let len = file.get_ref().len() as u64;
    let info = find_archive_info(&mut file, len).unwrap();
    // process_new may downgrade to non-Zip64 if small, but comment must be preserved
    assert_eq!(info.archive_comment, comment);
}

#[test]
fn inplace_preserves_archive_comment() {
    let src = unique_temp_path("inplace-comment.zip");
    let comment = b"staying put";
    let mut zip = Vec::new();
    zip.extend_from_slice(&make_eocd(0, 0, 0, 0, 0, 0, comment.len() as u16));
    zip.extend_from_slice(comment);
    fs::write(&src, zip).unwrap();

    let mut output = Vec::new();
    process_file(src.to_str().unwrap(), &make_options(), &mut output).unwrap();

    let mut file = File::open(&src).unwrap();
    let len = fs::metadata(&src).unwrap().len();
    let info = find_archive_info(&mut file, len).unwrap();
    assert_eq!(info.archive_comment, comment);

    let _ = fs::remove_file(src);
}

#[test]
fn inplace_leaves_ascii_archive_untouched_if_no_shrinkage() {
    let src = unique_temp_path("inplace-no-move.zip");
    let name = b"no-change.txt";
    let payload = b"data";
    let mut zip = Vec::new();
    let off = append_lfh(&mut zip, name, payload);
    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(name, &[], &[], 0, 4, 4, off as u32);
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_off as u32, 0));
    fs::write(&src, &zip).unwrap();

    let mut output = Vec::new();
    process_file(src.to_str().unwrap(), &make_options(), &mut output).unwrap();

    let zip_after = fs::read(&src).unwrap();
    assert_eq!(zip_after, zip);
    assert_eq!(read_u16(&zip_after, off as usize + 6) & BIT11, 0);

    // The rest of the file (name and payload) should be untouched and in the same place
    assert_eq!(
        &zip_after[off as usize + 30..off as usize + 30 + name.len()],
        name
    );
    assert_eq!(
        &zip_after[off as usize + 30 + name.len()..off as usize + 30 + name.len() + payload.len()],
        payload
    );

    let _ = fs::remove_file(src);
}

#[test]
fn process_new_downgrades_zip64_when_possible() {
    let comment = b"was zip64";
    let mut zip = Vec::new();
    let off = append_lfh(&mut zip, b"test.txt", b"data");
    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(b"test.txt", &[], &[], 0, 4, 4, off as u32);
    zip.extend_from_slice(&cd);
    let cd_size = cd.len() as u64;

    let mut input_cursor = Cursor::new(zip);
    input_cursor.seek(SeekFrom::End(0)).unwrap();
    let mut pos = input_cursor.position();
    write_zip64_eocd(&mut input_cursor, &mut pos, 1, cd_size, cd_off, comment).unwrap();
    let input_bytes = input_cursor.into_inner();

    let mut input = Cursor::new(input_bytes.clone());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        input_bytes.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    let out = output.into_inner();
    let mut file = Cursor::new(out);
    let len = file.get_ref().len() as u64;
    let info = find_archive_info(&mut file, len).unwrap();

    // Should be downgraded to non-Zip64 because it's small
    assert!(!info.is_zip64);
    assert_eq!(info.archive_comment, comment);
}

#[test]
fn inplace_handles_zero_length_payload() {
    let src = unique_temp_path("zero-len.zip");
    let name = b"empty.txt";
    let mut zip = Vec::new();
    let off = append_lfh(&mut zip, name, b"");
    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(name, &[], &[], 0, 0, 0, off as u32);
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_off as u32, 0));
    fs::write(&src, &zip).unwrap();

    let mut output = Vec::new();
    process_file(src.to_str().unwrap(), &make_options(), &mut output).unwrap();

    assert_unzip_test_accepts(&src);

    let _ = fs::remove_file(src);
}

#[test]
fn inplace_writes_minimum_padding_extra_field() {
    // Need a delta of exactly 4 bytes to trigger minimum padding
    // "e\u{301}" (3 bytes) -> "é" (2 bytes) saves 1 byte.
    // So 4 such characters will save 4 bytes.
    let nfd_name = "e\u{301}e\u{301}e\u{301}e\u{301}.txt".as_bytes();
    let nfc_name = "éééé.txt".as_bytes();
    assert_eq!(nfd_name.len() - nfc_name.len(), 4);

    let src = unique_temp_path("min-padding.zip");
    let mut zip = Vec::new();
    let off = append_lfh(&mut zip, nfd_name, b"data");
    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(nfd_name, &[], &[], 0, 4, 4, off as u32);
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_off as u32, 0));
    fs::write(&src, &zip).unwrap();

    let mut output = Vec::new();
    process_file(src.to_str().unwrap(), &make_options(), &mut output).unwrap();

    let extra = first_lfh_extra(&src);
    // Should have a padding extra field (ID 0xFFFF)
    assert!(has_extra_field(&extra, 0xFFFF));

    // Check total size of extra field is 4 bytes (ID(2) + Size(2) + Data(0))
    let mut cursor = 0;
    while cursor + 4 <= extra.len() {
        let id = read_u16(&extra, cursor);
        let sz = read_u16(&extra, cursor + 2) as usize;
        if id == 0xFFFF {
            assert_eq!(sz, 0);
            break;
        }
        cursor += 4 + sz;
    }

    let _ = fs::remove_file(src);
}

#[test]
fn parse_eocd_rejects_entry_count_mismatch() {
    let eocd = make_eocd(0, 0, 1, 2, 0, 0, 0);
    let mut cursor = Cursor::new(eocd.to_vec());
    let err = parse_eocd(&mut cursor, &eocd, 0, 22).unwrap_err();
    assert!(err.contains("entry count mismatch"));
}

#[test]
fn parse_eocd_rejects_cd_beyond_file_len() {
    let eocd = make_eocd(0, 0, 1, 1, 10, 20, 0); // CD at 20..30, file len 22
    let mut cursor = Cursor::new(eocd.to_vec());
    let err = parse_eocd(&mut cursor, &eocd, 0, 22).unwrap_err();
    assert!(err.contains("exceeds file length"));
}

#[test]
fn write_eocd_rejects_long_comment() {
    let comment = vec![0u8; 65536];
    let mut out = Cursor::new(Vec::new());
    let err = write_eocd(&mut out, 0, 0, 0, &comment).unwrap_err();
    assert!(err.contains("comment is too long"));
}

#[test]
fn patch_zip64_lhf_offset_in_extra_rejects_missing_field() {
    let extra = make_extra_field(0xCAFE, b"data");
    let cd_raw = make_cd_entry_raw(b"test", &extra, b"", 0, 0xFFFF_FFFF, 0, 0xFFFF_FFFF);
    let plan = make_entry_plan(b"test", b"test", cd_raw, true, false);

    let err = build_cd_entry(&plan, 0x1000).unwrap_err();
    assert!(err.contains("ZIP64 extra field not found"));
}

#[test]
fn copy_within_file_rejects_backward_copy() {
    let path = unique_temp_path("backward.bin");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .unwrap();
    let err = copy_within_file(&mut file, 100, 200, 10).unwrap_err();
    assert!(err.contains("invariant violated"));
    let _ = fs::remove_file(path);
}

#[test]
fn stream_copy_rejects_unexpected_eof() {
    let mut src = Cursor::new(vec![0u8; 10]);
    let mut dst = Cursor::new(Vec::new());
    let err = stream_copy(&mut src, &mut dst, 20).unwrap_err();
    assert!(err.contains("unexpected EOF"));
}

#[test]
fn parse_eocd_rejects_zip64_if_locator_at_too_low_offset() {
    let eocd = make_eocd(0, 0, 0xFFFF, 0xFFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, 0);
    let mut cursor = Cursor::new(eocd.to_vec());
    let err = parse_eocd(&mut cursor, &eocd, 19, 19 + 22).unwrap_err();
    assert_eq!(err, "ZIP64 EOCD locator not found before EOCD");
}

#[test]
fn parse_eocd_rejects_multi_disk_zip64_eocd_record() {
    let mut cursor = Cursor::new(Vec::new());
    let mut pos = 0u64;
    write_zip64_eocd(&mut cursor, &mut pos, 1, 0, 0, b"").unwrap();
    let mut bytes = cursor.into_inner();
    write_u32_slice(&mut bytes, 16, 1); // z64_disk = 1

    let eocd_offset = 56 + 20;
    let mut eocd = [0u8; 22];
    eocd.copy_from_slice(&bytes[eocd_offset..eocd_offset + 22]);

    let mut reader = Cursor::new(bytes.clone());
    let err = parse_eocd(&mut reader, &eocd, eocd_offset as u64, bytes.len() as u64).unwrap_err();
    assert_eq!(err, "multi-disk ZIP64 archives are not supported");
}

#[test]
fn patched_header_rejects_large_filename() {
    let mut header = [0u8; 30];
    write_u32_slice(&mut header, 0, LOCAL_FILE_HEADER_SIG);
    let lhf = LocalHeader {
        header,
        extra: vec![],
    };
    let err = lhf.patched_header(0, 65536, 0).unwrap_err();
    assert!(err.contains("LFH filename length exceeds ZIP limit"));
}

#[test]
fn parse_eocd_rejects_zip64_if_locator_signature_is_wrong() {
    let eocd = make_eocd(0, 0, 0xFFFF, 0xFFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, 0);
    let mut bytes = vec![0u8; 20];
    bytes.extend_from_slice(&eocd);
    // Locator signature is at bytes[0..4]. Let's make it wrong.
    bytes[0] = 0;

    let mut cursor = Cursor::new(bytes);
    let err = parse_eocd(&mut cursor, &eocd, 20, 42).unwrap_err();
    assert_eq!(err, "ZIP64 EOCD locator signature not found");
}

fn append_lfh_with_extra(zip: &mut Vec<u8>, name: &[u8], payload: &[u8], extra: &[u8]) -> u64 {
    let offset = zip.len() as u64;
    let mut header = [0u8; 30];
    write_u32_slice(&mut header, 0, LOCAL_FILE_HEADER_SIG);
    write_u32_slice(&mut header, 18, payload.len() as u32);
    write_u32_slice(&mut header, 22, payload.len() as u32);
    write_u16(&mut header, 26, name.len() as u16);
    write_u16(&mut header, 28, extra.len() as u16);
    zip.extend_from_slice(&header);
    zip.extend_from_slice(name);
    zip.extend_from_slice(extra);
    zip.extend_from_slice(payload);
    offset
}

fn lfh_extra_at_cursor<R: Read + Seek>(r: &mut R, offset: u64) -> Vec<u8> {
    r.seek(SeekFrom::Start(offset)).unwrap();
    let mut lhf = [0u8; 30];
    r.read_exact(&mut lhf).unwrap();
    assert_eq!(read_u32(&lhf, 0), LOCAL_FILE_HEADER_SIG);

    let fname_len = read_u16(&lhf, 26) as u64;
    let extra_len = read_u16(&lhf, 28) as usize;
    r.seek(SeekFrom::Current(fname_len as i64)).unwrap();

    let mut extra = vec![0u8; extra_len];
    r.read_exact(&mut extra).unwrap();
    extra
}

#[test]
fn inplace_preserves_arbitrary_extra_fields() {
    let src = unique_temp_path("extra-fields.zip");
    let name = "e\u{301}.txt".as_bytes(); // NFC will be shorter
    let custom_id = 0xCAFEu16;
    let custom_data = b"metadata";
    let extra = make_extra_field(custom_id, custom_data);

    let mut zip = Vec::new();
    let off = append_lfh_with_extra(&mut zip, name, b"payload", &extra);
    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(name, &extra, b"", 0, 7, 7, off as u32);
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_off as u32, 0));
    fs::write(&src, &zip).unwrap();

    let mut output = Vec::new();
    process_file(src.to_str().unwrap(), &make_options(), &mut output).unwrap();

    let extra_after = first_lfh_extra(&src);
    assert!(has_extra_field(&extra_after, custom_id));
    // Verify custom data is there
    assert!(extra_after
        .windows(custom_data.len())
        .any(|w| w == custom_data));

    let _ = fs::remove_file(src);
}

#[test]
fn process_new_preserves_arbitrary_extra_fields() {
    let name = "e\u{301}.txt".as_bytes();
    let custom_id = 0xBEEFu16;
    let custom_data = b"more-metadata";
    let extra = make_extra_field(custom_id, custom_data);

    let mut zip = Vec::new();
    let off = append_lfh_with_extra(&mut zip, name, b"payload", &extra);
    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(name, &extra, b"", 0, 7, 7, off as u32);
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_off as u32, 0));

    let mut input = Cursor::new(zip.clone());
    let mut output = Cursor::new(Vec::new());
    process_new(
        &mut input,
        zip.len() as u64,
        &mut output,
        &make_options(),
        &mut Vec::new(),
    )
    .unwrap();

    let out = output.into_inner();
    let mut file = Cursor::new(out);
    let len = file.get_ref().len() as u64;
    let info = find_archive_info(&mut file, len).unwrap();
    let plans = build_plans(&mut file, &info, &make_options()).unwrap();

    let extra_after = lfh_extra_at_cursor(&mut file, plans[0].lhf_offset);
    assert!(has_extra_field(&extra_after, custom_id));
    assert!(extra_after
        .windows(custom_data.len())
        .any(|w| w == custom_data));
}

#[test]
fn inplace_shifts_data_descriptor_correctly() {
    let src = unique_temp_path("inplace-dd.zip");
    let name = "e\u{301}.txt".as_bytes(); // Shorter after NFC
    let payload = b"compressed-data";
    let dd_sig = 0x08074b50u32;
    let mut zip = Vec::new();
    let off = zip.len() as u64;

    // LFH with Bit 3
    let mut lhf = [0u8; 30];
    write_u32_slice(&mut lhf, 0, LOCAL_FILE_HEADER_SIG);
    write_u16(&mut lhf, 6, 0x0008);
    write_u16(&mut lhf, 26, name.len() as u16);
    zip.extend_from_slice(&lhf);
    zip.extend_from_slice(name);
    zip.extend_from_slice(payload);

    // Data Descriptor
    zip.extend_from_slice(&dd_sig.to_le_bytes());
    zip.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]); // CRC
    zip.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // Comp
    zip.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // Uncomp

    let cd_off = zip.len() as u64;
    let cd = make_cd_entry_raw(
        name,
        &[],
        b"",
        0x0008,
        payload.len() as u32,
        payload.len() as u32,
        off as u32,
    );
    zip.extend_from_slice(&cd);
    zip.extend_from_slice(&make_eocd(0, 0, 1, 1, cd.len() as u32, cd_off as u32, 0));
    fs::write(&src, &zip).unwrap();

    process_file(src.to_str().unwrap(), &make_options(), &mut Vec::new()).unwrap();

    let zip_after = fs::read(&src).unwrap();
    // Bit 11 should be set, so flags become 0x0800 | 0x0008 = 0x0808
    assert_eq!(read_u16(&zip_after, off as usize + 6), 0x0808);

    // Original pos of DD: 30 + 7 (NFD name) + 15 (payload) = 52
    // New pos of DD: 30 + 6 (NFC name) + 15 (payload) = 51
    assert_eq!(read_u32(&zip_after, off as usize + 51), dd_sig);
    let _ = fs::remove_file(src);
}

#[test]
#[cfg(unix)]
fn preserves_symbolic_links() {
    let root = unique_temp_path("symlink-tree");
    fs::create_dir_all(&root).unwrap();
    let target = "target.txt";
    fs::write(root.join(target), b"content").unwrap();
    std::os::unix::fs::symlink(target, root.join("link.txt")).unwrap();

    let src = unique_temp_path("symlink.zip");
    let status = Command::new("zip")
        .arg("-q")
        .arg("-y") // preserve symlinks
        .arg(&src)
        .arg("link.txt")
        .arg("target.txt")
        .current_dir(&root)
        .status()
        .expect("failed to execute zip command");
    assert!(status.success());

    process_file(src.to_str().unwrap(), &make_options(), &mut Vec::new()).unwrap();

    let mut file = File::open(&src).unwrap();
    let len = fs::metadata(&src).unwrap().len();
    let info = find_archive_info(&mut file, len).unwrap();
    let plans = build_plans(&mut file, &info, &make_options()).unwrap();

    let link_plan = plans
        .iter()
        .find(|p| p.orig_fname == b"link.txt")
        .expect("link.txt not found");
    let external_attr = read_u32(&link_plan.cd_header, 38);
    // Unix symlink bit is 0xA000 in the upper 16 bits of external attributes
    assert_eq!(
        external_attr >> 28,
        0xA,
        "External attributes should indicate a symlink"
    );

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(src);
}

#[test]
fn inplace_fails_on_readonly_file() {
    let src = unique_temp_path("readonly.zip");
    create_simple_zip(&src, "test.txt", b"data");

    let mut perms = fs::metadata(&src).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&src, perms).unwrap();

    let res = process_file(src.to_str().unwrap(), &make_options(), &mut Vec::new());
    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    assert!(err_msg.contains("cannot open") || err_msg.contains("Permission denied"));

    // Reset permissions to allow cleanup
    let mut perms = fs::metadata(&src).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o644);
    }
    #[cfg(not(unix))]
    perms.set_readonly(false);
    fs::set_permissions(&src, perms).unwrap();
    let _ = fs::remove_file(src);
}
