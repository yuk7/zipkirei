use std::io::{Read, Seek, SeekFrom, Write};

use super::bytes::{read_u16, read_u32, read_u64, write_u16, write_u32, write_u64};
use super::{
    ArchiveError, Error, Result, UnsupportedFeature, EOCD_SIG, ZIP64_EOCD_LOCATOR_SIG,
    ZIP64_EOCD_SIG,
};

#[derive(Debug)]
pub(super) struct ArchiveInfo {
    /// Offset of first byte of Central Directory
    pub(super) cd_offset: u64,
    /// Size of Central Directory
    pub(super) cd_size: u64,
    /// Total number of CD entries
    pub(super) total_entries: u64,
    /// True if ZIP64
    pub(super) is_zip64: bool,
    /// Archive comment stored after EOCD
    pub(super) archive_comment: Vec<u8>,
}

pub(super) fn find_archive_info<R: Read + Seek>(r: &mut R, file_len: u64) -> Result<ArchiveInfo> {
    if file_len < 22 {
        return Err(Error::InvalidArchive(ArchiveError::TooSmall));
    }

    let search_from = file_len.saturating_sub(22 + 65535);
    let mut buf = [0u8; 22];

    let mut pos = file_len - 22;
    loop {
        r.seek(SeekFrom::Start(pos))?;
        r.read_exact(&mut buf)?;
        if read_u32(&buf, 0) == EOCD_SIG {
            let comment_len = read_u16(&buf, 20) as u64;
            if pos + 22 + comment_len == file_len {
                return parse_eocd(r, &buf, pos, file_len);
            }
        }
        if pos == search_from {
            break;
        }
        pos -= 1;
    }

    Err(Error::InvalidArchive(ArchiveError::EocdNotFound))
}

pub(super) fn parse_eocd<R: Read + Seek>(
    r: &mut R,
    eocd_buf: &[u8; 22],
    eocd_offset: u64,
    file_len: u64,
) -> Result<ArchiveInfo> {
    let disk = read_u16(eocd_buf, 4);
    let cd_disk = read_u16(eocd_buf, 6);
    if disk != 0 || cd_disk != 0 {
        return Err(Error::Unsupported(UnsupportedFeature::MultiDisk));
    }

    let entries_this = read_u16(eocd_buf, 8) as u64;
    let total_entries = read_u16(eocd_buf, 10) as u64;
    let cd_size = read_u32(eocd_buf, 12) as u64;
    let cd_offset = read_u32(eocd_buf, 16) as u64;
    let comment_len = read_u16(eocd_buf, 20) as usize;
    let mut archive_comment = vec![0u8; comment_len];
    if comment_len > 0 {
        r.seek(SeekFrom::Start(eocd_offset + 22))?;
        r.read_exact(&mut archive_comment)?;
    }

    let needs_zip64 = entries_this == 0xFFFF
        || total_entries == 0xFFFF
        || cd_size == 0xFFFF_FFFF
        || cd_offset == 0xFFFF_FFFF;

    if !needs_zip64 {
        if entries_this != total_entries {
            return Err(Error::Unsupported(UnsupportedFeature::EntryCountMismatch));
        }
        validate_cd_range(cd_offset, cd_size, eocd_offset, file_len)?;
        return Ok(ArchiveInfo {
            cd_offset,
            cd_size,
            total_entries,
            is_zip64: false,
            archive_comment,
        });
    }

    if eocd_offset < 20 {
        return Err(Error::invalid_archive(
            "ZIP64 EOCD locator not found before EOCD",
        ));
    }
    let locator_offset = eocd_offset - 20;
    let mut loc_buf = [0u8; 20];
    r.seek(SeekFrom::Start(locator_offset))?;
    r.read_exact(&mut loc_buf)?;
    if read_u32(&loc_buf, 0) != ZIP64_EOCD_LOCATOR_SIG {
        return Err(Error::invalid_archive(
            "ZIP64 EOCD locator signature not found",
        ));
    }
    let z64_eocd_disk = read_u32(&loc_buf, 4);
    let z64_eocd_offset = read_u64(&loc_buf, 8);
    let total_disks = read_u32(&loc_buf, 16);
    if z64_eocd_disk != 0 || total_disks != 1 {
        return Err(Error::Unsupported(UnsupportedFeature::MultiDiskZip64));
    }

    let mut z64_buf = [0u8; 56];
    r.seek(SeekFrom::Start(z64_eocd_offset))?;
    r.read_exact(&mut z64_buf)?;
    if read_u32(&z64_buf, 0) != ZIP64_EOCD_SIG {
        return Err(Error::invalid_archive("invalid ZIP64 EOCD signature"));
    }
    let z64_eocd_size = read_u64(&z64_buf, 4);
    if z64_eocd_size < 44 {
        return Err(Error::invalid_archive("ZIP64 EOCD record is too small"));
    }
    let z64_eocd_end = z64_eocd_offset
        .checked_add(12)
        .and_then(|v| v.checked_add(z64_eocd_size))
        .ok_or_else(|| Error::limit_exceeded("ZIP64 EOCD record range overflows u64"))?;
    if z64_eocd_end > locator_offset {
        return Err(Error::invalid_archive(
            "ZIP64 EOCD record overlaps ZIP64 locator",
        ));
    }

    let z64_disk = read_u32(&z64_buf, 16);
    let z64_cd_disk = read_u32(&z64_buf, 20);
    if z64_disk != 0 || z64_cd_disk != 0 {
        return Err(Error::Unsupported(UnsupportedFeature::MultiDiskZip64));
    }

    let entries_this64 = read_u64(&z64_buf, 24);
    let total_entries64 = read_u64(&z64_buf, 32);
    let cd_size64 = read_u64(&z64_buf, 40);
    let cd_offset64 = read_u64(&z64_buf, 48);

    if entries_this64 != total_entries64 {
        return Err(Error::unsupported(
            "entry count mismatch in ZIP64 EOCD; multi-disk may be unsupported",
        ));
    }
    validate_cd_range(cd_offset64, cd_size64, z64_eocd_offset, file_len)?;

    Ok(ArchiveInfo {
        cd_offset: cd_offset64,
        cd_size: cd_size64,
        total_entries: total_entries64,
        is_zip64: true,
        archive_comment,
    })
}

pub(super) fn write_eocd<W: Write>(
    w: &mut W,
    entries: u16,
    cd_size: u32,
    cd_offset: u32,
    comment: &[u8],
) -> Result<()> {
    let mut buf = Vec::with_capacity(22 + comment.len());
    build_eocd_into(&mut buf, entries, cd_size, cd_offset, comment)?;
    w.write_all(&buf)?;
    Ok(())
}

pub(super) fn write_zip64_eocd<W: Write + Seek>(
    w: &mut W,
    pos: &mut u64,
    entries: u64,
    cd_size: u64,
    cd_offset: u64,
    comment: &[u8],
) -> Result<()> {
    let z64_eocd_off = *pos;
    let mut buf = Vec::with_capacity(56 + 20 + 22 + comment.len());
    build_zip64_eocd_into(&mut buf, z64_eocd_off, entries, cd_size, cd_offset, comment)?;
    w.write_all(&buf)?;
    *pos += buf.len() as u64;

    Ok(())
}

pub(super) fn build_eocd_into(
    out: &mut Vec<u8>,
    entries: u16,
    cd_size: u32,
    cd_offset: u32,
    comment: &[u8],
) -> Result<()> {
    let comment_len = u16::try_from(comment.len())
        .map_err(|_| Error::limit_exceeded("archive comment is too long for EOCD"))?;
    let mut eocd = [0u8; 22];
    write_u32(&mut eocd, 0, EOCD_SIG);
    write_u16(&mut eocd, 8, entries);
    write_u16(&mut eocd, 10, entries);
    write_u32(&mut eocd, 12, cd_size);
    write_u32(&mut eocd, 16, cd_offset);
    write_u16(&mut eocd, 20, comment_len);
    out.extend_from_slice(&eocd);
    out.extend_from_slice(comment);
    Ok(())
}

pub(super) fn build_zip64_eocd_into(
    out: &mut Vec<u8>,
    z64_eocd_off: u64,
    entries: u64,
    cd_size: u64,
    cd_offset: u64,
    comment: &[u8],
) -> Result<()> {
    let mut z64 = [0u8; 56];
    write_u32(&mut z64, 0, ZIP64_EOCD_SIG);
    write_u64(&mut z64, 4, 44u64);
    write_u16(&mut z64, 12, 45);
    write_u16(&mut z64, 14, 45);
    write_u64(&mut z64, 24, entries);
    write_u64(&mut z64, 32, entries);
    write_u64(&mut z64, 40, cd_size);
    write_u64(&mut z64, 48, cd_offset);
    out.extend_from_slice(&z64);

    let mut loc = [0u8; 20];
    write_u32(&mut loc, 0, ZIP64_EOCD_LOCATOR_SIG);
    write_u64(&mut loc, 8, z64_eocd_off);
    write_u32(&mut loc, 16, 1);
    out.extend_from_slice(&loc);

    let comment_len = u16::try_from(comment.len())
        .map_err(|_| Error::limit_exceeded("archive comment is too long for EOCD"))?;

    let mut eocd = [0u8; 22];
    write_u32(&mut eocd, 0, EOCD_SIG);
    write_u16(&mut eocd, 8, 0xFFFF);
    write_u16(&mut eocd, 10, 0xFFFF);
    write_u32(&mut eocd, 12, 0xFFFF_FFFF);
    write_u32(&mut eocd, 16, 0xFFFF_FFFF);
    write_u16(&mut eocd, 20, comment_len);
    out.extend_from_slice(&eocd);
    out.extend_from_slice(comment);

    Ok(())
}

fn validate_cd_range(cd_offset: u64, cd_size: u64, cd_end_limit: u64, file_len: u64) -> Result<()> {
    let cd_end = cd_offset
        .checked_add(cd_size)
        .ok_or_else(|| Error::limit_exceeded("Central Directory range overflows u64"))?;
    if cd_end > file_len {
        return Err(Error::InvalidArchive(ArchiveError::CentralDirectoryRange {
            offset: cd_offset,
            size: cd_size,
            file_len,
        }));
    }
    if cd_end > cd_end_limit {
        return Err(Error::InvalidArchive(
            ArchiveError::CentralDirectoryOverlap {
                offset: cd_offset,
                size: cd_size,
                limit: cd_end_limit,
            },
        ));
    }
    Ok(())
}
