use std::io::{Read, Seek, Write};

use super::bytes::read_u16;
use super::cd_entry::build_cd_entry;
use super::copy::stream_copy;
use super::eocd::{write_eocd, write_zip64_eocd, ArchiveInfo};
use super::local_header::LocalHeader;
use super::plan::EntryPlan;
use super::{checked_u16, io_err, with_bit11};

pub(super) fn write_new_archive<R, W>(
    r: &mut R,
    w: &mut W,
    info: &ArchiveInfo,
    plans: &[EntryPlan],
) -> Result<(), String>
where
    R: Read + Seek,
    W: Write + Seek,
{
    let mut new_lhf_offsets: Vec<Option<u64>> = Vec::with_capacity(plans.len());
    let mut write_pos: u64 = 0;

    for p in plans {
        if p.excluded {
            new_lhf_offsets.push(None);
            continue;
        }

        new_lhf_offsets.push(Some(write_pos));

        let lhf = LocalHeader::read(r, p.lhf_offset, p.cd_index + 1)?;
        let new_flags = with_bit11(read_u16(&lhf.header, 6), p.new_bit11_set);
        let extra_len = checked_u16(
            lhf.extra_len() as u64,
            "LFH extra field length exceeds ZIP limit",
        )?;
        lhf.write(w, &p.new_fname, new_flags, extra_len)?;
        write_pos += 30 + p.new_fname.len() as u64 + lhf.extra_len() as u64;

        stream_copy(r, w, p.payload_size())?;
        write_pos += p.payload_size();
    }

    let cd_start = write_pos;
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
        w.write_all(&cd_bytes).map_err(io_err)?;
        write_pos += cd_bytes.len() as u64;
        cd_entries_written += 1;
    }

    let cd_size = write_pos - cd_start;

    let needs_zip64 = cd_entries_written > 0xFFFF
        || cd_size > 0xFFFF_FFFF
        || cd_start > 0xFFFF_FFFF
        || write_pos > 0xFFFF_FFFF;

    if needs_zip64 {
        write_zip64_eocd(
            w,
            &mut write_pos,
            cd_entries_written,
            cd_size,
            cd_start,
            &info.archive_comment,
        )?;
    } else {
        write_eocd(
            w,
            checked_u16(
                cd_entries_written,
                "central directory entry count exceeds ZIP limit",
            )?,
            cd_size as u32,
            cd_start as u32,
            &info.archive_comment,
        )?;
    }

    Ok(())
}
