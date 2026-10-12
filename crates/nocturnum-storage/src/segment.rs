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
//! | `graph.bin` | the HNSW graph, only in segments with at least [`GraphOptions::min_bytes`] of vectors, see `docs/specs/hnsw.md` |
//! | `sparse.bin` | each live point's sparse vector as (`u32` dimension, `f32` weight) pairs in dimension order, back to back, only in segments holding a sparse vector |
//! | `sparse_offsets.bin` | `rows + 1` little-endian `u64` pair offsets into `sparse.bin`, beside it |
//! | `meta.json` | format version, counts, the range of WAL sequence numbers and every file's length and CRC32C |
//!
//! A segment with sparse vectors is format 2, so a build that predates them refuses it rather
//! than losing them, and a segment without any is written as format 1, as before.
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
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use memmap2::Mmap;
use nocturnum_core::sparse::MAX_SPARSE_LEN;
use nocturnum_core::{Payload, PointId, SparseVector, VectorConfig};
use nocturnum_index::hnsw::{HEADER_LEN, max_file_len};
use nocturnum_index::{Hnsw, HnswConfig};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crash::crash_point;
use crate::error::{Error, IoContext, Result};
use crate::sync::{read_bounded, sync_dir, sync_file};

/// The format of a segment without sparse vectors.
const FORMAT_VERSION: u32 = 1;
/// The format of a segment with sparse vectors.
const FORMAT_SPARSE: u32 = 2;
const ENTRY_LEN: usize = 29;
const NO_ROW: u32 = u32::MAX;
/// The optional graph index, present when the segment was big enough, see `docs/specs/hnsw.md`.
const GRAPH_FILE: &str = "graph.bin";
const SPARSE_FILE: &str = "sparse.bin";
const SPARSE_OFFSETS_FILE: &str = "sparse_offsets.bin";
/// A sparse pair on disk: a `u32` dimension and an `f32` weight.
const PAIR_LEN: usize = 8;
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
    pub value: Option<(&'a [f32], PayloadIn<'a>)>,
    /// The sparse vector, if the point has one, always `None` for a deletion.
    pub sparse: Option<SparseIn<'a>>,
}

/// A sparse vector handed to [`write_segment`].
#[derive(Debug, Clone, Copy)]
pub enum SparseIn<'a> {
    Value(&'a SparseVector),
    /// Already encoded as pairs, as another segment stores it, so a merge copies it as is.
    Pairs(&'a [u8]),
}

/// A payload handed to [`write_segment`].
#[derive(Debug, Clone, Copy)]
pub enum PayloadIn<'a> {
    Value(&'a Payload),
    /// Already encoded, as another segment stores it, so a merge copies it without parsing.
    Json(&'a [u8]),
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

/// When a new segment gets a graph index, and how it is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphOptions {
    pub config: HnswConfig,
    /// Segments whose vectors take at least this many bytes get a graph, smaller ones are
    /// cheap enough to scan.
    pub min_bytes: usize,
}

impl Default for GraphOptions {
    fn default() -> Self {
        GraphOptions {
            config: HnswConfig::default(),
            min_bytes: 10 * 1024 * 1024,
        }
    }
}

pub fn segment_dir_name(id: u64) -> String {
    format!("seg-{id:016x}")
}

/// Counters a segment build updates as it runs, for progress reports.
///
/// Every update is a release and a reader loads with acquire in the reverse order of the
/// updates, so what it reads is never ahead of what came before it: rows inserted never pass
/// the rows the graph holds.
#[derive(Default)]
pub(crate) struct BuildProgress {
    /// Rows the graph will hold, 0 until building it starts, and for a segment without one.
    pub(crate) graph_rows: AtomicU64,
    /// Rows inserted into the graph so far.
    pub(crate) graph_done: AtomicU64,
    /// Called at fixed moments of a build, so a test can look at the counters mid-merge.
    #[cfg(test)]
    pub(crate) checkpoint: Option<crate::shard::Checkpoint>,
}

/// Writes a segment from entries sorted by id, building it in `tmp` and renaming it to
/// `dest` once everything is on disk.
pub(crate) fn write_segment<'a>(
    tmp: &Path,
    dest: &Path,
    id: u64,
    vectors: VectorConfig,
    graph: Option<&GraphOptions>,
    entries: impl IntoIterator<Item = EntryIn<'a>>,
) -> Result<()> {
    let progress = BuildProgress::default();
    write_segment_counted(tmp, dest, id, vectors, graph, entries, &progress)
}

/// Like [`write_segment`], reporting how far the graph has got in `progress`.
pub(crate) fn write_segment_counted<'a>(
    tmp: &Path,
    dest: &Path,
    id: u64,
    vectors: VectorConfig,
    graph: Option<&GraphOptions>,
    entries: impl IntoIterator<Item = EntryIn<'a>>,
    progress: &BuildProgress,
) -> Result<()> {
    let dim = vectors.dim;
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
    // Sparse vectors are written once the segment is known to hold one, so a segment without
    // any keeps the old format, and until then only their offsets, all zero, are counted.
    let mut sparse: Option<(CrcWriter, CrcWriter)> = None;
    let mut sparse_pairs = 0u64;
    // Kept in memory for building the graph, when the segment may get one.
    let mut all_vectors: Option<Vec<f32>> = graph.map(|_| Vec::new());
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
                if let Some(all) = &mut all_vectors {
                    all.extend_from_slice(vector);
                }
                let encoded;
                let json: &[u8] = match payload {
                    PayloadIn::Value(p) => {
                        encoded = serde_json::to_vec(p).expect("a payload always serialises");
                        &encoded
                    }
                    PayloadIn::Json(bytes) => bytes,
                };
                w_payloads.put(json)?;
                payload_at += json.len() as u64;
                w_offsets.put(&payload_at.to_le_bytes())?;
                if let Some(v) = e.sparse {
                    if sparse.is_none() {
                        sparse = Some(open_sparse(tmp, rows)?);
                    }
                    let (pairs, _) = sparse.as_mut().expect("opened above");
                    match v {
                        SparseIn::Value(v) => {
                            for (i, x) in v.indices.iter().zip(&v.values) {
                                pairs.put(&i.to_le_bytes())?;
                                pairs.put(&x.to_le_bytes())?;
                            }
                            sparse_pairs += v.indices.len() as u64;
                        }
                        SparseIn::Pairs(bytes) => {
                            assert_eq!(bytes.len() % PAIR_LEN, 0, "whole sparse pairs");
                            pairs.put(bytes)?;
                            sparse_pairs += (bytes.len() / PAIR_LEN) as u64;
                        }
                    }
                }
                if let Some((_, offsets)) = &mut sparse {
                    offsets.put(&sparse_pairs.to_le_bytes())?;
                }
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
            None => {
                assert!(e.sparse.is_none(), "a deletion has no sparse vector");
                NO_ROW
            }
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
    let format = match sparse {
        None => FORMAT_VERSION,
        Some((pairs, offsets)) => {
            for w in [pairs, offsets] {
                let (name, meta) = w.finish()?;
                files.insert(name, meta);
            }
            FORMAT_SPARSE
        }
    };
    if let (Some(g), Some(all)) = (graph, all_vectors)
        && all.len().saturating_mul(4) >= g.min_bytes
    {
        progress
            .graph_rows
            .store((all.len() / dim) as u64, Ordering::Release);
        #[cfg(test)]
        if let Some(checkpoint) = &progress.checkpoint {
            checkpoint("indexing");
        }
        let hnsw = Hnsw::build_counted(vectors.distance, &all, dim, g.config, &progress.graph_done);
        let path = tmp.join(GRAPH_FILE);
        let file = File::create(&path).ctx(|| format!("creating {}", path.display()))?;
        let mut w = CrcWriter::new(file, path);
        w.put(&hnsw.to_bytes())?;
        let (name, meta) = w.finish()?;
        files.insert(name, meta);
    }
    let meta = Meta {
        format,
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

/// Creates the sparse files once row `row`, counting from 0, turns out to hold the segment's
/// first sparse vector, with the offsets so far, all 0: the start, then the end of every row
/// before it.
fn open_sparse(tmp: &Path, row: u32) -> Result<(CrcWriter, CrcWriter)> {
    let mut out = Vec::new();
    for name in [SPARSE_FILE, SPARSE_OFFSETS_FILE] {
        let path = tmp.join(name);
        let file = File::create(&path).ctx(|| format!("creating {}", path.display()))?;
        out.push(CrcWriter::new(file, path));
    }
    let mut offsets = out.pop().expect("two files");
    let pairs = out.pop().expect("two files");
    for _ in 0..=row {
        offsets.put(&0u64.to_le_bytes())?;
    }
    Ok((pairs, offsets))
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
    /// The sparse pairs and each row's pair offsets, when the segment holds sparse vectors.
    sparse: Option<(Option<Mmap>, Vec<u64>)>,
    graph: Option<Hnsw>,
    disk_bytes: u64,
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
    pub(crate) fn open(
        dir: &Path,
        expected_id: u64,
        vectors_config: VectorConfig,
    ) -> Result<Segment> {
        let dim = vectors_config.dim;
        let corrupt = |reason: String| Error::Corrupt {
            path: dir.to_owned(),
            offset: 0,
            reason,
        };
        let meta_path = dir.join("meta.json");
        // A few hundred bytes in practice, bounded so a hostile one cannot exhaust memory.
        let meta_bytes = read_bounded(&meta_path, 1024 * 1024)
            .ctx(|| format!("reading {}", meta_path.display()))?;
        let meta: Meta = serde_json::from_slice(&meta_bytes)
            .map_err(|e| corrupt(format!("meta.json does not parse: {e}")))?;
        if meta.format != FORMAT_VERSION && meta.format != FORMAT_SPARSE {
            return Err(corrupt(format!(
                "segment format {} is not supported by this build, which reads \
                 {FORMAT_VERSION} and {FORMAT_SPARSE}",
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

        let graph = match meta.files.get(GRAPH_FILE) {
            None => None,
            Some(want) => {
                let path = dir.join(GRAPH_FILE);
                // The size is checked against what the segment's rows allow before the file is
                // read, so a damaged length cannot make opening read gigabytes.
                let mut file = File::open(&path).ctx(|| format!("opening {}", path.display()))?;
                let len = file
                    .metadata()
                    .ctx(|| format!("reading the size of {}", path.display()))?
                    .len();
                let mut header = [0u8; HEADER_LEN];
                file.read_exact(&mut header)
                    .ctx(|| format!("reading {}", path.display()))?;
                let limit = max_file_len(&header, meta.rows).map_err(|e| corrupt(e.to_string()))?;
                if len != want.len || len > limit {
                    return Err(corrupt(format!(
                        "{GRAPH_FILE} is {len} bytes, more than {} rows allow or meta.json records",
                        meta.rows
                    )));
                }
                let mut bytes = header.to_vec();
                file.read_to_end(&mut bytes)
                    .ctx(|| format!("reading {}", path.display()))?;
                if bytes.len() as u64 != want.len || crc32c::crc32c(&bytes) != want.crc32c {
                    return Err(corrupt(format!(
                        "{GRAPH_FILE} does not match its recorded checksum"
                    )));
                }
                let graph = Hnsw::from_bytes(&bytes, vectors_config.distance, dim, meta.rows)
                    .map_err(|e| corrupt(e.to_string()))?;
                Some(graph)
            }
        };
        let sparse = if meta.format == FORMAT_SPARSE {
            Some(open_sparse_files(dir, &meta, &corrupt)?)
        } else {
            None
        };
        let known = |k: &str| {
            FILES.contains(&k)
                || k == GRAPH_FILE
                || (sparse.is_some() && (k == SPARSE_FILE || k == SPARSE_OFFSETS_FILE))
        };
        if let Some(extra) = meta.files.keys().find(|k| !known(k)) {
            return Err(corrupt(format!(
                "meta.json lists {extra}, which this build does not know"
            )));
        }

        Ok(Segment {
            graph,
            disk_bytes: meta.files.values().map(|f| f.len).sum(),
            id: expected_id,
            dir: dir.to_owned(),
            dim,
            entries,
            vectors,
            payloads,
            payload_offsets,
            sparse,
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// The segment's graph index, if it was big enough to get one.
    pub fn graph(&self) -> Option<&Hnsw> {
        self.graph.as_ref()
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

    /// The payload of `row` as stored, JSON.
    pub fn payload_bytes(&self, row: u32) -> &[u8] {
        let r = row as usize;
        let (start, end) = (self.payload_offsets[r], self.payload_offsets[r + 1]);
        self.payloads
            .as_deref()
            .map_or(&[][..], |m| &m[start as usize..end as usize])
    }

    /// The sparse vector of `row` as stored, (dimension, weight) pairs in dimension order,
    /// empty when it has none.
    pub fn sparse_pairs(&self, row: u32) -> &[u8] {
        let Some((pairs, offsets)) = &self.sparse else {
            return &[];
        };
        let r = row as usize;
        let (start, end) = (offsets[r] as usize, offsets[r + 1] as usize);
        pairs
            .as_deref()
            .map_or(&[][..], |m| &m[start * PAIR_LEN..end * PAIR_LEN])
    }

    /// The sparse vector of `row`, if it has one.
    pub fn sparse(&self, row: u32) -> Option<SparseVector> {
        let pairs = self.sparse_pairs(row);
        if pairs.is_empty() {
            return None;
        }
        let (indices, values) = pairs
            .as_chunks::<PAIR_LEN>()
            .0
            .iter()
            .map(|p| {
                (
                    u32::from_le_bytes(p[..4].try_into().expect("4")),
                    f32::from_le_bytes(p[4..].try_into().expect("4")),
                )
            })
            .unzip();
        Some(SparseVector { indices, values })
    }

    /// Whether any row holds a sparse vector.
    pub fn has_sparse(&self) -> bool {
        self.sparse.is_some()
    }

    /// The bytes the segment's files take, as `meta.json` records them.
    pub fn disk_bytes(&self) -> u64 {
        self.disk_bytes
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

/// Loads and checks a format 2 segment's sparse files: pairs in dimension order, each
/// dimension once per row, finite weights and offsets that cover the pairs exactly.
fn open_sparse_files(
    dir: &Path,
    meta: &Meta,
    corrupt: &dyn Fn(String) -> Error,
) -> Result<(Option<Mmap>, Vec<u64>)> {
    // The largest each file can be for this many rows, checked against its size on disk
    // before it is mapped or read, so a damaged one cannot make loading read gigabytes.
    let rows = meta.rows as u64;
    let largest = [
        rows.saturating_mul((MAX_SPARSE_LEN * PAIR_LEN) as u64),
        rows.saturating_add(1).saturating_mul(8),
    ];
    let mut maps = Vec::new();
    for (name, largest) in [SPARSE_FILE, SPARSE_OFFSETS_FILE].into_iter().zip(largest) {
        let want = meta
            .files
            .get(name)
            .ok_or_else(|| corrupt(format!("meta.json does not list {name}")))?;
        let path = dir.join(name);
        let len = fs::metadata(&path)
            .ctx(|| format!("reading the size of {}", path.display()))?
            .len();
        if len != want.len || len > largest {
            return Err(corrupt(format!(
                "{name} is {len} bytes, more than {} rows allow or meta.json records",
                meta.rows
            )));
        }
        let map = map_file(&path)?;
        let bytes: &[u8] = map.as_deref().unwrap_or(&[]);
        if bytes.len() as u64 != want.len || crc32c::crc32c(bytes) != want.crc32c {
            return Err(corrupt(format!(
                "{name} does not match its recorded checksum"
            )));
        }
        maps.push(map);
    }
    let offsets_map = maps.pop().expect("two files");
    let pairs = maps.pop().expect("two files");
    let offset_bytes = offsets_map.as_deref().unwrap_or(&[]);
    let pair_bytes = pairs.as_deref().unwrap_or(&[]);
    if offset_bytes.len() as u64 != largest[1] || pair_bytes.len() % PAIR_LEN != 0 {
        return Err(corrupt(
            "sparse file sizes do not match the counts in meta.json".into(),
        ));
    }
    let offsets: Vec<u64> = offset_bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect();
    let total = (pair_bytes.len() / PAIR_LEN) as u64;
    if offsets.first() != Some(&0)
        || offsets.last() != Some(&total)
        || offsets.windows(2).any(|w| w[0] > w[1])
    {
        return Err(corrupt("sparse offsets are out of order".into()));
    }
    let all = pair_bytes.as_chunks::<PAIR_LEN>().0;
    for w in offsets.windows(2) {
        if w[1] - w[0] > MAX_SPARSE_LEN as u64 {
            return Err(corrupt(format!(
                "a sparse vector holds {} dimensions, more than {MAX_SPARSE_LEN}",
                w[1] - w[0]
            )));
        }
        let row = &all[w[0] as usize..w[1] as usize];
        let dims = row
            .iter()
            .map(|p| u32::from_le_bytes(p[..4].try_into().expect("4")));
        if !dims.clone().zip(dims.skip(1)).all(|(a, b)| a < b)
            || row
                .iter()
                .any(|p| !f32::from_le_bytes(p[4..].try_into().expect("4")).is_finite())
        {
            return Err(corrupt(
                "a sparse vector is out of order or holds a non-finite weight".into(),
            ));
        }
    }
    Ok((pairs, offsets))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn vc(dim: usize) -> VectorConfig {
        VectorConfig {
            dim,
            distance: nocturnum_core::Distance::Dot,
        }
    }

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
                value: Some((&v1, PayloadIn::Value(&p1))),
                sparse: None,
            },
            EntryIn {
                id: PointId::Num(2),
                version: 12,
                value: None,
                sparse: None,
            },
            EntryIn {
                id: PointId::Uuid(Uuid::nil()),
                version: 11,
                value: Some((&v3, PayloadIn::Value(&p3))),
                sparse: None,
            },
        ];
        let dest = root.join(segment_dir_name(id));
        write_segment(&root.join("tmp"), &dest, id, vc(2), None, entries).unwrap();
        dest
    }

    #[test]
    fn round_trips() {
        let root = TempDir::new().unwrap();
        let dir = build(root.path(), 5);
        assert!(!root.path().join("tmp").exists());
        let seg = Segment::open(&dir, 5, vc(2)).unwrap();
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
        write_segment(&root.path().join("tmp"), &dest, 1, vc(3), None, []).unwrap();
        let seg = Segment::open(&dest, 1, vc(3)).unwrap();
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
            assert!(Segment::open(&dir, 5, vc(2)).is_err(), "{field}");
        }
        let huge_dim = Segment::open(&dir, 5, vc(usize::MAX));
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
                assert!(Segment::open(&dir, 5, vc(2)).is_err(), "{name} byte {i}");
            }
            fs::write(&path, &original).unwrap();
        }
        assert!(Segment::open(&dir, 5, vc(2)).is_ok());
        assert!(Segment::open(&dir, 6, vc(2)).is_err());
        assert!(Segment::open(&dir, 5, vc(3)).is_err());
    }

    #[test]
    fn big_enough_segments_get_a_checked_graph() {
        let root = TempDir::new().unwrap();
        let config = VectorConfig {
            dim: 4,
            distance: nocturnum_core::Distance::Euclid,
        };
        let vectors: Vec<Vec<f32>> = (0..300)
            .map(|i| vec![i as f32, 0.0, 1.0, (i % 7) as f32])
            .collect();
        let p = payload(json!({}));
        let entries = vectors.iter().enumerate().map(|(i, v)| EntryIn {
            id: PointId::Num(i as u64),
            version: 1,
            value: Some((v.as_slice(), PayloadIn::Value(&p))),
            sparse: None,
        });
        let options = GraphOptions {
            min_bytes: 0,
            ..GraphOptions::default()
        };
        let dest = root.path().join(segment_dir_name(1));
        write_segment(
            &root.path().join("tmp"),
            &dest,
            1,
            config,
            Some(&options),
            entries,
        )
        .unwrap();
        let seg = Segment::open(&dest, 1, config).unwrap();
        let g = seg.graph().expect("a graph was built");
        let got = g
            .search(seg.vectors(), &[42.2, 0.0, 1.0, 0.0], 1, 32, |_| {
                Ok::<_, ()>(true)
            })
            .unwrap();
        assert_eq!(got[0].key, 42);

        // Too small for the threshold, no graph.
        let small = GraphOptions::default();
        let dest2 = root.path().join(segment_dir_name(2));
        let few = vectors.iter().take(3).enumerate().map(|(i, v)| EntryIn {
            id: PointId::Num(i as u64),
            version: 1,
            value: Some((v.as_slice(), PayloadIn::Value(&p))),
            sparse: None,
        });
        write_segment(
            &root.path().join("tmp"),
            &dest2,
            2,
            config,
            Some(&small),
            few,
        )
        .unwrap();
        assert!(Segment::open(&dest2, 2, config).unwrap().graph().is_none());

        // A damaged graph fails the segment, and a graph for another metric is refused.
        let path = dest.join(GRAPH_FILE);
        let original = fs::read(&path).unwrap();
        let mut bad = original.clone();
        bad[60] ^= 1;
        fs::write(&path, &bad).unwrap();
        assert!(Segment::open(&dest, 1, config).is_err());
        fs::write(&path, &original).unwrap();
        let dot = VectorConfig {
            distance: nocturnum_core::Distance::Dot,
            ..config
        };
        assert!(Segment::open(&dest, 1, dot).is_err());
    }

    #[test]
    fn sparse_vectors_round_trip_and_mark_the_format() {
        let root = TempDir::new().unwrap();
        let p = payload(json!({}));
        let a = SparseVector {
            indices: vec![2, 9, 70_000],
            values: vec![0.5, -1.0, 3.0],
        };
        let b = SparseVector {
            indices: vec![9],
            values: vec![2.0],
        };
        let v = [0.0f32, 1.0];
        // A deletion, a row without a sparse vector before the first one with, one with, a
        // row without after, and one copied as stored pairs.
        let pairs: Vec<u8> = b
            .indices
            .iter()
            .zip(&b.values)
            .flat_map(|(i, x)| [i.to_le_bytes(), x.to_le_bytes()].concat())
            .collect();
        let entries = vec![
            EntryIn {
                id: PointId::Num(1),
                version: 1,
                value: None,
                sparse: None,
            },
            EntryIn {
                id: PointId::Num(2),
                version: 2,
                value: Some((&v, PayloadIn::Value(&p))),
                sparse: None,
            },
            EntryIn {
                id: PointId::Num(3),
                version: 3,
                value: Some((&v, PayloadIn::Value(&p))),
                sparse: Some(SparseIn::Value(&a)),
            },
            EntryIn {
                id: PointId::Num(4),
                version: 4,
                value: Some((&v, PayloadIn::Value(&p))),
                sparse: None,
            },
            EntryIn {
                id: PointId::Num(5),
                version: 5,
                value: Some((&v, PayloadIn::Value(&p))),
                sparse: Some(SparseIn::Pairs(&pairs)),
            },
        ];
        let dest = root.path().join(segment_dir_name(1));
        write_segment(&root.path().join("tmp"), &dest, 1, vc(2), None, entries).unwrap();
        let meta: Meta =
            serde_json::from_slice(&fs::read(dest.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.format, FORMAT_SPARSE);
        let seg = Segment::open(&dest, 1, vc(2)).unwrap();
        assert!(seg.has_sparse());
        let got: Vec<_> = (0..4).map(|r| seg.sparse(r)).collect();
        assert_eq!(got, [None, Some(a), None, Some(b)]);

        // Without sparse vectors a segment keeps format 1 and the old file set.
        let plain = build(root.path(), 2);
        let meta: Meta =
            serde_json::from_slice(&fs::read(plain.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.format, FORMAT_VERSION);
        assert!(!plain.join(SPARSE_FILE).exists());
        assert!(!Segment::open(&plain, 2, vc(2)).unwrap().has_sparse());
    }

    #[test]
    fn damaged_sparse_files_are_refused() {
        let root = TempDir::new().unwrap();
        let p = payload(json!({}));
        let a = SparseVector {
            indices: vec![2, 9],
            values: vec![0.5, -1.0],
        };
        let v = [0.0f32, 1.0];
        let write = |id: u64, sparse: &SparseVector| {
            let entries = vec![EntryIn {
                id: PointId::Num(1),
                version: 1,
                value: Some((&v, PayloadIn::Value(&p))),
                sparse: Some(SparseIn::Value(sparse)),
            }];
            let dest = root.path().join(segment_dir_name(id));
            write_segment(&root.path().join("tmp"), &dest, id, vc(2), None, entries).unwrap();
            dest
        };
        let dest = write(1, &a);
        for name in [SPARSE_FILE, SPARSE_OFFSETS_FILE] {
            let path = dest.join(name);
            let original = fs::read(&path).unwrap();
            for i in 0..original.len() {
                let mut bytes = original.clone();
                bytes[i] ^= 0x01;
                fs::write(&path, &bytes).unwrap();
                assert!(Segment::open(&dest, 1, vc(2)).is_err(), "{name} byte {i}");
            }
            fs::write(&path, &original).unwrap();
        }
        assert!(Segment::open(&dest, 1, vc(2)).is_ok());

        // Files whose checksums match but whose contents break the rules are refused too.
        let rewrite = |dest: &Path, name: &str, bytes: Vec<u8>| {
            fs::write(dest.join(name), &bytes).unwrap();
            let meta_path = dest.join("meta.json");
            let mut meta: Meta = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
            meta.files.insert(
                name.into(),
                FileMeta {
                    len: bytes.len() as u64,
                    crc32c: crc32c::crc32c(&bytes),
                },
            );
            fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        };
        let mut swapped = fs::read(dest.join(SPARSE_FILE)).unwrap();
        swapped.rotate_left(PAIR_LEN);
        rewrite(&dest, SPARSE_FILE, swapped);
        assert!(matches!(
            Segment::open(&dest, 1, vc(2)),
            Err(Error::Corrupt { .. })
        ));

        let long = SparseVector {
            indices: (0..=MAX_SPARSE_LEN as u32).collect(),
            values: vec![1.0; MAX_SPARSE_LEN + 1],
        };
        let dest = write(2, &long);
        assert!(matches!(
            Segment::open(&dest, 2, vc(2)),
            Err(Error::Corrupt { .. })
        ));
    }

    #[test]
    fn many_rows_of_small_sparse_vectors_load() {
        let root = TempDir::new().unwrap();
        let p = payload(json!({}));
        let v = [0.0f32, 1.0];
        let sparse: Vec<SparseVector> = (0..9000u32)
            .map(|i| SparseVector {
                indices: vec![i],
                values: vec![1.0],
            })
            .collect();
        let entries = sparse.iter().enumerate().map(|(i, s)| EntryIn {
            id: PointId::Num(i as u64),
            version: 1,
            value: Some((&v, PayloadIn::Value(&p))),
            sparse: Some(SparseIn::Value(s)),
        });
        let dest = root.path().join(segment_dir_name(1));
        write_segment(&root.path().join("tmp"), &dest, 1, vc(2), None, entries).unwrap();
        let seg = Segment::open(&dest, 1, vc(2)).unwrap();
        assert_eq!(seg.sparse(8999), Some(sparse[8999].clone()));
    }
}
