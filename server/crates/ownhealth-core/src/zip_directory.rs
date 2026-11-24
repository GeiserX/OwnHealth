//! Bound ZIP metadata before the ZIP reader allocates an entry index.

use std::io::{Read, Seek, SeekFrom};

use zip::ZipArchive;
use zip::read::{ArchiveOffset, Config};
use zip::result::{ZipError, ZipResult};

pub const GARMIN_MAX_FILES: usize = 50_000;
pub const GARMIN_MAX_NAME_BYTES: usize = 4 * 1024 * 1024;
pub const DIRECTORY_LIMIT: &str = "ZIP directory exceeds the metadata budget";

// Allow room for folders and containers in addition to the importer's file budget. The directory
// budget includes the 46-byte header, extra fields and comments, plus the separate name budget.
const MAX_ENTRIES: u64 = 2 * GARMIN_MAX_FILES as u64;
const MAX_DIRECTORY_BYTES: u64 = MAX_ENTRIES * (46 + 128) + GARMIN_MAX_NAME_BYTES as u64;
const BAD_DIRECTORY: ZipError = ZipError::InvalidArchive(std::borrow::Cow::Borrowed("Invalid ZIP directory"));
const END_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
const END_BYTES: usize = 22;
const MAX_COMMENT_BYTES: usize = u16::MAX as usize;
const LOCATOR_BYTES: u64 = 20;
const ZIP64_END_BYTES: u64 = 56;

struct Directory {
    entries: u64,
    size: u64,
    start: u64,
    end: u64,
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..][..2].try_into().expect("ZIP u16 field"))
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..][..4].try_into().expect("ZIP u32 field"))
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..][..8].try_into().expect("ZIP u64 field"))
}

fn read_at<const N: usize>(reader: &mut (impl Read + Seek), offset: u64) -> ZipResult<[u8; N]> {
    let mut bytes = [0; N];
    reader.seek(SeekFrom::Start(offset))?;
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn zip32_end(reader: &mut (impl Read + Seek)) -> ZipResult<(u64, [u8; END_BYTES])> {
    let end = reader.seek(SeekFrom::End(0))?;
    let start = end.saturating_sub((END_BYTES + MAX_COMMENT_BYTES) as u64);
    let mut tail = vec![0; (end - start) as usize];
    reader.seek(SeekFrom::Start(start))?;
    reader.read_exact(&mut tail)?;
    let index = tail
        .windows(END_SIGNATURE.len())
        .rposition(|bytes| bytes == END_SIGNATURE)
        .ok_or(BAD_DIRECTORY)?;
    let record = tail.get(index..index + END_BYTES).ok_or(BAD_DIRECTORY)?;
    if index + END_BYTES + usize::from(u16_at(record, 20)) != tail.len() {
        return Err(BAD_DIRECTORY);
    }
    // Choose the last signature, as the indexer does. The central directory must end at this
    // record (or its ZIP64 end record), so the indexer cannot fall back to a different directory.
    Ok((start + index as u64, record.try_into().expect("ZIP end record")))
}

fn zip64_directory(reader: &mut (impl Read + Seek), locator_at: u64, locator: &[u8; 20]) -> ZipResult<Directory> {
    if u32_at(locator, 4) != 0 || u32_at(locator, 16) != 1 {
        return Err(BAD_DIRECTORY);
    }
    let relative_end = u64_at(locator, 8);
    let last_possible_end = locator_at.checked_sub(ZIP64_END_BYTES).ok_or(BAD_DIRECTORY)?;
    if relative_end > last_possible_end {
        return Err(BAD_DIRECTORY);
    }
    // Normal ZIP64 archives give the absolute position in the locator. With a prepended stub,
    // the fixed record directly before the locator gives the physical position instead.
    let mut end = relative_end;
    let mut record = read_at::<56>(reader, end)?;
    if !record.starts_with(b"PK\x06\x06") && end != last_possible_end {
        end = last_possible_end;
        record = read_at::<56>(reader, end)?;
    }
    let record_size = u64_at(&record, 4);
    let entries = u64_at(&record, 32);
    let size = u64_at(&record, 40);
    let start = u64_at(&record, 48);
    if !record.starts_with(b"PK\x06\x06")
        || !(44..=44 + MAX_COMMENT_BYTES as u64).contains(&record_size)
        || end + record_size + 12 != locator_at
        || u32_at(&record, 16) != 0
        || u32_at(&record, 20) != 0
        || u64_at(&record, 24) != entries
        || start.checked_add(size) != Some(relative_end)
    {
        return Err(BAD_DIRECTORY);
    }
    Ok(Directory {
        entries,
        size,
        start,
        end,
    })
}

fn directory(reader: &mut (impl Read + Seek)) -> ZipResult<Directory> {
    let (end, record) = zip32_end(reader)?;
    let entries = u16_at(&record, 10);
    let size = u32_at(&record, 12);
    let start = u32_at(&record, 16);
    if (entries == u16::MAX || size == u32::MAX || start == u32::MAX)
        && let Some(locator_at) = end.checked_sub(LOCATOR_BYTES)
    {
        let locator = read_at::<20>(reader, locator_at)?;
        if locator.starts_with(b"PK\x06\x07") {
            return zip64_directory(reader, locator_at, &locator);
        }
    }
    if u16_at(&record, 4) != 0 || u16_at(&record, 6) != 0 || u16_at(&record, 8) != entries {
        return Err(BAD_DIRECTORY);
    }
    Ok(Directory {
        entries: u64::from(entries),
        size: u64::from(size),
        start: u64::from(start),
        end,
    })
}

fn open_with_limits<R: Read + Seek>(
    mut reader: R,
    max_entries: u64,
    max_directory_bytes: u64,
) -> ZipResult<ZipArchive<R>> {
    let d = directory(&mut reader)?;
    if d.entries > max_entries || d.size > max_directory_bytes {
        return Err(ZipError::UnsupportedArchive(DIRECTORY_LIMIT));
    }
    let physical_start = d.end.checked_sub(d.size).ok_or(BAD_DIRECTORY)?;
    let offset = physical_start.checked_sub(d.start).ok_or(BAD_DIRECTORY)?;
    if d.entries > d.size / 46 || (d.entries == 0 && d.size != 0) {
        return Err(BAD_DIRECTORY);
    }
    let config = Config {
        archive_offset: ArchiveOffset::Known(offset),
    };
    ZipArchive::with_config(config, reader)
}

/// Open the archive used by upload classification and by the Garmin worker, including nested FIT
/// parts. Both the entry count and central directory bytes are checked before indexing.
pub fn open<R: Read + Seek>(reader: R) -> ZipResult<ZipArchive<R>> {
    open_with_limits(reader, MAX_ENTRIES, MAX_DIRECTORY_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_metadata_limits_apply_before_indexing_members() -> anyhow::Result<()> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for name in ["folder/", "other/", "UDSFile_1.json"] {
            if name.ends_with('/') {
                zip.add_directory(name, zip::write::SimpleFileOptions::default())?;
            } else {
                zip.start_file(name, zip::write::SimpleFileOptions::default())?;
            }
        }
        zip.set_comment("archive comment");
        let archive = zip.finish()?;
        assert_eq!(open_with_limits(archive.clone(), 3, 1024)?.len(), 3);
        for (entries, bytes) in [(2, 1024), (3, 100)] {
            assert!(matches!(
                open_with_limits(archive.clone(), entries, bytes),
                Err(ZipError::UnsupportedArchive(DIRECTORY_LIMIT))
            ));
        }
        Ok(())
    }
}
