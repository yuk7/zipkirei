use std::io::{Read, Seek, SeekFrom};

use unicode_normalization::UnicodeNormalization;

use super::bytes::{read_u16, read_u32, read_u64};
use super::eocd::ArchiveInfo;
use super::options::Options;
use super::{io_err, BIT11, CENTRAL_DIR_SIG, LOCAL_FILE_HEADER_SIG, ZIP64_EXTRA_FIELD_ID};

#[derive(Debug)]
pub(crate) struct EntryPlan {
    pub(crate) cd_index: usize,
    pub(crate) lhf_offset: u64,
    pub(crate) excluded: bool,
    pub(crate) orig_fname: Vec<u8>,
    pub(crate) new_fname: Vec<u8>,
    pub(crate) needs_bit11: bool,
    pub(crate) new_bit11_set: bool,
    pub(crate) span_size: u64,
    pub(crate) lhf_header_size: u64,
    pub(crate) lhf_extra_len: u16,
    pub(crate) lhf_offset_in_zip64_extra: bool,
    pub(crate) cd_header: [u8; 46],
    pub(crate) cd_extra: Vec<u8>,
    pub(crate) cd_comment: Vec<u8>,
}

struct CdEntryPlan {
    cd_index: usize,
    lhf_offset: u64,
    excluded: bool,
    orig_fname: Vec<u8>,
    new_fname: Vec<u8>,
    needs_bit11: bool,
    new_bit11_set: bool,
    lhf_offset_in_zip64_extra: bool,
    cd_header: [u8; 46],
    cd_extra: Vec<u8>,
    cd_comment: Vec<u8>,
}

struct LfhHeaderInfo {
    fname: Vec<u8>,
    extra_len: u16,
}

impl EntryPlan {
    pub(crate) fn fname_delta(&self) -> u64 {
        self.orig_fname.len() as u64 - self.new_fname.len() as u64
    }

    pub(crate) fn payload_size(&self) -> u64 {
        self.span_size - self.lhf_header_size
    }
}

pub(crate) fn cd_order(plans: &[EntryPlan]) -> Result<Vec<usize>, String> {
    let mut order = vec![usize::MAX; plans.len()];
    for (physical_index, p) in plans.iter().enumerate() {
        if p.cd_index >= plans.len() {
            return Err(format!("CD entry index {} is out of range", p.cd_index + 1));
        }
        if order[p.cd_index] != usize::MAX {
            return Err(format!("duplicate CD entry index {}", p.cd_index + 1));
        }
        order[p.cd_index] = physical_index;
    }
    if let Some(missing) = order.iter().position(|&i| i == usize::MAX) {
        return Err(format!("missing CD entry index {}", missing + 1));
    }
    Ok(order)
}

pub(crate) fn build_plans<R: Read + Seek>(
    r: &mut R,
    info: &ArchiveInfo,
    opts: &Options,
) -> Result<Vec<EntryPlan>, String> {
    r.seek(SeekFrom::Start(info.cd_offset)).map_err(io_err)?;
    let entry_count = usize::try_from(info.total_entries)
        .map_err(|_| format!("too many Central Directory entries: {}", info.total_entries))?;
    let mut cd_entries = Vec::with_capacity(entry_count);
    let mut cd_consumed = 0u64;

    for i in 0..info.total_entries {
        cd_entries.push(read_cd_entry_plan(
            r,
            &mut cd_consumed,
            info.cd_size,
            i,
            opts,
        )?);
    }
    if cd_consumed != info.cd_size {
        return Err("Central Directory has trailing bytes after expected entries".to_string());
    }

    cd_entries.sort_by_key(|p| p.lhf_offset);

    let mut plans = Vec::with_capacity(cd_entries.len());
    for cd_entry in cd_entries {
        let lfh = read_lhf_header(r, cd_entry.lhf_offset, cd_entry.cd_index as u64 + 1)?;
        if lfh.fname.len() != cd_entry.orig_fname.len() {
            return Err(format!(
                "LFH filename length mismatch at entry {}: LFH has {}, CD has {}",
                cd_entry.cd_index + 1,
                lfh.fname.len(),
                cd_entry.orig_fname.len()
            ));
        }
        if lfh.fname != cd_entry.orig_fname {
            return Err(format!(
                "LFH filename bytes mismatch at entry {}",
                cd_entry.cd_index + 1
            ));
        }
        let lhf_header_size = 30 + lfh.fname.len() as u64 + lfh.extra_len as u64;
        plans.push(EntryPlan {
            cd_index: cd_entry.cd_index,
            lhf_offset: cd_entry.lhf_offset,
            excluded: cd_entry.excluded,
            orig_fname: cd_entry.orig_fname,
            new_fname: cd_entry.new_fname,
            needs_bit11: cd_entry.needs_bit11,
            new_bit11_set: cd_entry.new_bit11_set,
            span_size: 0,
            lhf_header_size,
            lhf_extra_len: lfh.extra_len,
            lhf_offset_in_zip64_extra: cd_entry.lhf_offset_in_zip64_extra,
            cd_header: cd_entry.cd_header,
            cd_extra: cd_entry.cd_extra,
            cd_comment: cd_entry.cd_comment,
        });
    }

    for i in 0..plans.len() {
        let end = if i + 1 < plans.len() {
            plans[i + 1].lhf_offset
        } else {
            info.cd_offset
        };
        if end < plans[i].lhf_offset + plans[i].lhf_header_size {
            return Err(format!(
                "entry {} has an invalid physical span",
                plans[i].cd_index + 1
            ));
        }
        plans[i].span_size = end - plans[i].lhf_offset;
    }

    Ok(plans)
}

fn read_cd_entry_plan(
    r: &mut impl Read,
    cd_consumed: &mut u64,
    cd_size: u64,
    cd_index: u64,
    opts: &Options,
) -> Result<CdEntryPlan, String> {
    let entry_no = cd_index + 1;
    let mut hdr = [0u8; 46];
    read_cd_exact(
        r,
        cd_consumed,
        cd_size,
        &mut hdr,
        "CD entry header",
        entry_no,
    )?;
    if read_u32(&hdr, 0) != CENTRAL_DIR_SIG {
        return Err(format!("invalid CD signature at entry {}", entry_no));
    }

    let flags = read_u16(&hdr, 8);
    let fname_len = read_u16(&hdr, 28) as usize;
    let extra_len = read_u16(&hdr, 30) as usize;
    let comment_len = read_u16(&hdr, 32) as usize;
    let compressed_size_32 = read_u32(&hdr, 20);
    let uncompressed_size_32 = read_u32(&hdr, 24);
    let lhf_offset_32 = read_u32(&hdr, 42);

    let mut fname_buf = vec![0u8; fname_len];
    read_cd_exact(
        r,
        cd_consumed,
        cd_size,
        &mut fname_buf,
        "filename",
        entry_no,
    )?;
    let mut extra_buf = vec![0u8; extra_len];
    read_cd_exact(r, cd_consumed, cd_size, &mut extra_buf, "extra", entry_no)?;
    let mut comment_buf = vec![0u8; comment_len];
    read_cd_exact(
        r,
        cd_consumed,
        cd_size,
        &mut comment_buf,
        "comment",
        entry_no,
    )?;

    let (lhf_offset, lhf_offset_in_zip64) = resolve_lfh_offset(
        compressed_size_32,
        uncompressed_size_32,
        lhf_offset_32,
        &extra_buf,
        entry_no,
    )?;

    let excluded = opts.is_excluded(&fname_buf);
    let (new_fname, new_bit11_set) = if opts.not_utf8 || excluded {
        (fname_buf.clone(), (flags & BIT11) != 0)
    } else {
        (nfc_normalize(&fname_buf, entry_no)?, true)
    };

    Ok(CdEntryPlan {
        cd_index: cd_index as usize,
        lhf_offset,
        excluded,
        orig_fname: fname_buf,
        new_fname,
        needs_bit11: (flags & BIT11) == 0 && new_bit11_set,
        new_bit11_set,
        lhf_offset_in_zip64_extra: lhf_offset_in_zip64,
        cd_header: hdr,
        cd_extra: extra_buf,
        cd_comment: comment_buf,
    })
}

fn read_cd_exact(
    r: &mut impl Read,
    cd_consumed: &mut u64,
    cd_size: u64,
    out: &mut [u8],
    part: &str,
    entry_no: u64,
) -> Result<(), String> {
    let end = cd_consumed
        .checked_add(out.len() as u64)
        .ok_or_else(|| format!("Central Directory {part} length overflow at entry {entry_no}"))?;
    if end > cd_size {
        return Err(format!(
            "unexpected EOF reading {part} at CD entry {}",
            entry_no
        ));
    }
    r.read_exact(out)
        .map_err(|_| format!("unexpected EOF reading {part} at CD entry {}", entry_no))?;
    *cd_consumed = end;
    Ok(())
}

fn nfc_normalize(raw: &[u8], entry_no: u64) -> Result<Vec<u8>, String> {
    let s = std::str::from_utf8(raw).map_err(|_| {
        format!(
            "entry {} has a non-UTF-8 filename; rerun with --not-utf-8 to leave filename bytes and bit 11 unchanged",
            entry_no
        )
    })?;
    let nfc: String = s.nfc().collect();
    let new_bytes = nfc.into_bytes();
    if new_bytes.len() > raw.len() {
        return Err(format!(
            "NFC normalization increased filename size for entry {} (unexpected); aborting",
            entry_no
        ));
    }
    Ok(new_bytes)
}

pub(crate) fn resolve_lfh_offset(
    comp32: u32,
    uncomp32: u32,
    lhf32: u32,
    extra: &[u8],
    entry_no: u64,
) -> Result<(u64, bool), String> {
    if lhf32 != 0xFFFF_FFFF {
        return Ok((lhf32 as u64, false));
    }

    let mut cursor = 0usize;
    while cursor + 4 <= extra.len() {
        let id = read_u16(extra, cursor);
        let sz = read_u16(extra, cursor + 2) as usize;
        cursor += 4;
        if cursor + sz > extra.len() {
            return Err(format!("truncated extra field in CD entry {}", entry_no));
        }
        if id == ZIP64_EXTRA_FIELD_ID {
            let field = &extra[cursor..cursor + sz];
            let mut off = 0;
            if uncomp32 == 0xFFFF_FFFF {
                off += 8;
            }
            if comp32 == 0xFFFF_FFFF {
                off += 8;
            }
            if off + 8 > sz {
                return Err(format!("ZIP64 extra too short in CD entry {}", entry_no));
            }
            return Ok((read_u64(field, off), true));
        }
        cursor += sz;
    }

    Err(format!(
        "ZIP64 extra field missing for CD entry {}",
        entry_no
    ))
}

fn read_lhf_header<R: Read + Seek>(
    r: &mut R,
    lhf_offset: u64,
    entry_no: u64,
) -> Result<LfhHeaderInfo, String> {
    r.seek(SeekFrom::Start(lhf_offset)).map_err(io_err)?;
    let mut hdr = [0u8; 30];
    r.read_exact(&mut hdr)
        .map_err(|_| format!("unexpected EOF reading LFH at entry {}", entry_no))?;
    if read_u32(&hdr, 0) != LOCAL_FILE_HEADER_SIG {
        return Err(format!(
            "invalid LFH signature at entry {} (offset {:#x})",
            entry_no, lhf_offset
        ));
    }
    let fname_len = read_u16(&hdr, 26) as usize;
    let extra_len = read_u16(&hdr, 28);
    let mut fname = vec![0u8; fname_len];
    r.read_exact(&mut fname)
        .map_err(|_| format!("unexpected EOF reading LFH filename at entry {}", entry_no))?;
    r.seek(SeekFrom::Current(extra_len as i64))
        .map_err(io_err)?;

    Ok(LfhHeaderInfo { fname, extra_len })
}

#[cfg(test)]
pub(crate) fn normalize_for_test(raw: &[u8], entry_no: u64) -> Result<Vec<u8>, String> {
    nfc_normalize(raw, entry_no)
}
