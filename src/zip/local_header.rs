use std::io::{Read, Seek, SeekFrom, Write};

use super::bytes::{read_u16, read_u32, write_u16};
#[cfg(unix)]
use super::copy::read_exact_at;
use super::{checked_u16, Error, Result, LOCAL_FILE_HEADER_SIG};

pub(super) struct LocalHeader {
    pub(super) header: [u8; 30],
    pub(super) extra: Vec<u8>,
}

impl LocalHeader {
    pub(super) fn read<R: Read + Seek>(r: &mut R, offset: u64, entry_no: usize) -> Result<Self> {
        r.seek(SeekFrom::Start(offset))?;

        let mut header = [0u8; 30];
        r.read_exact(&mut header).map_err(|_| {
            Error::invalid_archive(format!("unexpected EOF reading LFH at entry {}", entry_no))
        })?;
        if read_u32(&header, 0) != LOCAL_FILE_HEADER_SIG {
            return Err(Error::invalid_archive(format!(
                "invalid LFH signature at entry {} (offset {:#x})",
                entry_no, offset
            )));
        }

        let fname_len = read_u16(&header, 26) as i64;
        let extra_len = read_u16(&header, 28) as usize;
        r.seek(SeekFrom::Current(fname_len))?;

        let mut extra = vec![0u8; extra_len];
        r.read_exact(&mut extra).map_err(|_| {
            Error::invalid_archive(format!(
                "unexpected EOF reading LFH extra at entry {}",
                entry_no
            ))
        })?;

        Ok(Self { header, extra })
    }

    #[cfg(unix)]
    pub(super) fn read_from_file(f: &std::fs::File, offset: u64, entry_no: usize) -> Result<Self> {
        let mut header = [0u8; 30];
        read_exact_at(f, &mut header, offset).map_err(|_| {
            Error::invalid_archive(format!("unexpected EOF reading LFH at entry {}", entry_no))
        })?;
        if read_u32(&header, 0) != LOCAL_FILE_HEADER_SIG {
            return Err(Error::invalid_archive(format!(
                "invalid LFH signature at entry {} (offset {:#x})",
                entry_no, offset
            )));
        }

        let fname_len = read_u16(&header, 26) as u64;
        let extra_len = read_u16(&header, 28) as usize;
        let mut extra = vec![0u8; extra_len];
        read_exact_at(f, &mut extra, offset + 30 + fname_len).map_err(|_| {
            Error::invalid_archive(format!(
                "unexpected EOF reading LFH extra at entry {}",
                entry_no
            ))
        })?;

        Ok(Self { header, extra })
    }

    pub(super) fn extra_len(&self) -> usize {
        self.extra.len()
    }

    pub(super) fn patched_header(
        &self,
        new_flags: u16,
        new_fname_len: usize,
        new_extra_len: u16,
    ) -> Result<[u8; 30]> {
        let new_fname_len = checked_u16(
            new_fname_len as u64,
            "LFH filename length exceeds ZIP limit",
        )?;
        let mut header = self.header;
        write_u16(&mut header, 6, new_flags);
        write_u16(&mut header, 26, new_fname_len);
        write_u16(&mut header, 28, new_extra_len);
        Ok(header)
    }

    pub(super) fn write<W: Write>(
        &self,
        w: &mut W,
        new_name: &[u8],
        new_flags: u16,
        new_extra_len: u16,
    ) -> Result<()> {
        let header = self.patched_header(new_flags, new_name.len(), new_extra_len)?;
        w.write_all(&header)?;
        w.write_all(new_name)?;
        w.write_all(&self.extra)?;
        Ok(())
    }
}
