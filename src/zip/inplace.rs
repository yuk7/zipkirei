use std::io::{Read, Seek, SeekFrom, Write};

use super::bytes::{read_u16, write_u16};
use super::cd_entry::build_cd_entry;
use super::copy::copy_within_file_with_buf;
use super::eocd::{find_archive_info, write_eocd, write_zip64_eocd, ArchiveInfo};
use super::local_header::LocalHeader;
use super::options::Options;
use super::plan::{build_plans, EntryPlan};
use super::{
    checked_u16, dry_run_report, io_err, with_bit11, Error, ZipResult, MIN_PADDING,
    PADDING_EXTRA_FIELD_ID,
};

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

            let lhf = LocalHeader::read(f, p.lhf_offset, p.cd_index + 1)?;
            let new_flags = with_bit11(read_u16(&lhf.header, 6), p.new_bit11_set);
            let new_extra_len = checked_u16(
                p.lhf_extra_len as u64 + absorb,
                "LFH extra field length exceeds ZIP limit",
            )?;

            f.seek(SeekFrom::Start(new_lhf_off)).map_err(io_err)?;
            let header = lhf.patched_header(new_flags, p.new_fname.len(), new_extra_len)?;
            f.write_all(&header).map_err(io_err)?;
            f.write_all(&p.new_fname).map_err(io_err)?;

            // Write a proper, independent Padding Extra Field (0xFFFF)
            write_padding_extra(f, absorb)?;
            // Keep original extra fields intact and untouched
            f.write_all(&lhf.extra).map_err(io_err)?;

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

            f.seek(SeekFrom::Start(p.lhf_offset + 6)).map_err(io_err)?;
            let mut flags_buf = [0u8; 2];
            f.read_exact(&mut flags_buf).map_err(io_err)?;
            let new_flags = with_bit11(read_u16(&flags_buf, 0), p.new_bit11_set);
            f.seek(SeekFrom::Start(p.lhf_offset + 6)).map_err(io_err)?;
            f.write_all(&new_flags.to_le_bytes()).map_err(io_err)?;

            write_pos = p.lhf_offset + p.span_size;
        } else {
            let new_lhf_off = p.lhf_offset - carry;
            new_lhf_offsets[i] = Some(new_lhf_off);

            let lhf = LocalHeader::read(f, p.lhf_offset, p.cd_index + 1)?;
            let new_flags = with_bit11(read_u16(&lhf.header, 6), p.new_bit11_set);

            f.seek(SeekFrom::Start(new_lhf_off)).map_err(io_err)?;
            let extra_len = checked_u16(
                lhf.extra_len() as u64,
                "LFH extra field length exceeds ZIP limit",
            )?;
            lhf.write(f, &p.new_fname, new_flags, extra_len)?;

            let data_src = p.lhf_offset + p.lhf_header_size;
            let new_header_size = 30 + p.new_fname.len() as u64 + lhf.extra_len() as u64;
            let data_dst = new_lhf_off + new_header_size;
            copy_within_file_with_buf(f, data_src, data_dst, p.payload_size(), &mut copy_buf)?;

            write_pos = data_dst + p.payload_size();
            carry += delta;
        }
    }

    let new_cd_start = write_pos;

    f.seek(SeekFrom::Start(new_cd_start)).map_err(io_err)?;
    let mut cd_write_pos = new_cd_start;
    let mut cd_entries_written: u64 = 0;

    let mut cd_order: Vec<usize> = (0..plans.len()).collect();
    cd_order.sort_by_key(|&i| plans[i].cd_index);

    for i in cd_order {
        let p = &plans[i];
        if p.excluded {
            continue;
        }
        let new_lhf = new_lhf_offsets[i]
            .ok_or_else(|| format!("missing LFH offset for CD entry {}", p.cd_index + 1))?;
        let cd_bytes = build_cd_entry(p, new_lhf)?;
        f.write_all(&cd_bytes).map_err(io_err)?;
        cd_write_pos += cd_bytes.len() as u64;
        cd_entries_written += 1;
    }

    let cd_size = cd_write_pos - new_cd_start;
    let needs_zip64 = info.is_zip64
        || cd_entries_written > 0xFFFF
        || cd_size > 0xFFFF_FFFF
        || new_cd_start > 0xFFFF_FFFF;

    if needs_zip64 {
        write_zip64_eocd(
            f,
            &mut cd_write_pos,
            cd_entries_written,
            cd_size,
            new_cd_start,
            &info.archive_comment,
        )?;
    } else {
        write_eocd(
            f,
            checked_u16(
                cd_entries_written,
                "central directory entry count exceeds ZIP limit",
            )?,
            cd_size as u32,
            new_cd_start as u32,
            &info.archive_comment,
        )?;
    }

    let new_file_len = f.stream_position().map_err(io_err)?;
    f.set_len(new_file_len)
        .map_err(|e| format!("truncate failed: {}", e))?;

    Ok(())
}

fn write_padding_extra<W: Write>(w: &mut W, absorb: u64) -> Result<(), String> {
    debug_assert!(absorb >= MIN_PADDING);
    let data_len = absorb - 4;
    let data_len_u16 = checked_u16(data_len, "padding extra field exceeds ZIP limit")?;
    let mut hdr = [0u8; 4];
    write_u16(&mut hdr, 0, PADDING_EXTRA_FIELD_ID);
    write_u16(&mut hdr, 2, data_len_u16);
    w.write_all(&hdr).map_err(io_err)?;
    let zeros = [0u8; 8192];
    let mut remaining = data_len;
    while remaining > 0 {
        let to_write = remaining.min(zeros.len() as u64) as usize;
        w.write_all(&zeros[..to_write]).map_err(io_err)?;
        remaining -= to_write as u64;
    }
    Ok(())
}
