//! Bgzipped, tabix-indexed VCFs with noodles: what `pysam.VariantFile(..., index_filename=...)` did.
//!
//! The index is read from its own path (blob names are hashes). `fetch` answers the records that
//! overlap a 1-based closed range the way htslib's tabix iterator does: a record spans POS to its
//! INFO/END, else to POS + len(REF) - 1. Only the first sample is read; the fields the lookups need
//! are parsed from the text line.

use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::Path;

use noodles::bgzf;
use noodles::core::Position;
use noodles::core::region::Interval;
use noodles::csi::BinningIndex;
use noodles::csi::binning_index::index::header::Builder as HeaderBuilder;
use noodles::csi::binning_index::index::reference_sequence::bin::Chunk;
use noodles::tabix;

/// One VCF data line, as pysam showed it.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub chrom: String,
    /// 1-based POS.
    pub pos: i64,
    /// 1-based closed end: INFO/END, else POS + len(REF) - 1 (pysam's `stop`).
    pub end: i64,
    /// None for `.`.
    pub id: Option<String>,
    pub reference: String,
    /// Empty for `.` (pysam's `alts` is None then).
    pub alts: Vec<String>,
    pub format: Vec<String>,
    /// The first sample's values, None when the file has no sample column.
    pub sample: Option<Vec<String>>,
}

impl Record {
    pub fn parse(line: &str) -> Option<Record> {
        let cols: Vec<&str> = line.trim_end_matches(['\n', '\r']).split('\t').collect();
        if cols.len() < 8 {
            return None;
        }
        let pos: i64 = cols[1].parse().ok()?;
        let reference = cols[3].to_string();
        let alts = if cols[4] == "." {
            Vec::new()
        } else {
            cols[4].split(',').map(str::to_string).collect()
        };
        let info_end = cols[7]
            .split(';')
            .find_map(|kv| kv.strip_prefix("END="))
            .and_then(|v| v.parse::<i64>().ok());
        let end = info_end.unwrap_or(pos + reference.len().max(1) as i64 - 1);
        let format = cols
            .get(8)
            .map_or_else(Vec::new, |f| f.split(':').map(str::to_string).collect());
        let sample = cols.get(9).map(|s| s.split(':').map(str::to_string).collect());
        Some(Record {
            chrom: cols[0].to_string(),
            pos,
            end,
            id: (cols[2] != ".").then(|| cols[2].to_string()),
            reference,
            alts,
            format,
            sample,
        })
    }

    /// REF then ALT (pysam's `alleles`).
    pub fn alleles(&self) -> Vec<&str> {
        std::iter::once(self.reference.as_str())
            .chain(self.alts.iter().map(String::as_str))
            .collect()
    }

    /// One FORMAT value of the first sample: None when the key is absent or the value is `.`.
    pub fn sample_value(&self, key: &str) -> Option<&str> {
        let i = self.format.iter().position(|k| k == key)?;
        let v = self.sample.as_ref()?.get(i)?.as_str();
        (!v.is_empty() && v != ".").then_some(v)
    }

    /// The GT alleles (None for `.`) and whether every separator is `|`.
    pub fn genotype(&self) -> (Vec<Option<usize>>, bool) {
        let Some(gt) = self.sample_value("GT") else {
            return (vec![None], false);
        };
        let alleles: Vec<Option<usize>> = gt.split(['/', '|']).map(|a| a.parse().ok()).collect();
        let seps: Vec<char> = gt.chars().filter(|c| *c == '/' || *c == '|').collect();
        let phased = !seps.is_empty() && seps.iter().all(|c| *c == '|');
        (alleles, phased)
    }
}

/// A bgzipped VCF opened with its tabix index.
pub struct VcfFile {
    reader: bgzf::io::Reader<File>,
    index: tabix::Index,
}

impl VcfFile {
    pub fn open(data: &Path, index: &Path) -> io::Result<VcfFile> {
        let index = File::open(index).map(tabix::io::Reader::new)?.read_index()?;
        let reader = bgzf::io::Reader::new(File::open(data)?);
        Ok(VcfFile { reader, index })
    }

    /// Opens and parses the header: a bgzipped text file that starts with `##fileformat=VCF` and has
    /// its `#CHROM` line.
    pub fn check_header(&mut self) -> io::Result<()> {
        self.reader.seek(bgzf::VirtualPosition::default())?;
        let mut line = String::new();
        let mut first = true;
        loop {
            line.clear();
            if self.reader.read_line(&mut line)? == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "no #CHROM header line"));
            }
            if first && !line.starts_with("##fileformat=VCF") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "not a VCF: no ##fileformat line",
                ));
            }
            first = false;
            if line.starts_with("#CHROM") {
                return Ok(());
            }
            if !line.starts_with("##") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a data line before the #CHROM line",
                ));
            }
        }
    }

    /// Records of contig `name` overlapping [start, end] (1-based, closed). None when the index does
    /// not know the contig (pysam raised ValueError then).
    pub fn fetch_contig(&mut self, name: &str, start: i64, end: i64) -> io::Result<Option<Vec<Record>>> {
        let Some(id) = self
            .index
            .header()
            .and_then(|h| h.reference_sequence_names().get_index_of(name.as_bytes()))
        else {
            return Ok(None);
        };
        let to_pos = |p: i64| Position::try_from(usize::try_from(p.max(1)).unwrap_or(1)).unwrap_or(Position::MIN);
        let interval: Interval = (to_pos(start)..=to_pos(end.max(start))).into();
        let chunks = self.index.query(id, interval)?;
        let mut out = Vec::new();
        let mut line = String::new();
        for chunk in chunks {
            self.reader.seek(chunk.start())?;
            while self.reader.virtual_position() < chunk.end() {
                line.clear();
                if self.reader.read_line(&mut line)? == 0 {
                    break;
                }
                if line.starts_with('#') {
                    continue;
                }
                if let Some(rec) = Record::parse(&line)
                    && rec.chrom == name
                    && rec.pos <= end
                    && rec.end >= start
                {
                    out.push(rec);
                }
            }
        }
        Ok(Some(out))
    }

    /// The first of `names` the index knows, fetched; empty when none is known.
    pub fn fetch(&mut self, names: &[String], start: i64, end: i64) -> io::Result<Vec<Record>> {
        for name in names {
            if let Some(records) = self.fetch_contig(name, start, end)? {
                return Ok(records);
            }
        }
        Ok(Vec::new())
    }
}

/// Bgzip `header` and `lines` (data lines in coordinate order) and build their tabix index.
/// Returns (vcf.gz bytes, .tbi bytes).
pub fn write_indexed(header: &[String], lines: &[String]) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut writer = bgzf::io::Writer::new(Vec::new());
    for h in header {
        writeln!(writer, "{h}")?;
    }
    let mut indexer = tabix::index::Indexer::default();
    indexer.set_header(HeaderBuilder::vcf().build());
    let mut start = writer.virtual_position();
    for line in lines {
        let rec = Record::parse(line)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("not a VCF line: {line}")))?;
        writeln!(writer, "{line}")?;
        let stop = writer.virtual_position();
        let p = |v: i64| {
            usize::try_from(v)
                .ok()
                .and_then(|v| Position::try_from(v).ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "position below 1"))
        };
        indexer.add_record(
            &rec.chrom,
            p(rec.pos)?,
            p(rec.end.max(rec.pos))?,
            Chunk::new(start, stop),
        )?;
        start = stop;
    }
    let data = writer.finish()?;
    let mut index_writer = tabix::io::Writer::new(Vec::new());
    index_writer.write_index(&indexer.build())?;
    let index = index_writer.into_inner().finish()?;
    Ok((data, index))
}
