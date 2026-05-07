use std::io::{Read, Seek, Write};

mod bytes;
mod cd_entry;
mod copy;
mod eocd;
mod error;
mod inplace;
mod local_header;
mod options;
mod plan;
mod write_new;

use eocd::find_archive_info;
pub use error::{Error, Result as ZipResult};
pub use inplace::process_file;
pub use options::Options;
use plan::{build_plans, EntryPlan};
use write_new::write_new_archive;

// ZIP format constants

const LOCAL_FILE_HEADER_SIG: u32 = 0x04034b50;
const CENTRAL_DIR_SIG: u32 = 0x02014b50;
const EOCD_SIG: u32 = 0x06054b50;
const ZIP64_EOCD_SIG: u32 = 0x06064b50;
const ZIP64_EOCD_LOCATOR_SIG: u32 = 0x07064b50;
const ZIP64_EXTRA_FIELD_ID: u16 = 0x0001;
const PADDING_EXTRA_FIELD_ID: u16 = 0xFFFF;
const BIT11: u16 = 0x0800;

// Minimum padding extra field size: id(2) + size(2) = 4
const MIN_PADDING: u64 = 4;

const COPY_BUF_SIZE: usize = 4 * 1024 * 1024;

// Public entry points

pub fn process_new<R, W>(
    input: &mut R,
    file_len: u64,
    output: &mut W,
    opts: &Options,
    stdout: &mut impl Write,
) -> ZipResult<()>
where
    R: Read + Seek,
    W: Write + Seek,
{
    let info = find_archive_info(input, file_len)?;
    let plans = build_plans(input, &info, opts)?;

    if opts.dry_run {
        return dry_run_report(&plans, stdout).map_err(Error::from);
    }

    write_new_archive(input, output, &info, &plans).map_err(Error::from)
}

// Dry run

fn dry_run_report<W: Write>(plans: &[EntryPlan], out: &mut W) -> Result<(), String> {
    let mut excluded_count = 0u64;
    let mut orphan_bytes = 0u64;
    let mut nfc_count = 0u64;
    let mut nfc_saved = 0u64;
    let mut bit11_count = 0u64;

    for p in plans {
        let name = String::from_utf8_lossy(&p.orig_fname);
        if p.excluded {
            excluded_count += 1;
            orphan_bytes += p.span_size;
            writeln!(out, "[exclude]  {}  ({} B)", name, p.span_size).map_err(io_err)?;
        } else {
            let delta = p.fname_delta();
            if delta > 0 {
                nfc_count += 1;
                nfc_saved += delta;
                let new_name = String::from_utf8_lossy(&p.new_fname);
                writeln!(
                    out,
                    "[nfc]      {}  →  {}  ({} B shorter)",
                    name, new_name, delta
                )
                .map_err(io_err)?;
            }
            if p.needs_bit11 {
                bit11_count += 1;
                writeln!(out, "[bit11]    {}", name).map_err(io_err)?;
            }
        }
    }

    writeln!(out).map_err(io_err)?;
    writeln!(out, "Summary:").map_err(io_err)?;
    writeln!(
        out,
        "  Excluded:     {} entries (orphan data: {} B)",
        excluded_count, orphan_bytes
    )
    .map_err(io_err)?;
    writeln!(
        out,
        "  NFC renamed:  {} entries (total saved: {} B)",
        nfc_count, nfc_saved
    )
    .map_err(io_err)?;
    writeln!(out, "  bit11 set:    {} entries", bit11_count).map_err(io_err)?;

    Ok(())
}

// I/O primitives

fn io_err(e: impl std::fmt::Display) -> String {
    format!("I/O error: {}", e)
}

#[inline]
fn with_bit11(flags: u16, enabled: bool) -> u16 {
    if enabled {
        flags | BIT11
    } else {
        flags & !BIT11
    }
}

fn checked_u16(value: u64, context: &str) -> std::result::Result<u16, String> {
    u16::try_from(value).map_err(|_| format!("{context}: {value}"))
}

#[cfg(test)]
mod tests;
