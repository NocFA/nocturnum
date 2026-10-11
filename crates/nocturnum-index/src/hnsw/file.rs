//! The graph's file format, specified in `docs/specs/hnsw.md`.
//!
//! Everything is little-endian. A file holds a fixed header, every node's level, layer 0 as
//! offsets and links, each upper layer as its nodes with their links, and a CRC32C of all of
//! that at the end. Reading checks the checksum and then every structural rule a search
//! relies on, so a damaged or hand-made file is refused instead of sending a search out of
//! bounds.

use std::collections::HashMap;
use std::fmt;

use nocturnum_core::Distance;

use super::{Hnsw, HnswConfig, MAX_LEVEL, MAX_M};

const MAGIC: &[u8; 8] = b"nochnsw1";
const VERSION: u32 = 1;
/// The fixed header's length, which [`max_file_len`] needs.
pub const HEADER_LEN: usize = 48;
const NO_ENTRY: u32 = u32::MAX;

/// Why a graph file was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFileError(pub String);

impl fmt::Display for GraphFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid graph file: {}", self.0)
    }
}

impl std::error::Error for GraphFileError {}

fn distance_code(d: Distance) -> u8 {
    match d {
        Distance::Cosine => 0,
        Distance::Dot => 1,
        Distance::Euclid => 2,
        Distance::Manhattan => 3,
    }
}

fn u32_of(n: usize) -> u32 {
    u32::try_from(n).expect("graph sizes fit in u32")
}

impl Hnsw {
    /// The graph in its file format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let put32 = |out: &mut Vec<u8>, x: u32| out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(MAGIC);
        put32(&mut out, VERSION);
        out.extend_from_slice(&[distance_code(self.distance), 0, 0, 0]);
        put32(&mut out, u32_of(self.dim));
        put32(&mut out, u32_of(self.config.m));
        put32(&mut out, u32_of(self.config.ef_construct));
        out.extend_from_slice(&self.config.seed.to_le_bytes());
        put32(&mut out, u32_of(self.levels.len()));
        put32(&mut out, self.entry.unwrap_or(NO_ENTRY));
        put32(&mut out, u32_of(self.upper.len()));
        debug_assert_eq!(out.len(), HEADER_LEN);

        out.extend_from_slice(&self.levels);
        for &o in &self.offsets {
            put32(&mut out, o);
        }
        for &l in &self.layer0 {
            put32(&mut out, l);
        }
        for layer in &self.upper {
            let mut nodes: Vec<_> = layer.keys().copied().collect();
            nodes.sort_unstable();
            put32(&mut out, u32_of(nodes.len()));
            for node in nodes {
                let links = &layer[&node];
                put32(&mut out, node);
                put32(&mut out, u32_of(links.len()));
                for &l in links.iter() {
                    put32(&mut out, l);
                }
            }
        }
        let crc = crc32c::crc32c(&out);
        put32(&mut out, crc);
        out
    }

    /// Reads a graph written by [`Hnsw::to_bytes`], checking it can be searched safely over
    /// `rows` vectors of `dim` floats compared by `distance`.
    pub fn from_bytes(
        bytes: &[u8],
        distance: Distance,
        dim: usize,
        rows: usize,
    ) -> Result<Hnsw, GraphFileError> {
        let fail = |reason: String| Err(GraphFileError(reason));
        let Some((body, crc)) = bytes.split_last_chunk::<4>() else {
            return fail("the file is too short".into());
        };
        if crc32c::crc32c(body) != u32::from_le_bytes(*crc) {
            return fail("the checksum does not match".into());
        }
        let mut r = Reader { bytes: body, at: 0 };
        if r.take(8)? != MAGIC {
            return fail("the magic number is wrong".into());
        }
        let version = r.u32()?;
        if version != VERSION {
            return fail(format!(
                "version {version} is not supported, this build reads {VERSION}"
            ));
        }
        let code = r.take(4)?[0];
        if code != distance_code(distance) {
            return fail(format!(
                "built for distance code {code}, the segment uses {distance:?}"
            ));
        }
        let file_dim = r.u32()? as usize;
        let m = r.u32()? as usize;
        let ef_construct = r.u32()? as usize;
        let seed = u64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes"));
        let n = r.u32()? as usize;
        let entry = r.u32()?;
        let upper_layers = r.u32()? as usize;
        if file_dim != dim || n != rows {
            return fail(format!(
                "built over {n} rows of {file_dim} dimensions, the segment has {rows} of {dim}"
            ));
        }
        if !(2..=MAX_M).contains(&m) || upper_layers > MAX_LEVEL as usize {
            return fail(format!(
                "m {m} or {upper_layers} upper layers is out of range"
            ));
        }

        let levels = r.take(n)?.to_vec();
        let top = levels.iter().copied().max().unwrap_or(0) as usize;
        if levels.iter().any(|&l| l > MAX_LEVEL) || top != upper_layers {
            return fail("node levels do not match the layer count".into());
        }
        let entry = match (n, entry) {
            (0, NO_ENTRY) => None,
            (0, _) => return fail("an empty graph has an entry".into()),
            (_, e) if (e as usize) < n && levels[e as usize] as usize == top => Some(e),
            _ => return fail("the entry is not a node on the top layer".into()),
        };

        let offsets = r.u32s(n + 1)?;
        if offsets[0] != 0 || offsets.windows(2).any(|w| w[0] > w[1]) {
            return fail("layer 0 offsets are not increasing from 0".into());
        }
        // Each node holds at most 2m links plus one the repair pass may add to it per
        // stranded node, so the total is at most n * (2m + 1), checked before allocating.
        let total = *offsets.last().expect("n + 1 offsets") as usize;
        if n.checked_mul(2 * m + 1).is_none_or(|cap| total > cap) {
            return fail(format!(
                "layer 0 holds {total} links, more than {n} nodes can"
            ));
        }
        let layer0 = r.u32s(total)?;
        let mut seen = vec![false; n];
        for node in 0..n {
            let links = &layer0[offsets[node] as usize..offsets[node + 1] as usize];
            if links.iter().any(|&l| l as usize >= n || l as usize == node) {
                return fail(format!(
                    "node {node} has a layer 0 link out of range or to itself"
                ));
            }
            if !distinct(links, &mut seen) {
                return fail(format!(
                    "node {node} links to the same node twice on layer 0"
                ));
            }
        }

        let mut upper = Vec::with_capacity(upper_layers);
        for layer in 1..=upper_layers {
            let count = r.u32()? as usize;
            let expected = levels.iter().filter(|&&l| l as usize >= layer).count();
            if count != expected {
                return fail(format!(
                    "layer {layer} lists {count} nodes, {expected} reach it"
                ));
            }
            // Every record takes at least 8 bytes, so a count the file cannot hold is refused
            // before anything is reserved for it.
            if count
                .checked_mul(8)
                .is_none_or(|need| need > body.len() - r.at)
            {
                return fail(format!(
                    "layer {layer} lists more nodes than the file holds"
                ));
            }
            let mut map = HashMap::with_capacity(count);
            let mut last = None;
            for _ in 0..count {
                let node = r.u32()?;
                let len = r.u32()? as usize;
                if len > m {
                    return fail(format!(
                        "node {node} has {len} links on layer {layer}, over m"
                    ));
                }
                let links = r.u32s(len)?;
                let reaches = |x: u32| (x as usize) < n && levels[x as usize] as usize >= layer;
                if last.is_some_and(|p| p >= node) || !reaches(node) {
                    return fail(format!(
                        "layer {layer} lists node {node} out of order or level"
                    ));
                }
                if links.iter().any(|&l| !reaches(l) || l == node) {
                    return fail(format!("node {node} links off layer {layer}"));
                }
                if !distinct(&links, &mut seen) {
                    return fail(format!(
                        "node {node} links to the same node twice on layer {layer}"
                    ));
                }
                last = Some(node);
                map.insert(node, links.into_boxed_slice());
            }
            upper.push(map);
        }
        if r.at != body.len() {
            return fail("there are bytes after the last layer".into());
        }
        Ok(Hnsw {
            config: HnswConfig {
                m,
                ef_construct,
                seed,
            },
            dim,
            distance,
            levels,
            offsets,
            layer0,
            upper,
            entry,
        })
    }
}

/// Whether `links` names no node twice, using `seen`, all false, as scratch space.
fn distinct(links: &[u32], seen: &mut [bool]) -> bool {
    let mut ok = true;
    for &l in links {
        ok &= !std::mem::replace(&mut seen[l as usize], true);
    }
    for &l in links {
        seen[l as usize] = false;
    }
    ok
}

/// The largest graph file a segment of `rows` rows can have, worked out from the file's first
/// [`HEADER_LEN`] bytes, so a reader can refuse an oversized file before reading it.
///
/// Every node may sit on every layer, so the bound is loose, about `rows * (72m + 132)` bytes,
/// while staying proportional to the segment.
pub fn max_file_len(header: &[u8], rows: usize) -> Result<u64, GraphFileError> {
    let fail = |reason: &str| GraphFileError(reason.into());
    let m = header
        .get(20..24)
        .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) as u64)
        .ok_or_else(|| fail("the file is shorter than its header"))?;
    if !(2..=MAX_M as u64).contains(&m) {
        return Err(fail("m is out of range"));
    }
    let n = rows as u64;
    let levels = u64::from(MAX_LEVEL);
    let parts = [
        Some(HEADER_LEN as u64 + 4),
        Some(n),
        n.checked_add(1).and_then(|x| x.checked_mul(4)),
        n.checked_mul(4 * (2 * m + 1)),
        Some(4 * levels),
        n.checked_mul(levels * (8 + 4 * m)),
    ];
    parts
        .into_iter()
        .try_fold(0u64, |acc, p| acc.checked_add(p?))
        .ok_or_else(|| fail("the size bound overflows"))
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], GraphFileError> {
        let end = self
            .at
            .checked_add(len)
            .filter(|&e| e <= self.bytes.len())
            .ok_or_else(|| GraphFileError("the file ends early".into()))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, GraphFileError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u32s(&mut self, count: usize) -> Result<Vec<u32>, GraphFileError> {
        let len = count
            .checked_mul(4)
            .ok_or_else(|| GraphFileError("a length overflows".into()))?;
        Ok(self
            .take(len)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> (Hnsw, Vec<f32>) {
        let vectors: Vec<f32> = (0..400 * 6)
            .map(|i| ((i * 7919) % 1000) as f32 / 500.0 - 1.0)
            .collect();
        let g = Hnsw::build_serial(
            Distance::Euclid,
            &vectors,
            6,
            HnswConfig {
                m: 4,
                ..HnswConfig::default()
            },
        );
        (g, vectors)
    }

    fn read(bytes: &[u8]) -> Result<Hnsw, GraphFileError> {
        Hnsw::from_bytes(bytes, Distance::Euclid, 6, 400)
    }

    /// Recomputes the checksum, so a test can reach the structural checks behind it.
    fn reseal(mut body: Vec<u8>) -> Vec<u8> {
        body.truncate(body.len() - 4);
        let crc = crc32c::crc32c(&body);
        body.extend_from_slice(&crc.to_le_bytes());
        body
    }

    #[test]
    fn round_trips() {
        let (g, vectors) = graph();
        let back = read(&g.to_bytes()).unwrap();
        assert_eq!(back, g);
        let q = [0.1; 6];
        let ok = |_| Ok::<_, ()>(true);
        assert_eq!(
            back.search(&vectors, &q, 5, 32, ok),
            g.search(&vectors, &q, 5, 32, ok)
        );
        let empty = Hnsw::build(Distance::Dot, &[], 3, HnswConfig::default());
        assert_eq!(
            Hnsw::from_bytes(&empty.to_bytes(), Distance::Dot, 3, 0).unwrap(),
            empty
        );
    }

    #[test]
    fn any_damage_is_refused() {
        let (g, _) = graph();
        let bytes = g.to_bytes();
        for i in (0..bytes.len()).step_by(3) {
            let mut b = bytes.clone();
            b[i] ^= 0x10;
            assert!(read(&b).is_err(), "a flip at byte {i} was accepted");
        }
        for len in [0, 3, 40, bytes.len() / 2, bytes.len() - 1] {
            assert!(read(&bytes[..len]).is_err(), "a cut at {len} was accepted");
        }
    }

    #[test]
    fn a_graph_for_other_vectors_is_refused() {
        let (g, _) = graph();
        let bytes = g.to_bytes();
        assert!(Hnsw::from_bytes(&bytes, Distance::Dot, 6, 400).is_err());
        assert!(Hnsw::from_bytes(&bytes, Distance::Euclid, 5, 400).is_err());
        assert!(Hnsw::from_bytes(&bytes, Distance::Euclid, 6, 401).is_err());
    }

    #[test]
    fn structural_damage_behind_a_valid_checksum_is_refused() {
        let (g, _) = graph();
        let bytes = g.to_bytes();
        let n = g.len();
        let layer0_start = HEADER_LEN + n + (n + 1) * 4;
        type Damage = Box<dyn Fn(&mut Vec<u8>)>;
        let cases: Vec<(&str, Damage)> = vec![
            (
                "a link past the last row",
                Box::new(move |b: &mut Vec<u8>| {
                    b[layer0_start..layer0_start + 4].copy_from_slice(&(n as u32).to_le_bytes())
                }),
            ),
            (
                "a node level above the top",
                Box::new(|b: &mut Vec<u8>| b[HEADER_LEN] = MAX_LEVEL + 1),
            ),
            (
                "an entry off the top layer",
                Box::new(|b: &mut Vec<u8>| b[40..44].copy_from_slice(&0u32.to_le_bytes())),
            ),
            (
                "trailing bytes",
                Box::new(|b: &mut Vec<u8>| {
                    let at = b.len() - 4;
                    b.insert(at, 0)
                }),
            ),
        ];
        for (name, damage) in cases {
            let mut b = bytes.clone();
            damage(&mut b);
            let b = reseal(b);
            // Node 0 may happen to be the entry, which makes that case valid.
            if name == "an entry off the top layer" && g.entry == Some(0) {
                continue;
            }
            assert!(read(&b).is_err(), "{name} was accepted");
        }
    }

    #[test]
    fn oversized_link_lists_and_counts_are_refused_before_allocating() {
        let (g, _) = graph();
        // Two nodes with a huge layer 0 list: the offsets alone claim far more links than two
        // nodes can hold.
        let two = Hnsw::build_serial(Distance::Euclid, &[0.0, 1.0], 1, HnswConfig::default());
        let mut b = two.to_bytes();
        let offsets_at = HEADER_LEN + 2;
        b[offsets_at + 4..offsets_at + 8].copy_from_slice(&(1u32 << 30).to_le_bytes());
        b[offsets_at + 8..offsets_at + 12].copy_from_slice(&((1u32 << 30) + 1).to_le_bytes());
        let err = Hnsw::from_bytes(&reseal(b), Distance::Euclid, 1, 2).unwrap_err();
        assert!(err.0.contains("more than"), "{err}");

        // A repeated link is refused too.
        let mut b = g.to_bytes();
        let n = g.len();
        let links_at = HEADER_LEN + n + (n + 1) * 4;
        let (first, second) = (g.offsets[0] as usize, g.offsets[1] as usize);
        assert!(second - first >= 2);
        let dup: [u8; 4] = b[links_at..links_at + 4].try_into().unwrap();
        b[links_at + 4..links_at + 8].copy_from_slice(&dup);
        assert!(read(&reseal(b)).unwrap_err().0.contains("twice"));

        let header = &g.to_bytes()[..HEADER_LEN];
        let bound = max_file_len(header, n).unwrap();
        assert!(g.to_bytes().len() as u64 <= bound);
        assert!(max_file_len(&header[..10], n).is_err());
    }
}
