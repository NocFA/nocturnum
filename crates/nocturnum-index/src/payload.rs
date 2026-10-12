//! Payload indexes for one immutable segment, and filter evaluation over them.
//!
//! Indexes are opt-in per field and per [`PayloadIndexKind`], and every indexed field also
//! records how many non-null values each row holds and which rows hold a `null`, which answers
//! `is_empty`, `is_null`, `values_count` and the presence half of `except`.
//!
//! They are derived data: built in memory from a segment's payloads when the segment is
//! opened or written, never logged or synced, so they add nothing to crash recovery.
//!
//! # Bounds
//!
//! [`PayloadIndex::bounds`] turns a filter into two sets of rows, the rows that surely match
//! and the rows that may match. Conditions the indexes can answer, which is most conditions on
//! indexed fields, give exact sets, and anything else (unindexed fields, a value type the field
//! has no index for, `nested`) is unknown: surely nothing, maybe everything. Rows between the
//! two sets need the reference evaluator, [`nocturnum_core::Filter::matches`], which defines
//! what every filter means and which the indexed answer must always agree with.

use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};

use nocturnum_core::filter::{FieldCondition, MinShould, field_values};
use nocturnum_core::{Condition, Filter, JsonPath, MatchValue, Payload, PayloadIndexKind, PointId};
use roaring::RoaringBitmap;
use serde_json::Value;

use crate::text::TextIndex;

/// What a filter can match, from the indexes alone. `sure` is always a subset of `maybe`.
#[derive(Debug, Clone, PartialEq)]
pub struct Bounds {
    /// Rows that match.
    pub sure: RoaringBitmap,
    /// Rows that may match, every matching row is in here.
    pub maybe: RoaringBitmap,
}

impl Bounds {
    fn exact(rows: RoaringBitmap) -> Bounds {
        Bounds {
            sure: rows.clone(),
            maybe: rows,
        }
    }

    fn unknown(universe: &RoaringBitmap) -> Bounds {
        Bounds {
            sure: RoaringBitmap::new(),
            maybe: universe.clone(),
        }
    }

    /// Whether the indexes settled every row, so no payload has to be read.
    pub fn is_exact(&self) -> bool {
        self.sure.len() == self.maybe.len()
    }

    /// Rows only the reference evaluator can settle.
    pub fn uncertain(&self) -> RoaringBitmap {
        &self.maybe - &self.sure
    }

    /// Keeps only rows in `rows`, such as the rows still live in the segment.
    pub fn restrict(&mut self, rows: &RoaringBitmap) {
        self.sure &= rows;
        self.maybe &= rows;
    }

    fn and(&mut self, other: &Bounds) {
        self.sure &= &other.sure;
        self.maybe &= &other.maybe;
    }

    fn or(&mut self, other: &Bounds) {
        self.sure |= &other.sure;
        self.maybe |= &other.maybe;
    }

    fn not(self, universe: &RoaringBitmap) -> Bounds {
        Bounds {
            sure: universe - &self.maybe,
            maybe: universe - &self.sure,
        }
    }
}

/// The payload indexes of one segment, over rows `0..rows`.
pub struct PayloadIndex {
    universe: RoaringBitmap,
    ids: HashMap<PointId, u32>,
    fields: HashMap<JsonPath, FieldIndex>,
}

#[derive(Default)]
struct FieldIndex {
    /// Rows by their number of non-null values, for counts above zero.
    counts: BTreeMap<u32, RoaringBitmap>,
    /// Rows with at least one non-null value.
    has_values: RoaringBitmap,
    /// Rows with a `null` among their values.
    has_null: RoaringBitmap,
    keyword: Option<HashMap<String, RoaringBitmap>>,
    integer: Option<IntegerIndex>,
    /// Every numeric value with its row, sorted by value, present with either numeric kind.
    numbers: Option<Vec<(f64, u32)>>,
    /// Rows holding `false`, then rows holding `true`.
    bools: Option<[RoaringBitmap; 2]>,
    /// Words in the field's strings, for ranking rather than filtering.
    text: Option<TextIndex>,
}

#[derive(Default)]
struct IntegerIndex {
    /// Numbers stored as integers that fit in an i64.
    ints: HashMap<i64, RoaringBitmap>,
    /// Numbers stored as floats with no fractional part, by their bits with -0.0 folded into
    /// 0.0, since the evaluator lets the integer `5` match a stored `5.0`.
    whole_floats: HashMap<u64, RoaringBitmap>,
}

fn float_key(f: f64) -> u64 {
    // -0.0 == 0.0, so both get the bits of 0.0.
    if f == 0.0 { 0 } else { f.to_bits() }
}

impl PayloadIndex {
    /// Builds the indexes listed in `schema` over a segment of `rows` rows, from each row's
    /// point id and, only when `schema` names a field, its payload, read through `payload`.
    /// A field may appear with several kinds.
    ///
    /// Panics if a row is not below `rows`.
    pub fn build<P: Borrow<Payload>, E>(
        rows: u32,
        schema: &[(JsonPath, PayloadIndexKind)],
        points: impl IntoIterator<Item = (u32, PointId)>,
        mut payload: impl FnMut(u32) -> Result<P, E>,
    ) -> Result<PayloadIndex, E> {
        let mut fields: HashMap<JsonPath, FieldIndex> = HashMap::new();
        for (path, kind) in schema {
            let f = fields.entry(path.clone()).or_default();
            match kind {
                PayloadIndexKind::Keyword => {
                    f.keyword.get_or_insert_default();
                }
                PayloadIndexKind::Integer => {
                    f.integer.get_or_insert_default();
                    f.numbers.get_or_insert_default();
                }
                PayloadIndexKind::Float => {
                    f.numbers.get_or_insert_default();
                }
                PayloadIndexKind::Bool => {
                    f.bools.get_or_insert_default();
                }
                PayloadIndexKind::Text => {
                    f.text.get_or_insert_default();
                }
            }
        }

        let mut ids = HashMap::new();
        for (row, id) in points {
            assert!(row < rows, "row {row} is outside a segment of {rows} rows");
            ids.insert(id, row);
            if !fields.is_empty() {
                let p = payload(row)?;
                for (path, f) in &mut fields {
                    f.add(row, &field_values(path, &p.borrow().0));
                }
            }
        }
        for f in fields.values_mut() {
            if let Some(numbers) = &mut f.numbers {
                numbers.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            }
        }

        let mut universe = RoaringBitmap::new();
        universe.insert_range(0..rows);
        Ok(PayloadIndex {
            universe,
            ids,
            fields,
        })
    }

    /// Every row, matching or not.
    pub fn universe(&self) -> &RoaringBitmap {
        &self.universe
    }

    /// The text index on `path`, when the field has one.
    pub fn text(&self, path: &JsonPath) -> Option<&TextIndex> {
        self.fields.get(path)?.text.as_ref()
    }

    /// The rows `filter` surely matches and the rows it may match.
    pub fn bounds(&self, filter: &Filter) -> Bounds {
        let mut acc = Bounds::exact(self.universe.clone());
        for c in &filter.must {
            acc.and(&self.condition(c));
            if acc.maybe.is_empty() {
                return acc;
            }
        }
        if !filter.should.is_empty() {
            acc.and(&self.any_of(&filter.should));
        }
        if !filter.must_not.is_empty() {
            acc.and(&self.any_of(&filter.must_not).not(&self.universe));
        }
        if let Some(ms) = &filter.min_should {
            acc.and(&self.at_least(ms));
        }
        acc
    }

    /// The rows `filter` matches, asking `check` only about rows the indexes cannot settle.
    pub fn matching_rows<E>(
        &self,
        filter: &Filter,
        mut check: impl FnMut(u32) -> Result<bool, E>,
    ) -> Result<RoaringBitmap, E> {
        let bounds = self.bounds(filter);
        let mut rows = bounds.sure.clone();
        for row in bounds.uncertain() {
            if check(row)? {
                rows.insert(row);
            }
        }
        Ok(rows)
    }

    fn any_of(&self, conditions: &[Condition]) -> Bounds {
        let mut acc = Bounds::exact(RoaringBitmap::new());
        for c in conditions {
            acc.or(&self.condition(c));
        }
        acc
    }

    /// Rows where at least `min_count` conditions surely or maybe hold, counted per row.
    fn at_least(&self, ms: &MinShould) -> Bounds {
        let n = self.universe.len() as usize;
        let mut sure_counts = vec![0u32; n];
        let mut maybe_counts = vec![0u32; n];
        for c in &ms.conditions {
            let b = self.condition(c);
            b.sure.iter().for_each(|r| sure_counts[r as usize] += 1);
            b.maybe.iter().for_each(|r| maybe_counts[r as usize] += 1);
        }
        let k = u32::try_from(ms.min_count).unwrap_or(u32::MAX);
        let pick = |counts: &[u32]| {
            (0..n as u32)
                .filter(|&r| counts[r as usize] >= k)
                .collect::<RoaringBitmap>()
        };
        Bounds {
            sure: pick(&sure_counts),
            maybe: pick(&maybe_counts),
        }
    }

    fn condition(&self, c: &Condition) -> Bounds {
        match c {
            Condition::Field(f) => self.field(f),
            Condition::HasId(h) => Bounds::exact(
                h.has_id
                    .iter()
                    .filter_map(|id| self.ids.get(id).copied())
                    .collect(),
            ),
            // Paths inside are relative to each object, which no index covers.
            Condition::Nested(_) => Bounds::unknown(&self.universe),
            Condition::Filter(f) => self.bounds(f),
        }
    }

    fn field(&self, c: &FieldCondition) -> Bounds {
        let Some(f) = self.fields.get(&c.key) else {
            return Bounds::unknown(&self.universe);
        };
        let u = &self.universe;
        // Every predicate in one condition must hold, so known answers intersect and any
        // unknown one leaves only an upper bound.
        let answers: [Option<Option<RoaringBitmap>>; 7] = [
            c.match_value.as_ref().map(|m| f.lookup(m)),
            c.any.as_ref().map(|list| f.lookup_any(list)),
            c.except
                .as_ref()
                .map(|list| f.lookup_any(list).map(|hit| &f.has_values - hit)),
            c.range.as_ref().map(|r| {
                f.numbers.as_ref().map(|numbers| {
                    let lo = numbers.partition_point(|(x, _)| {
                        !(r.gt.is_none_or(|b| *x > b) && r.gte.is_none_or(|b| *x >= b))
                    });
                    let hi = numbers.partition_point(|(x, _)| {
                        r.lt.is_none_or(|b| *x < b) && r.lte.is_none_or(|b| *x <= b)
                    });
                    numbers
                        .get(lo..hi.max(lo))
                        .unwrap_or_default()
                        .iter()
                        .map(|(_, row)| *row)
                        .collect()
                })
            }),
            c.values_count.as_ref().map(|r| {
                let mut rows: RoaringBitmap = f
                    .counts
                    .iter()
                    .filter(|(count, _)| r.contains(**count as f64))
                    .fold(RoaringBitmap::new(), |acc, (_, rows)| acc | rows);
                if r.contains(0.0) {
                    rows |= u - &f.has_values;
                }
                Some(rows)
            }),
            c.is_empty.map(|want| {
                Some(if want {
                    u - &f.has_values
                } else {
                    f.has_values.clone()
                })
            }),
            c.is_null.map(|want| {
                Some(if want {
                    f.has_null.clone()
                } else {
                    u - &f.has_null
                })
            }),
        ];
        let mut known = u.clone();
        let mut exact = true;
        for answer in answers.into_iter().flatten() {
            match answer {
                Some(rows) => known &= rows,
                None => exact = false,
            }
        }
        if exact {
            Bounds::exact(known)
        } else {
            Bounds {
                sure: RoaringBitmap::new(),
                maybe: known,
            }
        }
    }
}

impl FieldIndex {
    fn add(&mut self, row: u32, values: &[&Value]) {
        let non_null = values.iter().filter(|v| !v.is_null()).count() as u32;
        if non_null > 0 {
            self.counts.entry(non_null).or_default().insert(row);
            self.has_values.insert(row);
        }
        if values.iter().any(|v| v.is_null()) {
            self.has_null.insert(row);
        }
        if let Some(t) = &mut self.text {
            t.add(row, values);
        }
        for v in values {
            match v {
                Value::String(s) => {
                    if let Some(k) = &mut self.keyword {
                        k.entry(s.clone()).or_default().insert(row);
                    }
                }
                Value::Bool(b) => {
                    if let Some(bools) = &mut self.bools {
                        bools[*b as usize].insert(row);
                    }
                }
                Value::Number(n) => {
                    if let Some(i) = &mut self.integer {
                        if let Some(x) = n.as_i64() {
                            i.ints.entry(x).or_default().insert(row);
                        } else if !n.is_u64()
                            && let Some(x) = n.as_f64()
                            && x.fract() == 0.0
                        {
                            i.whole_floats.entry(float_key(x)).or_default().insert(row);
                        }
                    }
                    if let (Some(numbers), Some(x)) = (&mut self.numbers, n.as_f64()) {
                        numbers.push((x, row));
                    }
                }
                _ => {}
            }
        }
    }

    /// Rows with a value equal to `m`, `None` when this field has no index for its type.
    fn lookup(&self, m: &MatchValue) -> Option<RoaringBitmap> {
        let found = |map: Option<&RoaringBitmap>| map.cloned().unwrap_or_default();
        match m {
            MatchValue::Keyword(s) => Some(found(self.keyword.as_ref()?.get(s))),
            MatchValue::Integer(x) => {
                let i = self.integer.as_ref()?;
                Some(found(i.ints.get(x)) | found(i.whole_floats.get(&float_key(*x as f64))))
            }
            MatchValue::Bool(b) => Some(self.bools.as_ref()?[*b as usize].clone()),
        }
    }

    fn lookup_any(&self, list: &[MatchValue]) -> Option<RoaringBitmap> {
        list.iter()
            .try_fold(RoaringBitmap::new(), |acc, m| Some(acc | self.lookup(m)?))
    }
}

#[cfg(test)]
mod tests;
