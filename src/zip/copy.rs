use std::io::{Read, Seek, SeekFrom, Write};

use super::{io_err, COPY_BUF_SIZE};

pub(super) fn stream_copy<R: Read, W: Write>(r: &mut R, w: &mut W, len: u64) -> Result<(), String> {
    let mut remaining = len;
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    while remaining > 0 {
        let to_read = remaining.min(COPY_BUF_SIZE as u64) as usize;
        let n = r.read(&mut buf[..to_read]).map_err(io_err)?;
        if n == 0 {
            return Err(format!(
                "unexpected EOF while copying {} bytes ({} remaining)",
                len, remaining
            ));
        }
        w.write_all(&buf[..n]).map_err(io_err)?;
        remaining -= n as u64;
    }
    Ok(())
}

/// Copy bytes within the same file from src to dst (src > dst guaranteed by invariant).
#[cfg(test)]
pub(super) fn copy_within_file(
    f: &mut std::fs::File,
    src: u64,
    dst: u64,
    len: u64,
) -> Result<(), String> {
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    copy_within_file_with_buf(f, src, dst, len, &mut buf)
}

pub(super) fn copy_within_file_with_buf(
    f: &mut std::fs::File,
    src: u64,
    dst: u64,
    len: u64,
    buf: &mut [u8],
) -> Result<(), String> {
    if len == 0 || src == dst {
        return Ok(());
    }
    if buf.is_empty() {
        return Err("in-place copy buffer is empty".into());
    }
    if dst > src {
        return Err("in-place copy invariant violated (dst > src)".into());
    }

    let mut remaining = len;
    let mut read_pos = src;
    let mut write_pos = dst;

    while remaining > 0 {
        let to_read = remaining.min(buf.len() as u64) as usize;
        f.seek(SeekFrom::Start(read_pos)).map_err(io_err)?;
        let n = f.read(&mut buf[..to_read]).map_err(io_err)?;
        if n == 0 {
            return Err("unexpected EOF during in-place copy".into());
        }
        f.seek(SeekFrom::Start(write_pos)).map_err(io_err)?;
        f.write_all(&buf[..n]).map_err(io_err)?;
        read_pos += n as u64;
        write_pos += n as u64;
        remaining -= n as u64;
    }
    Ok(())
}
