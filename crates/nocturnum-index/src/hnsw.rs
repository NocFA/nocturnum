//! A hierarchical navigable small world graph over one segment's vectors, after Malkov and
//! Yashunin's HNSW paper.
//!
//! # Structure
//!
//! Every row of the segment is a node. Each node gets a level `L` drawn once, and appears on
//! layers `0..=L`, linked on each layer to up to `m` neighbours, or `2m` on layer 0. Upper
//! layers hold fewer and fewer nodes, so a search can cross the space in a few long hops at
//! the top and then refine on the full layer 0.
//!
//! A node's level is `floor(-ln(U) / ln(m))`, capped at [`MAX_LEVEL`], where `U` in (0, 1]
//! comes from hashing the node's row with [`HnswConfig::seed`], so the same rows and seed give
//! the same levels on every build.
//!
//! # Building
//!
//! Nodes are inserted one at a time: a greedy walk down the upper layers finds a start, a
//! beam search of width `ef_construct` on each of the node's layers finds candidates, and the
//! diversity heuristic picks the links. A candidate is linked only when it is closer to the
//! new node than to every neighbour already picked, which spreads links across directions,
//! and the slots it leaves free are filled with the nearest rejected candidates. Neighbours
//! link back, trimming their own lists with the same heuristic when they overflow.
//!
//! Trimming links can leave a node with no way in, so a last pass links every node a search
//! from the entry cannot reach from its nearest reachable node.
//!
//! The first nodes go in one by one, then the rest in parallel, each node's links behind its
//! own lock, so a parallel build is not repeatable bit for bit, while [`Hnsw::build_serial`]
//! is.
//!
//! # Searching
//!
//! A search walks greedily down the upper layers and then runs a beam search of width
//! `max(ef, k)` on layer 0, from the node it reached and from the entry, since every node is
//! reachable from the entry. With a filter, rows that fail it are still walked through, so
//! the search can cross them, but never returned, and the filter is only asked about rows
//! that score well enough to enter the results.
//!
//! Scores are the engine's internal ones, higher is better, see
//! [`nocturnum_core::Distance::score`].

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::sync::{Mutex, RwLock};

use nocturnum_core::Distance;
use rayon::prelude::*;

use crate::flat::{Scored, TopK};

mod file;

pub use file::{GraphFileError, HEADER_LEN, max_file_len};

/// The highest level a node can reach.
pub const MAX_LEVEL: u8 = 16;

/// The largest `m` a graph can have, far above any useful setting, which bounds graph files.
pub const MAX_M: usize = 128;

/// How many nodes a parallel build inserts one by one before going wide, so the first
/// parallel inserts already find a graph to search.
const SERIAL_SEED: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswConfig {
    /// Links per node on the upper layers, layer 0 allows twice as many.
    pub m: usize,
    /// The beam width while building, higher builds a better graph more slowly.
    pub ef_construct: usize,
    /// Seeds the node levels.
    pub seed: u64,
}

impl Default for HnswConfig {
    fn default() -> Self {
        HnswConfig {
            m: 16,
            ef_construct: 100,
            seed: 0x6E6F_6374_7572_6E75,
        }
    }
}

impl HnswConfig {
    pub fn m0(&self) -> usize {
        self.m * 2
    }

    /// The level of node `row`.
    pub fn level(&self, row: u32) -> u8 {
        let bits = mix(self.seed ^ u64::from(row));
        // 53 random bits make U in (0, 1], so ln(U) is finite.
        let u = ((bits >> 11) + 1) as f64 / (1u64 << 53) as f64;
        let level = (-u.ln() / (self.m.max(2) as f64).ln()).floor();
        (level as u64).min(u64::from(MAX_LEVEL)) as u8
    }
}

/// splitmix64's finaliser.
fn mix(i: u64) -> u64 {
    let mut z = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A finished graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Hnsw {
    pub(crate) config: HnswConfig,
    pub(crate) dim: usize,
    pub(crate) distance: Distance,
    pub(crate) levels: Vec<u8>,
    /// Layer 0 links of node `i` are `layer0[offsets[i]..offsets[i + 1]]`.
    pub(crate) offsets: Vec<u32>,
    pub(crate) layer0: Vec<u32>,
    /// Links on layer `l + 1` by node, for nodes on that layer.
    pub(crate) upper: Vec<HashMap<u32, Box<[u32]>>>,
    /// The node search starts from, on the top layer, `None` for an empty graph.
    pub(crate) entry: Option<u32>,
}

/// A node and its score against the query, ordered so a greater value is a better result:
/// higher score, then the smaller `tie`.
///
/// Searches break ties by row, so results are repeatable. Building breaks them by a hash of
/// the pair instead, because ordering equal candidates by row would send every duplicate's
/// links to the same few low rows and strand the rest.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Near {
    score: f32,
    node: u32,
    tie: u64,
}

impl Near {
    fn by_row(score: f32, node: u32) -> Near {
        Near {
            score,
            node,
            tie: u64::from(node),
        }
    }

    fn hashed(score: f32, node: u32, base: u32) -> Near {
        Near {
            score,
            node,
            tie: mix((u64::from(base) << 32) | u64::from(node)),
        }
    }
}

impl Eq for Near {}

impl Ord for Near {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .partial_cmp(&other.score)
            .expect("scores are ranked, never NaN")
            .then_with(|| other.tie.cmp(&self.tie))
    }
}

impl PartialOrd for Near {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// NaN, which only an overflowing dot product produces, ranks as the worst score.
fn rank(score: f32) -> f32 {
    if score.is_nan() {
        f32::NEG_INFINITY
    } else {
        score
    }
}

/// Which nodes a search has reached.
struct Visited(Vec<u64>);

impl Visited {
    fn new(n: usize) -> Visited {
        Visited(vec![0; n.div_ceil(64)])
    }

    /// Marks `node`, returning whether it was new.
    fn insert(&mut self, node: u32) -> bool {
        let (word, bit) = (node as usize / 64, 1u64 << (node % 64));
        let new = self.0[word] & bit == 0;
        self.0[word] |= bit;
        new
    }
}

/// Read access to links, shared by the builder and the finished graph.
trait Links {
    fn with_links<T>(&self, node: u32, layer: u8, f: impl FnOnce(&[u32]) -> T) -> T;
}

struct Space<'a> {
    distance: Distance,
    vectors: &'a [f32],
    dim: usize,
}

impl Space<'_> {
    fn vector(&self, node: u32) -> &[f32] {
        let start = node as usize * self.dim;
        &self.vectors[start..start + self.dim]
    }

    fn score(&self, query: &[f32], node: u32) -> f32 {
        rank(self.distance.score(query, self.vector(node)))
    }

    fn between(&self, a: u32, b: u32) -> f32 {
        rank(self.distance.score(self.vector(a), self.vector(b)))
    }
}

/// How a search builds its [`Near`] values from a score and a node.
type MakeNear<'a> = &'a dyn Fn(f32, u32) -> Near;

/// What a walk over the graph needs: the vectors, the links and how to order ties.
struct Walk<'a, L> {
    space: &'a Space<'a>,
    links: &'a L,
    near: MakeNear<'a>,
}

/// Walks greedily on `layer` from `start` to the node nearest `query` it can reach.
fn greedy<L: Links>(walk: &Walk<'_, L>, query: &[f32], start: Near, layer: u8) -> Near {
    let Walk { space, links, near } = walk;
    let mut best = start;
    loop {
        let mut moved = false;
        let neighbours = links.with_links(best.node, layer, |l| l.to_vec());
        for n in neighbours {
            let candidate = near(space.score(query, n), n);
            if candidate > best {
                best = candidate;
                moved = true;
            }
        }
        if !moved {
            return best;
        }
    }
}

/// A beam search of width `ef` on `layer` from `entry`, returning up to `ef` nodes that pass
/// `allow`, best first. Nodes failing `allow` are still expanded. `allow` is asked only
/// about nodes good enough to enter the results.
fn beam<L: Links, E>(
    walk: &Walk<'_, L>,
    query: &[f32],
    entry: &[Near],
    ef: usize,
    layer: u8,
    visited: &mut Visited,
    mut allow: impl FnMut(u32) -> Result<bool, E>,
) -> Result<Vec<Near>, E> {
    let Walk { space, links, near } = walk;
    let mut candidates: BinaryHeap<Near> = BinaryHeap::new();
    let mut results: BinaryHeap<Reverse<Near>> = BinaryHeap::new();
    let worst = |r: &BinaryHeap<Reverse<Near>>| r.peek().map(|w| w.0);
    let mut offer = |n: Near,
                     candidates: &mut BinaryHeap<Near>,
                     results: &mut BinaryHeap<Reverse<Near>>|
     -> Result<(), E> {
        let full = results.len() >= ef;
        if full && worst(results).is_some_and(|w| n <= w) {
            return Ok(());
        }
        candidates.push(n);
        if allow(n.node)? {
            results.push(Reverse(n));
            if results.len() > ef {
                results.pop();
            }
        }
        Ok(())
    };
    for &e in entry {
        if visited.insert(e.node) {
            offer(e, &mut candidates, &mut results)?;
        }
    }
    while let Some(c) = candidates.pop() {
        if results.len() >= ef && worst(&results).is_some_and(|w| c < w) {
            break;
        }
        let neighbours = links.with_links(c.node, layer, |l| l.to_vec());
        for n in neighbours {
            if visited.insert(n) {
                offer(
                    near(space.score(query, n), n),
                    &mut candidates,
                    &mut results,
                )?;
            }
        }
    }
    let mut out: Vec<Near> = results.into_iter().map(|r| r.0).collect();
    out.sort_by(|a, b| b.cmp(a));
    Ok(out)
}

/// Picks up to `m` links for `base` from `candidates`, sorted best first by their score
/// against `base`: a candidate is taken when it is closer to `base` than to every one already
/// taken, and free slots go to the nearest rejected ones.
fn select(space: &Space<'_>, candidates: &[Near], m: usize) -> Vec<u32> {
    let mut picked: Vec<u32> = Vec::with_capacity(m);
    let mut rejected = Vec::new();
    for c in candidates {
        if picked.len() == m {
            break;
        }
        if picked.iter().all(|&p| space.between(c.node, p) < c.score) {
            picked.push(c.node);
        } else {
            rejected.push(c.node);
        }
    }
    for r in rejected {
        if picked.len() == m {
            break;
        }
        picked.push(r);
    }
    picked
}

/// The graph while it is built, every node's links on every layer behind its own lock.
struct Builder<'a> {
    space: Space<'a>,
    config: HnswConfig,
    levels: Vec<u8>,
    links: Vec<Vec<RwLock<Vec<u32>>>>,
    /// The entry node and the top layer.
    entry: Mutex<Option<(u32, u8)>>,
}

impl Links for Builder<'_> {
    fn with_links<T>(&self, node: u32, layer: u8, f: impl FnOnce(&[u32]) -> T) -> T {
        let lock = &self.links[node as usize][layer as usize];
        f(&lock.read().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Builder<'_> {
    fn limit(&self, layer: u8) -> usize {
        if layer == 0 {
            self.config.m0()
        } else {
            self.config.m
        }
    }

    fn insert(&self, node: u32) {
        let level = self.levels[node as usize];
        let start = *self.entry.lock().unwrap_or_else(|e| e.into_inner());
        let Some((entry, top)) = start else {
            let mut slot = self.entry.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some((node, level));
                return;
            }
            drop(slot);
            return self.insert(node);
        };

        let query = self.space.vector(node);
        let make = |score, n| Near::hashed(score, n, node);
        let walk = Walk {
            space: &self.space,
            links: self,
            near: &make,
        };
        let mut near = make(self.space.score(query, entry), entry);
        for layer in (level.saturating_add(1)..=top).rev() {
            near = greedy(&walk, query, near, layer);
        }
        let mut entries = vec![near];
        for layer in (0..=level.min(top)).rev() {
            let mut visited = Visited::new(self.levels.len());
            let found = beam(
                &walk,
                query,
                &entries,
                self.config.ef_construct,
                layer,
                &mut visited,
                |n| Ok::<_, ()>(n != node),
            )
            .expect("never fails");
            let chosen = select(&self.space, &found, self.limit(layer));
            *self.links[node as usize][layer as usize]
                .write()
                .unwrap_or_else(|e| e.into_inner()) = chosen.clone();
            for n in chosen {
                self.link_back(n, node, layer);
            }
            if !found.is_empty() {
                entries = found;
            }
        }

        if level > top {
            let mut slot = self.entry.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_some_and(|(_, t)| level > t) {
                *slot = Some((node, level));
            }
        }
    }

    /// Adds `to` to `from`'s links on `layer`, trimming them with the heuristic if they
    /// overflow.
    fn link_back(&self, from: u32, to: u32, layer: u8) {
        let mut links = self.links[from as usize][layer as usize]
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if links.contains(&to) {
            return;
        }
        links.push(to);
        let limit = self.limit(layer);
        if links.len() > limit {
            let mut scored: Vec<Near> = links
                .iter()
                .map(|&n| Near::hashed(self.space.between(from, n), n, from))
                .collect();
            scored.sort_by(|a, b| b.cmp(a));
            *links = select(&self.space, &scored, limit);
        }
    }

    /// Links every node no search could reach on layer 0 from its nearest reachable node,
    /// since trimming links can leave a node with no way in. Such a link may take its node
    /// past `2m` links, which is the price of every row staying findable.
    fn repair(&self) {
        let n = self.levels.len();
        let Some((entry, top)) = *self.entry.lock().unwrap_or_else(|e| e.into_inner()) else {
            return;
        };
        let mut reached = vec![false; n];
        let reach_from = |start: u32, reached: &mut Vec<bool>| {
            let mut stack = vec![start];
            reached[start as usize] = true;
            while let Some(node) = stack.pop() {
                self.with_links(node, 0, |links| {
                    for &l in links {
                        if !std::mem::replace(&mut reached[l as usize], true) {
                            stack.push(l);
                        }
                    }
                });
            }
        };
        reach_from(entry, &mut reached);
        for node in 0..n as u32 {
            if reached[node as usize] {
                continue;
            }
            let query = self.space.vector(node);
            let make = |score, n| Near::hashed(score, n, node);
            let walk = Walk {
                space: &self.space,
                links: self,
                near: &make,
            };
            let mut near = make(self.space.score(query, entry), entry);
            for layer in (1..=top).rev() {
                near = greedy(&walk, query, near, layer);
            }
            let mut visited = Visited::new(n);
            let found = beam(
                &walk,
                query,
                &[near],
                self.config.ef_construct,
                0,
                &mut visited,
                |r| Ok::<_, ()>(r != node && reached[r as usize]),
            )
            .expect("never fails");
            let from = found.first().map_or(entry, |f| f.node);
            self.links[from as usize][0]
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .push(node);
            reach_from(node, &mut reached);
        }
    }

    fn finish(self) -> Hnsw {
        self.repair();
        let n = self.levels.len();
        let mut offsets = Vec::with_capacity(n + 1);
        let mut layer0 = Vec::new();
        offsets.push(0u32);
        let top = self.levels.iter().copied().max().unwrap_or(0);
        let mut upper: Vec<HashMap<u32, Box<[u32]>>> = vec![HashMap::new(); top as usize];
        for (node, layers) in self.links.into_iter().enumerate() {
            for (layer, lock) in layers.into_iter().enumerate() {
                let links = lock.into_inner().unwrap_or_else(|e| e.into_inner());
                if layer == 0 {
                    layer0.extend_from_slice(&links);
                    offsets.push(u32::try_from(layer0.len()).expect("links fit in u32"));
                } else {
                    upper[layer - 1].insert(node as u32, links.into_boxed_slice());
                }
            }
        }
        Hnsw {
            config: self.config,
            dim: self.space.dim,
            distance: self.space.distance,
            levels: self.levels,
            offsets,
            layer0,
            upper,
            entry: self
                .entry
                .into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .map(|(e, _)| e),
        }
    }
}

impl Links for Hnsw {
    fn with_links<T>(&self, node: u32, layer: u8, f: impl FnOnce(&[u32]) -> T) -> T {
        if layer == 0 {
            let i = node as usize;
            f(&self.layer0[self.offsets[i] as usize..self.offsets[i + 1] as usize])
        } else {
            f(self.upper[layer as usize - 1]
                .get(&node)
                .map_or(&[], |l| &l[..]))
        }
    }
}

impl Hnsw {
    /// Builds a graph over `vectors`, `dim` floats per row, inserting in parallel after the
    /// first few hundred rows.
    pub fn build(distance: Distance, vectors: &[f32], dim: usize, config: HnswConfig) -> Hnsw {
        let builder = Self::builder(distance, vectors, dim, config);
        let n = builder.levels.len() as u32;
        let seed = (SERIAL_SEED as u32).min(n);
        (0..seed).for_each(|node| builder.insert(node));
        (seed..n)
            .into_par_iter()
            .for_each(|node| builder.insert(node));
        builder.finish()
    }

    /// Builds the same graph as [`Hnsw::build`] would, one row at a time, so the result is
    /// the same on every run.
    pub fn build_serial(
        distance: Distance,
        vectors: &[f32],
        dim: usize,
        config: HnswConfig,
    ) -> Hnsw {
        let builder = Self::builder(distance, vectors, dim, config);
        (0..builder.levels.len() as u32).for_each(|node| builder.insert(node));
        builder.finish()
    }

    fn builder(distance: Distance, vectors: &[f32], dim: usize, config: HnswConfig) -> Builder<'_> {
        assert!(dim > 0, "vectors have at least one dimension");
        assert!(
            (2..=MAX_M).contains(&config.m),
            "m is between 2 and {MAX_M}"
        );
        assert_eq!(
            vectors.len() % dim,
            0,
            "vectors are not a whole number of rows"
        );
        let n = vectors.len() / dim;
        let n32 = u32::try_from(n).expect("a segment holds fewer than 2^32 rows");
        let levels: Vec<u8> = (0..n32).map(|row| config.level(row)).collect();
        let links = levels
            .iter()
            .map(|&l| (0..=l).map(|_| RwLock::new(Vec::new())).collect())
            .collect();
        Builder {
            space: Space {
                distance,
                vectors,
                dim,
            },
            config,
            levels,
            links,
            entry: Mutex::new(None),
        }
    }

    pub fn config(&self) -> HnswConfig {
        self.config
    }

    pub fn len(&self) -> usize {
        self.levels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    /// Roughly the heap memory the graph takes: levels, layer 0 links and offsets, and the
    /// upper layers' links, leaving out allocator and hash table overhead.
    pub fn heap_bytes(&self) -> u64 {
        let u32s = (self.offsets.len() + self.layer0.len()) as u64;
        let upper: u64 = self
            .upper
            .iter()
            .flat_map(|layer| layer.values())
            .map(|links| 4 + 4 * links.len() as u64)
            .sum();
        self.levels.len() as u64 + 4 * u32s + upper
    }

    /// The best `k` rows for `query` that `allow` accepts, best first, searching layer 0 with
    /// a beam of `max(ef, k)`. `vectors` must be the rows the graph was built over.
    pub fn search<E>(
        &self,
        vectors: &[f32],
        query: &[f32],
        k: usize,
        ef: usize,
        allow: impl FnMut(u32) -> Result<bool, E>,
    ) -> Result<Vec<Scored<u32>>, E> {
        assert_eq!(query.len(), self.dim, "the query has the wrong dimension");
        assert_eq!(
            vectors.len(),
            self.levels.len() * self.dim,
            "the vectors are not the ones the graph was built over"
        );
        let Some(entry) = self.entry else {
            return Ok(Vec::new());
        };
        if k == 0 {
            return Ok(Vec::new());
        }
        let space = Space {
            distance: self.distance,
            vectors,
            dim: self.dim,
        };
        let make = |score, n| Near::by_row(score, n);
        let walk = Walk {
            space: &space,
            links: self,
            near: &make,
        };
        let mut near = make(space.score(query, entry), entry);
        for layer in (1..=self.levels[entry as usize]).rev() {
            near = greedy(&walk, query, near, layer);
        }
        // Layer 0 links run one way, and the repair pass only promises every node can be
        // reached from the entry, so the entry seeds the beam beside the node the descent found.
        let mut seeds = vec![near];
        if near.node != entry {
            seeds.push(make(space.score(query, entry), entry));
        }
        let mut visited = Visited::new(self.levels.len());
        let found = beam(&walk, query, &seeds, ef.max(k), 0, &mut visited, allow)?;
        let mut top = TopK::new(k);
        for n in found {
            top.push(n.node, n.score);
        }
        Ok(top.into_sorted_vec())
    }
}

#[cfg(test)]
mod tests;
