use super::bytes::{read_u16, read_u32, write_u16, write_u32_slice, write_u64_slice};
use super::plan::EntryPlan;
use super::{checked_u16, with_bit11, ZIP64_EXTRA_FIELD_ID};

#[cfg(test)]
pub(crate) fn build_cd_entry(p: &EntryPlan, new_lhf_offset: u64) -> Result<Vec<u8>, String> {
    let mut out =
        Vec::with_capacity(46 + p.new_fname.len() + p.cd_extra.len() + p.cd_comment.len());
    build_cd_entry_into(p, new_lhf_offset, &mut out)?;
    Ok(out)
}

pub(crate) fn build_cd_entry_into(
    p: &EntryPlan,
    new_lhf_offset: u64,
    out: &mut Vec<u8>,
) -> Result<usize, String> {
    let start = out.len();
    let mut header = p.cd_header;

    let flags = with_bit11(read_u16(&header, 8), p.new_bit11_set);
    write_u16(&mut header, 8, flags);
    let fname_len = checked_u16(
        p.new_fname.len() as u64,
        "CD filename length exceeds ZIP limit",
    )?;
    write_u16(&mut header, 28, fname_len);

    if !p.lhf_offset_in_zip64_extra {
        if new_lhf_offset > 0xFFFF_FFFF {
            return Err(format!(
                "entry {}: LFH offset grown beyond 4 GB but no ZIP64 extra field present",
                p.cd_index + 1
            ));
        }
        write_u32_slice(&mut header, 42, new_lhf_offset as u32);
    }

    out.extend_from_slice(&header);
    out.extend_from_slice(&p.new_fname);

    let extra_start = out.len();
    out.extend_from_slice(&p.cd_extra);
    if p.lhf_offset_in_zip64_extra {
        patch_zip64_lhf_offset_in_extra(&mut out[extra_start..], new_lhf_offset, p)?;
    }
    out.extend_from_slice(&p.cd_comment);

    Ok(out.len() - start)
}

fn patch_zip64_lhf_offset_in_extra(
    extra: &mut [u8],
    new_lhf_offset: u64,
    p: &EntryPlan,
) -> Result<(), String> {
    let comp32 = read_u32(&p.cd_header, 20);
    let uncomp32 = read_u32(&p.cd_header, 24);

    let mut cursor = 0usize;
    while cursor + 4 <= extra.len() {
        let id = read_u16(extra, cursor);
        let sz = read_u16(extra, cursor + 2) as usize;
        let data_start = cursor + 4;
        cursor += 4;
        if cursor + sz > extra.len() {
            return Err(format!(
                "truncated extra field patching CD entry {}",
                p.cd_index + 1
            ));
        }
        if id == ZIP64_EXTRA_FIELD_ID {
            let mut off = data_start;
            if uncomp32 == 0xFFFF_FFFF {
                off += 8;
            }
            if comp32 == 0xFFFF_FFFF {
                off += 8;
            }
            if off + 8 > data_start + sz {
                return Err(format!(
                    "ZIP64 extra too short to hold LFH offset for CD entry {}",
                    p.cd_index + 1
                ));
            }
            write_u64_slice(extra, off, new_lhf_offset);
            return Ok(());
        }
        cursor += sz;
    }
    Err(format!(
        "ZIP64 extra field not found while patching CD entry {}",
        p.cd_index + 1
    ))
}
