//! Immutable segments: points flushed from the memtable, written once and never changed.
//!
//! A segment is a directory holding:
//!
//! | file | contents |
//! |---|---|
//! | `entries.bin` | one 29-byte entry per point, sorted by id: id tag, 16 id bytes, version (`u64`), row (`u32`, `u32::MAX` for a deletion) |
//! | `vectors.bin` | one row of `dim` little-endian `f32` per live point, memory mapped |
//! | `payloads.bin` | each live point's payload as JSON, back to back |
//! | `payload_offsets.bin` | `rows + 1` little-endian `u64` offsets into `payloads.bin` |
//! | `meta.json` | format version, counts, the range of WAL sequence numbers and every file's length and CRC32C |
//!
//! A deletion is kept as an entry without a row, because it has to hide older copies of the
//! point in older segments until a merge removes both.
//!
//! Segments are built in a temporary directory, every file and the directory are synced, and
//! only then is the directory renamed into place, so a segment directory either is complete
//! or is a leftover that no manifest lists. Loading checks every file against `meta.json`.

#[cfg(not(target_endian = "little"))]
compile_error!("segment files store little-endian floats and are memory mapped as native ones");

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use nocturnum_core::{Payload, PointId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crash::crash_point;
use crate::error::{Error, IoContext, Result};
use crate::sync::{sync_dir, sync_file};

const FORMAT_VERSION: u32 = 1;
const ENTRY_LEN: usize = 29;
const NO_ROW: u32 = u32::MAX;
const FILES: [&str; 4] = [
    "entries.bin",
    "vectors.bin",
    "payloads.bin",
    "payload_offsets.bin",
];

/// One point in a segment, or the deletion of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub id: PointId,
    /// The WAL sequence number of the write that produced this entry.
    pub version: u64,
    /// The point's row in the vector and payload files, `None` for a deletion.
    pub row: Option<u32>,
}

/// A point handed to [`write_segment`], in id order.
pub struct EntryIn<'a> {
    pub id: PointId,
    pub version: u64,
    /// The vector and payload, `None` for a deletion.
    pub value: Option<(&'a [f32], &'a Payload)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileMeta {
    len: u64,
    crc32c: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Meta {
    format: u32,
    id: u64,
    dim: usize,
    entries: usize,
    rows: usize,
    min_seq: u64,
    max_seq: u64,
    files: std::collections::BTreeMap<String, FileMeta>,
}

pub fn segment_dir_name(id: u64) -> String {
    format!("seg-{id:016x}")
}

/// Writes a segment from entries sorted by id, building it in `tmp` and renaming it to
/// `dest` once everything is on disk.
pub(crate) fn write_segment<'a>(
    tmp: &Path,
    dest: &Path,
    id: u64,
    dim: usize,
    entries: impl IntoIterator<Item = EntryIn<'a>>,
) -> Result<()> {
    if tmp.exists() {
        fs::remove_dir_all(tmp).ctx(|| format!("clearing {}", tmp.display()))?;
    }
    fs::create_dir(tmp).ctx(|| format!("creating {}", tmp.display()))?;

    let mut writers = Vec::new();
    for name in FILES {
        let path = tmp.join(name);
        let file = File::create(&path).ctx(|| format!("creating {}", path.display()))?;
        writers.push(CrcWriter::new(file, path));
    }
    let [w_entries, w_vectors, w_payloads, w_offsets] = &mut writers[..] else {
        unreachable!("four files")
    };

    let mut count = 0usize;
    let mut rows = 0u32;
    let mut min_seq = u64::MAX;
    let mut max_seq = 0;
    let mut payload_at = 0u64;
    let mut last_id: Option<PointId> = None;
    w_offsets.put(&0u64.to_le_bytes())?;
    for e in entries {
        assert!(
            last_id < Some(e.id),
            "segment entries must be sorted and unique"
        );
        last_id = Some(e.id);
        min_seq = min_seq.min(e.version);
        max_seq = max_seq.max(e.version);
        let row = match e.value {
            Some((vector, payload)) => {
                assert_eq!(vector.len(), dim, "vector dimension");
                for x in vector {
                    w_vectors.put(&x.to_le_bytes())?;
                }
                let json = serde_json::to_vec(payload).expect("a payload always serialises");
                w_payloads.put(&json)?;
                payload_at += json.len() as u64;
                w_offsets.put(&payload_at.to_le_bytes())?;
                rows = rows
                    .checked_add(1)
                    .filter(|&r| r != NO_ROW)
                    .ok_or_else(|| Error::Corrupt {
                        path: tmp.to_owned(),
                        offset: 0,
                        reason: "a segment holds at most u32::MAX - 1 points".into(),
                    })?;
                rows - 1
            }
            None => NO_ROW,
        };
        let mut buf = [0u8; ENTRY_LEN];
        match e.id {
            PointId::Num(n) => buf[1..9].copy_from_slice(&n.to_le_bytes()),
            PointId::Uuid(u) => {
                buf[0] = 1;
                buf[1..17].copy_from_slice(u.as_bytes());
            }
        }
        buf[17..25].copy_from_slice(&e.version.to_le_bytes());
        buf[25..29].copy_from_slice(&row.to_le_bytes());
        w_entries.put(&buf)?;
        count += 1;
    }

    let mut files = std::collections::BTreeMap::new();
    for w in writers {
        let (name, meta) = w.finish()?;
        files.insert(name, meta);
    }
    let meta = Meta {
        format: FORMAT_VERSION,
        id,
        dim,
        entries: count,
        rows: rows as usize,
        min_seq: if count == 0 { 0 } else { min_seq },
        max_seq,
        files,
    };
    let meta_path = tmp.join("meta.json");
    let mut f = File::create(&meta_path).ctx(|| format!("creating {}", meta_path.display()))?;
    serde_json::to_writer_pretty(&mut f, &meta).expect("meta serialises");
    f.write_all(b"\n")
        .ctx(|| format!("writing {}", meta_path.display()))?;
    sync_file(&f).ctx(|| format!("syncing {}", meta_path.display()))?;
    sync_dir(tmp).ctx(|| format!("syncing {}", tmp.display()))?;

    crash_point("segment.built_not_renamed", || {});
    fs::rename(tmp, dest).ctx(|| format!("renaming {} to {}", tmp.display(), dest.display()))?;
    let parent = dest.parent().expect("segments live in a directory");
    sync_dir(parent).ctx(|| format!("syncing {}", parent.display()))?;
    Ok(())
}

/// Buffers writes to one segment file while computing its CRC32C and length.
struct CrcWriter {
    out: BufWriter<File>,
    path: PathBuf,
    crc: u32,
    len: u64,
}

impl CrcWriter {
    fn new(file: File, path: PathBuf) -> Self {
        CrcWriter {
            out: BufWriter::with_capacity(1 << 20, file),
            path,
            crc: 0,
            len: 0,
        }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.crc = crc32c::crc32c_append(self.crc, bytes);
        self.len += bytes.len() as u64;
        self.out
            .write_all(bytes)
            .ctx(|| format!("writing {}", self.path.display()))
    }

    fn finish(self) -> Result<(String, FileMeta)> {
        let path = self.path;
        let file = self
            .out
            .into_inner()
            .map_err(|e| e.into_error())
            .ctx(|| format!("writing {}", path.display()))?;
        sync_file(&file).ctx(|| format!("syncing {}", path.display()))?;
        let name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        Ok((
            name,
            FileMeta {
                len: self.len,
                crc32c: self.crc,
            },
        ))
    }
}

/// A loaded segment.
///
/// Only the shard opens segments, inside a data directory it owns, which is what makes the
/// memory mapping sound.
pub struct Segment {
    id: u64,
    dir: PathBuf,
    dim: usize,
    entries: Vec<Entry>,
    vectors: Option<Mmap>,
    payloads: Option<Mmap>,
    payload_offsets: Vec<u64>,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("id", &self.id)
            .field("entries", &self.entries.len())
            .field("rows", &self.rows())
            .finish()
    }
}

fn map_file(path: &Path) -> Result<Option<Mmap>> {
    let file = File::open(path).ctx(|| format!("opening {}", path.display()))?;
    let len = file
        .metadata()
        .ctx(|| format!("reading the size of {}", path.display()))?
        .len();
    if len == 0 {
        return Ok(None);
    }
    // SAFETY: segment files are never modified once published, nocturnum only ever creates,
    // renames and deletes them, so the mapping is not changed underneath us by this process.
    // Another process editing files inside the data directory is outside what is supported.
    let map = unsafe { Mmap::map(&file) }.ctx(|| format!("mapping {}", path.display()))?;
    Ok(Some(map))
}

impl Segment {
    /// Loads the segment in `dir`, checking every file against its recorded length and
    /// checksum.
    pub(crate) fn open(dir: &Path, expected_id: u64, dim: usize) -> Result<Segment> {
        let corrupt = |reason: String| Error::Corrupt {
            path: dir.to_owned(),
            offset: 0,
            reason,
        };
        let meta_path = dir.join("meta.json");
        let meta_bytes = fs::read(&meta_path).ctx(|| format!("reading {}", meta_path.display()))?;
        let meta: Meta = serde_json::from_slice(&meta_bytes)
            .map_err(|e| corrupt(format!("meta.json does not parse: {e}")))?;
        if meta.format != FORMAT_VERSION {
            return Err(corrupt(format!(
                "segment format {} is not supported by this build, which reads {FORMAT_VERSION}",
                meta.format
            )));
        }
        if meta.id != expected_id || meta.dim != dim {
            return Err(corrupt(format!(
                "meta.json describes segment {} with dim {}, expected segment {expected_id} with dim {dim}",
                meta.id, meta.dim
            )));
        }

        let mut maps = Vec::new();
        for name in FILES {
            let path = dir.join(name);
            let map = map_file(&path)?;
            let bytes: &[u8] = map.as_deref().unwrap_or(&[]);
            let want = meta
                .files
                .get(name)
                .ok_or_else(|| corrupt(format!("meta.json does not list {name}")))?;
            if bytes.len() as u64 != want.len || crc32c::crc32c(bytes) != want.crc32c {
                return Err(corrupt(format!(
                    "{name} does not match its recorded checksum"
                )));
            }
            maps.push(map);
        }
        let [entries_map, vectors, payloads, offsets_map] =
            <[_; 4]>::try_from(maps).unwrap_or_else(|_| unreachable!("four files"));

        let entry_bytes = entries_map.as_deref().unwrap_or(&[]);
        let offset_bytes = offsets_map.as_deref().unwrap_or(&[]);
        let vector_bytes = vectors.as_deref().unwrap_or(&[]);
        // meta.json carries no checksum of its own, so its counts are checked against the
        // real file sizes without trusting them in any arithmetic that could overflow.
        let sizes_match = |len: usize, count: usize, width: usize| {
            count.checked_mul(width).is_some_and(|want| want == len)
        };
        if !sizes_match(entry_bytes.len(), meta.entries, ENTRY_LEN)
            || !sizes_match(offset_bytes.len(), meta.rows.saturating_add(1), 8)
            || !dim
                .checked_mul(4)
                .is_some_and(|row| sizes_match(vector_bytes.len(), meta.rows, row))
        {
            return Err(corrupt(
                "file sizes do not match the counts in meta.json".into(),
            ));
        }

        let mut entries = Vec::with_capacity(meta.entries);
        let mut next_row = 0u32;
        for chunk in entry_bytes.as_chunks::<ENTRY_LEN>().0 {
            let id = match chunk[0] {
                0 => PointId::Num(u64::from_le_bytes(chunk[1..9].try_into().expect("8"))),
                1 => PointId::Uuid(Uuid::from_bytes(chunk[1..17].try_into().expect("16"))),
                tag => return Err(corrupt(format!("unknown id tag {tag}"))),
            };
            let version = u64::from_le_bytes(chunk[17..25].try_into().expect("8"));
            let row = u32::from_le_bytes(chunk[25..29].try_into().expect("4"));
            let row = if row == NO_ROW {
                None
            } else if row == next_row {
                next_row += 1;
                Some(row)
            } else {
                return Err(corrupt(format!(
                    "entry for {id} has row {row}, expected {next_row}"
                )));
            };
            if entries.last().is_some_and(|e: &Entry| e.id >= id) {
                return Err(corrupt("entries are not sorted by id".into()));
            }
            entries.push(Entry { id, version, row });
        }
        if next_row as usize != meta.rows {
            return Err(corrupt("the entries do not account for every row".into()));
        }

        let payload_offsets: Vec<u64> = offset_bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c))
            .collect();
        let payload_len = payloads.as_ref().map_or(0, |m| m.len() as u64);
        if payload_offsets.windows(2).any(|w| w[0] > w[1])
            || payload_offsets.last() != Some(&payload_len)
            || payload_offsets.first() != Some(&0)
        {
            return Err(corrupt("payload offsets are out of order".into()));
        }

        Ok(Segment {
            id: expected_id,
            dir: dir.to_owned(),
            dim,
            entries,
            vectors,
            payloads,
            payload_offsets,
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Every entry, sorted by id, deletions included.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The number of stored points, which is the number of rows.
    pub fn rows(&self) -> usize {
        self.payload_offsets.len() - 1
    }

    /// All rows' vectors back to back, `rows() * dim` floats.
    pub fn vectors(&self) -> &[f32] {
        match &self.vectors {
            // The length was checked to be whole floats at load, and a mapping starts on a
            // page boundary, so the cast cannot fail.
            Some(map) => bytemuck::cast_slice(&map[..]),
            None => &[],
        }
    }

    pub fn vector(&self, row: u32) -> &[f32] {
        let start = row as usize * self.dim;
        &self.vectors()[start..start + self.dim]
    }

    pub fn payload(&self, row: u32) -> Result<Payload> {
        let r = row as usize;
        let (start, end) = (self.payload_offsets[r], self.payload_offsets[r + 1]);
        let bytes = self
            .payloads
            .as_deref()
            .map_or(&[][..], |m| &m[start as usize..end as usize]);
        serde_json::from_slice(bytes).map_err(|e| Error::Corrupt {
            path: self.dir.join("payloads.bin"),
            offset: start,
            reason: format!("payload of row {row} does not parse: {e}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn payload(v: serde_json::Value) -> Payload {
        serde_json::from_value(v).unwrap()
    }

    fn build(root: &Path, id: u64) -> PathBuf {
        let p1 = payload(json!({"a": 1}));
        let p3 = payload(json!({"b": [true, null]}));
        let v1 = [1.0, 2.0];
        let v3 = [3.0, 4.0];
        let entries = vec![
            EntryIn {
                id: PointId::Num(1),
                version: 10,
                value: Some((&v1, &p1)),
            },
            EntryIn {
                id: PointId::Num(2),
                version: 12,
                value: None,
            },
            EntryIn {
                id: PointId::Uuid(Uuid::nil()),
                version: 11,
                value: Some((&v3, &p3)),
            },
        ];
        let dest = root.join(segment_dir_name(id));
        write_segment(&root.join("tmp"), &dest, id, 2, entries).unwrap();
        dest
    }

    #[test]
    fn round_trips() {
        let root = TempDir::new().unwrap();
        let dir = build(root.path(), 5);
        assert!(!root.path().join("tmp").exists());
        let seg = Segment::open(&dir, 5, 2).unwrap();
        assert_eq!(seg.rows(), 2);
        assert_eq!(
            seg.entries(),
            &[
                Entry {
                    id: PointId::Num(1),
                    version: 10,
                    row: Some(0)
                },
                Entry {
                    id: PointId::Num(2),
                    version: 12,
                    row: None
                },
                Entry {
                    id: PointId::Uuid(Uuid::nil()),
                    version: 11,
                    row: Some(1)
                },
            ]
        );
        assert_eq!(seg.vector(1), &[3.0, 4.0]);
        assert_eq!(seg.vectors(), &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(seg.payload(0).unwrap(), payload(json!({"a": 1})));
        assert_eq!(seg.payload(1).unwrap(), payload(json!({"b": [true, null]})));
    }

    #[test]
    fn an_empty_segment_works() {
        let root = TempDir::new().unwrap();
        let dest = root.path().join(segment_dir_name(1));
        write_segment(&root.path().join("tmp"), &dest, 1, 3, []).unwrap();
        let seg = Segment::open(&dest, 1, 3).unwrap();
        assert_eq!((seg.rows(), seg.entries().len()), (0, 0));
        assert!(seg.vectors().is_empty());
    }

    #[test]
    fn absurd_counts_in_meta_are_errors_not_panics() {
        let root = TempDir::new().unwrap();
        let dir = build(root.path(), 5);
        let path = dir.join("meta.json");
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for (field, value) in [
            ("entries", json!(usize::MAX)),
            ("entries", json!(3_816_567_739_388_183_093u64)),
            ("rows", json!(usize::MAX)),
            ("rows", json!(usize::MAX / 4)),
        ] {
            let mut meta = original.clone();
            meta[field] = value;
            fs::write(&path, serde_json::to_vec(&meta).unwrap()).unwrap();
            assert!(Segment::open(&dir, 5, 2).is_err(), "{field}");
        }
        let huge_dim = Segment::open(&dir, 5, usize::MAX);
        assert!(huge_dim.is_err());
    }

    #[test]
    fn any_damaged_byte_is_caught() {
        let root = TempDir::new().unwrap();
        let dir = build(root.path(), 5);
        for name in FILES {
            let path = dir.join(name);
            let original = fs::read(&path).unwrap();
            for i in 0..original.len() {
                let mut bytes = original.clone();
                bytes[i] ^= 0x01;
                fs::write(&path, &bytes).unwrap();
                assert!(Segment::open(&dir, 5, 2).is_err(), "{name} byte {i}");
            }
            fs::write(&path, &original).unwrap();
        }
        assert!(Segment::open(&dir, 5, 2).is_ok());
        assert!(Segment::open(&dir, 6, 2).is_err());
        assert!(Segment::open(&dir, 5, 3).is_err());
    }
}
