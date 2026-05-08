use std::io::{Seek, SeekFrom, Write};

#[cfg(not(unix))]
use std::io::Read;

use super::bytes::{read_u16, write_u16};
use super::cd_entry::build_cd_entry_into;
use super::copy::copy_within_file_with_buf;
#[cfg(unix)]
use super::copy::{read_exact_at, write_all_at};
use super::eocd::{build_eocd_into, build_zip64_eocd_into, find_archive_info, ArchiveInfo};
use super::local_header::LocalHeader;
use super::options::Options;
use super::plan::{build_plans, cd_order, EntryPlan};
use super::{
    checked_u16, dry_run_report, io_err, with_bit11, Error, ZipResult, MIN_PADDING,
    PADDING_EXTRA_FIELD_ID,
};

const COALESCE_PADDING_LIMIT: u64 = 64 * 1024;

/// In-place processing with full read+write access.
pub fn process_file(path: &str, opts: &Options, stdout: &mut impl Write) -> ZipResult<()> {
    use std::fs::OpenOptions;

    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("cannot open '{}': {}", path, e))?;

    let file_len = f.seek(SeekFrom::End(0)).map_err(io_err)?;

    let info = find_archive_info(&mut f, file_len)?;
    let plans = build_plans(&mut f, &info, opts)?;

    if opts.dry_run {
        return dry_run_report(&plans, stdout).map_err(Error::from);
    }

    if plans
        .iter()
        .all(|p| !p.excluded && p.fname_delta() == 0 && !p.needs_bit11)
    {
        return Ok(());
    }

    inplace_patch(&mut f, &info, &plans)?;

    Ok(())
}

fn inplace_patch(
    f: &mut std::fs::File,
    info: &ArchiveInfo,
    plans: &[EntryPlan],
) -> Result<(), String> {
    let mut carry: u64 = 0;
    let mut write_pos: u64 = 0;
    let mut first = true;
    let mut new_lhf_offsets: Vec<Option<u64>> = vec![None; plans.len()];
    let mut copy_buf = vec![0u8; super::COPY_BUF_SIZE];
    let mut header_buf = Vec::new();

    for (i, p) in plans.iter().enumerate() {
        if p.excluded {
            if carry == 0 {
                write_pos = p.lhf_offset + p.span_size;
            } else {
                let new_lhf_off = p.lhf_offset - carry;
                copy_within_file_with_buf(
                    f,
                    p.lhf_offset,
                    new_lhf_off,
                    p.span_size,
                    &mut copy_buf,
                )?;
                write_pos = new_lhf_off + p.span_size;
                carry = 0;
            }
            first = false;
            new_lhf_offsets[i] = None;
            continue;
        }

        if first {
            write_pos = p.lhf_offset.saturating_sub(carry);
            first = false;
        }

        let delta = p.fname_delta();
        let absorb = delta + carry;

        let absorb_with_padding = if absorb >= MIN_PADDING {
            Some(p.lhf_extra_len as u64 + absorb <= u16::MAX as u64)
        } else {
            None
        };

        if let Some(true) = absorb_with_padding {
            let new_lhf_off = write_pos;
            new_lhf_offsets[i] = Some(new_lhf_off);

            let lhf = read_local_header(f, p.lhf_offset, p.cd_index + 1)?;
            let new_flags = with_bit11(read_u16(&lhf.header, 6), p.new_bit11_set);
            let new_extra_len = checked_u16(
                p.lhf_extra_len as u64 + absorb,
                "LFH extra field length exceeds ZIP limit",
            )?;

            let header = lhf.patched_header(new_flags, p.new_fname.len(), new_extra_len)?;
            header_buf.clear();
            header_buf.reserve(30 + p.new_fname.len() + 4);
            header_buf.extend_from_slice(&header);
            header_buf.extend_from_slice(&p.new_fname);
            append_padding_extra_header(&mut header_buf, absorb)?;
            let mut header_pos = new_lhf_off;
            let padding_data_len = absorb - MIN_PADDING;

            if padding_data_len <= COALESCE_PADDING_LIMIT {
                header_buf.resize(header_buf.len() + padding_data_len as usize, 0);
                header_buf.extend_from_slice(&lhf.extra);
                write_all_at_portable(f, &header_buf, header_pos)?;
            } else {
                write_all_at_portable(f, &header_buf, header_pos)?;
                header_pos += header_buf.len() as u64;

                // Write a proper, independent Padding Extra Field (0xFFFF).
                header_pos = write_padding_extra_data_at(f, header_pos, absorb, &mut copy_buf)?;
                // Keep original extra fields intact and untouched.
                write_all_at_portable(f, &lhf.extra, header_pos)?;
            }

            let data_src = p.lhf_offset + p.lhf_header_size;
            let new_header_size = 30 + p.new_fname.len() as u64 + new_extra_len as u64;
            let data_dst = new_lhf_off + new_header_size;

            if data_src != data_dst {
                copy_within_file_with_buf(f, data_src, data_dst, p.payload_size(), &mut copy_buf)?;
            }

            write_pos = data_dst + p.payload_size();
            carry = 0;
        } else if carry == 0 && delta == 0 {
            new_lhf_offsets[i] = Some(p.lhf_offset);

            let mut flags_buf = [0u8; 2];
            read_exact_at_portable(f, &mut flags_buf, p.lhf_offset + 6)?;
            let new_flags = with_bit11(read_u16(&flags_buf, 0), p.new_bit11_set);
            write_all_at_portable(f, &new_flags.to_le_bytes(), p.lhf_offset + 6)?;

            write_pos = p.lhf_offset + p.span_size;
        } else {
            let new_lhf_off = p.lhf_offset - carry;
            new_lhf_offsets[i] = Some(new_lhf_off);

            let lhf = read_local_header(f, p.lhf_offset, p.cd_index + 1)?;
            let new_flags = with_bit11(read_u16(&lhf.header, 6), p.new_bit11_set);

            let extra_len = checked_u16(
                lhf.extra_len() as u64,
                "LFH extra field length exceeds ZIP limit",
            )?;
            header_buf.clear();
            header_buf.reserve(30 + p.new_fname.len() + lhf.extra_len());
            lhf.write(&mut header_buf, &p.new_fname, new_flags, extra_len)?;
            write_all_at_portable(f, &header_buf, new_lhf_off)?;

            let data_src = p.lhf_offset + p.lhf_header_size;
            let new_header_size = 30 + p.new_fname.len() as u64 + lhf.extra_len() as u64;
            let data_dst = new_lhf_off + new_header_size;
            copy_within_file_with_buf(f, data_src, data_dst, p.payload_size(), &mut copy_buf)?;

            write_pos = data_dst + p.payload_size();
            carry += delta;
        }
    }

    let new_cd_start = write_pos;

    let mut cd_buf = Vec::with_capacity(super::COPY_BUF_SIZE.min(1024 * 1024));
    let mut cd_write_pos = new_cd_start;
    let mut cd_entries_written: u64 = 0;

    let cd_order = cd_order(plans)?;

    for i in cd_order {
        let p = &plans[i];
        if p.excluded {
            continue;
        }
        let new_lhf = new_lhf_offsets[i]
            .ok_or_else(|| format!("missing LFH offset for CD entry {}", p.cd_index + 1))?;
        let before_len = cd_buf.len();
        build_cd_entry_into(p, new_lhf, &mut cd_buf)?;
        if cd_buf.len() > super::COPY_BUF_SIZE {
            if before_len == 0 {
                flush_positioned(&mut cd_buf, f, &mut cd_write_pos)?;
            } else {
                let cd_bytes = cd_buf.split_off(before_len);
                flush_positioned(&mut cd_buf, f, &mut cd_write_pos)?;
                append_positioned(&mut cd_buf, f, &cd_bytes, &mut cd_write_pos)?;
            }
        }
        cd_entries_written += 1;
    }

    flush_positioned(&mut cd_buf, f, &mut cd_write_pos)?;
    let cd_size = cd_write_pos - new_cd_start;
    let needs_zip64 = info.is_zip64
        || cd_entries_written > 0xFFFF
        || cd_size > 0xFFFF_FFFF
        || new_cd_start > 0xFFFF_FFFF;

    if needs_zip64 {
        let mut eocd = Vec::with_capacity(56 + 20 + 22 + info.archive_comment.len());
        build_zip64_eocd_into(
            &mut eocd,
            new_cd_start + cd_size,
            cd_entries_written,
            cd_size,
            new_cd_start,
            &info.archive_comment,
        )?;
        write_all_at_portable(f, &eocd, new_cd_start + cd_size)?;
        cd_write_pos += eocd.len() as u64;
    } else {
        let mut eocd = Vec::with_capacity(22 + info.archive_comment.len());
        build_eocd_into(
            &mut eocd,
            checked_u16(
                cd_entries_written,
                "central directory entry count exceeds ZIP limit",
            )?,
            cd_size as u32,
            new_cd_start as u32,
            &info.archive_comment,
        )?;
        cd_write_pos += eocd.len() as u64;
        write_all_at_portable(f, &eocd, new_cd_start + cd_size)?;
    }

    f.set_len(cd_write_pos)
        .map_err(|e| format!("truncate failed: {}", e))?;

    Ok(())
}

fn read_local_header(
    f: &mut std::fs::File,
    offset: u64,
    entry_no: usize,
) -> Result<LocalHeader, String> {
    #[cfg(unix)]
    {
        LocalHeader::read_from_file(f, offset, entry_no)
    }

    #[cfg(not(unix))]
    {
        LocalHeader::read(f, offset, entry_no)
    }
}

fn read_exact_at_portable(
    f: &mut std::fs::File,
    buf: &mut [u8],
    offset: u64,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        read_exact_at(f, buf, offset)
    }

    #[cfg(not(unix))]
    {
        f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
        f.read_exact(buf).map_err(io_err)
    }
}

fn write_all_at_portable(f: &mut std::fs::File, buf: &[u8], offset: u64) -> Result<(), String> {
    #[cfg(unix)]
    {
        write_all_at(f, buf, offset)
    }

    #[cfg(not(unix))]
    {
        f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
        f.write_all(buf).map_err(io_err)
    }
}

fn append_padding_extra_header(out: &mut Vec<u8>, absorb: u64) -> Result<(), String> {
    debug_assert!(absorb >= MIN_PADDING);
    let data_len = absorb - 4;
    let data_len_u16 = checked_u16(data_len, "padding extra field exceeds ZIP limit")?;
    let mut hdr = [0u8; 4];
    write_u16(&mut hdr, 0, PADDING_EXTRA_FIELD_ID);
    write_u16(&mut hdr, 2, data_len_u16);
    out.extend_from_slice(&hdr);
    Ok(())
}

fn write_padding_extra_data_at(
    f: &mut std::fs::File,
    mut pos: u64,
    absorb: u64,
    zero_buf: &mut [u8],
) -> Result<u64, String> {
    debug_assert!(absorb >= MIN_PADDING);
    let data_len = absorb - 4;

    if zero_buf.is_empty() {
        return Err("padding write buffer is empty".into());
    }
    zero_buf.fill(0);
    let mut remaining = data_len;
    while remaining > 0 {
        let to_write = remaining.min(zero_buf.len() as u64) as usize;
        write_all_at_portable(f, &zero_buf[..to_write], pos)?;
        pos += to_write as u64;
        remaining -= to_write as u64;
    }
    Ok(pos)
}

fn append_positioned(
    buf: &mut Vec<u8>,
    f: &mut std::fs::File,
    bytes: &[u8],
    write_pos: &mut u64,
) -> Result<(), String> {
    if bytes.len() > super::COPY_BUF_SIZE {
        flush_positioned(buf, f, write_pos)?;
        write_all_at_portable(f, bytes, *write_pos)?;
        *write_pos += bytes.len() as u64;
        return Ok(());
    }

    if buf.len() + bytes.len() > super::COPY_BUF_SIZE {
        flush_positioned(buf, f, write_pos)?;
    }
    buf.extend_from_slice(bytes);
    Ok(())
}

fn flush_positioned(
    buf: &mut Vec<u8>,
    f: &mut std::fs::File,
    write_pos: &mut u64,
) -> Result<(), String> {
    if buf.is_empty() {
        return Ok(());
    }
    write_all_at_portable(f, buf, *write_pos)?;
    *write_pos += buf.len() as u64;
    buf.clear();
    Ok(())
}
