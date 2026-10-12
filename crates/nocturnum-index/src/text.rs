//! Text indexes and BM25 ranking.
//!
//! A text index belongs to one field of one immutable segment, like the other payload
//! indexes, and is built in memory from the segment's payloads, see
//! `docs/decisions/0004-text-ranking.md`. It keeps each term's postings, the rows holding the
//! term with how often they hold it, and each row's length in terms.
//!
//! Terms are kept as strings, so two words never share an entry, and nothing about the
//! corpus is baked into what is stored: the statistics BM25 needs (how many documents there
//! are, how many hold each term and their average length) are counted over the live rows when
//! a query runs, so deleting or adding documents changes later scores straight away.
//!
//! # Tokens
//!
//! [`tokenize`] lowercases text and splits it on every character that is not a letter or a
//! digit, in any script, so `"Don't panic!"` gives `don`, `t` and `panic`. There is no
//! stemming and no stop-word list, so `run` and `running` are different terms. A field's
//! string values are tokenized in turn, an array of strings counting as one document, and
//! values of other types are skipped.
//!
//! # Scoring
//!
//! A document `d` scores, for the distinct terms `t` of the query,
//!
//! ```text
//! sum over t of  idf(t) * tf(t, d) * (k1 + 1) / (tf(t, d) + k1 * (1 - b + b * len(d) / avg_len))
//! idf(t) = ln(1 + (docs - df(t) + 0.5) / (df(t) + 0.5))
//! ```
//!
//! where `docs` counts the live documents with at least one term, `df(t)` those holding `t`,
//! and `avg_len` is their average length. A query repeating a word counts it once, and a
//! document holding none of the query's terms is not a match.

use std::collections::{HashMap, HashSet};

use roaring::RoaringBitmap;
use serde_json::Value;

/// The BM25 `k1` when a query gives none, how quickly repeats of a term stop adding score.
pub const DEFAULT_K1: f64 = 1.2;
/// The BM25 `b` when a query gives none, how much a long document is penalised.
pub const DEFAULT_B: f64 = 0.75;

/// Calls `emit` with each token of `text`, lowercased, in order.
pub fn tokenize(text: &str, mut emit: impl FnMut(String)) {
    let mut token = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            token.extend(c.to_lowercase());
        } else if !token.is_empty() {
            emit(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        emit(token);
    }
}

/// The distinct tokens of a query, sorted, so scores add up their terms in one fixed order,
/// or `None` once there are more than `max` of them, which bounds the memory a query takes
/// however long it is.
pub fn query_terms(text: &str, max: usize) -> Option<Vec<String>> {
    let mut terms = HashSet::new();
    let mut over = false;
    tokenize(text, |t| {
        if !over && !terms.contains(&t) {
            if terms.len() == max {
                over = true;
            } else {
                terms.insert(t);
            }
        }
    });
    if over {
        return None;
    }
    let mut terms: Vec<String> = terms.into_iter().collect();
    terms.sort_unstable();
    Some(terms)
}

/// A field's values as one document: how often each token appears, and how many tokens it
/// has in all.
pub fn document(values: &[&Value]) -> (HashMap<String, u32>, u32) {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut len = 0u32;
    for v in values {
        if let Value::String(s) = v {
            tokenize(s, |t| {
                *counts.entry(t).or_default() += 1;
                len = len.saturating_add(1);
            });
        }
    }
    (counts, len)
}

/// BM25's two knobs, chosen per query since nothing stored depends on them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bm25 {
    pub k1: f64,
    pub b: f64,
}

impl Default for Bm25 {
    fn default() -> Self {
        Bm25 {
            k1: DEFAULT_K1,
            b: DEFAULT_B,
        }
    }
}

impl Bm25 {
    /// Checks the knobs are in the ranges where BM25 behaves, `k1` from 0 to 10 and `b` from
    /// 0 to 1, returning what is wrong.
    pub fn validate(&self) -> Result<(), String> {
        if !(0.0..=10.0).contains(&self.k1) {
            return Err(format!("k1 must be between 0 and 10, got {}", self.k1));
        }
        if !(0.0..=1.0).contains(&self.b) {
            return Err(format!("b must be between 0 and 1, got {}", self.b));
        }
        Ok(())
    }

    /// One term's share of a document's score.
    #[inline]
    pub fn term(&self, idf: f64, tf: u32, len: u32, avg_len: f64) -> f64 {
        let tf = f64::from(tf);
        let norm = 1.0 - self.b + self.b * f64::from(len) / avg_len;
        idf * tf * (self.k1 + 1.0) / (tf + self.k1 * norm)
    }
}

/// How rare a term is, from how many of `docs` documents hold it.
pub fn idf(docs: u64, df: u64) -> f64 {
    let (n, df) = (docs as f64, df as f64);
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

/// The rows holding one term, sorted, with how often each holds it.
#[derive(Debug, Default)]
pub struct Postings {
    pub rows: Vec<u32>,
    pub tfs: Vec<u32>,
}

/// One field's text index over the rows of a segment.
#[derive(Debug, Default)]
pub struct TextIndex {
    terms: HashMap<Box<str>, Postings>,
    /// Each row's length in tokens, 0 for rows without text.
    lens: Vec<u32>,
    /// Rows with at least one token.
    docs: RoaringBitmap,
    /// The sum of `lens`.
    total_len: u64,
}

impl TextIndex {
    /// Adds a row's values, rows coming in increasing order.
    pub(crate) fn add(&mut self, row: u32, values: &[&Value]) {
        let (counts, len) = document(values);
        if len == 0 {
            return;
        }
        let at = row as usize;
        if self.lens.len() <= at {
            self.lens.resize(at + 1, 0);
        }
        self.lens[at] = len;
        self.docs.insert(row);
        self.total_len += u64::from(len);
        for (term, tf) in counts {
            let p = self.terms.entry(term.into_boxed_str()).or_default();
            debug_assert!(p.rows.last().is_none_or(|&r| r < row), "rows in order");
            p.rows.push(row);
            p.tfs.push(tf);
        }
    }

    /// The rows holding `term`.
    pub fn postings(&self, term: &str) -> Option<&Postings> {
        self.terms.get(term)
    }

    /// A row's length in tokens.
    pub fn len_of(&self, row: u32) -> u32 {
        self.lens.get(row as usize).copied().unwrap_or(0)
    }

    /// How many documents are among `live`, and their total length in tokens.
    pub fn stats(&self, live: &RoaringBitmap) -> (u64, u64) {
        let docs = self.docs.intersection_len(live);
        // Usually few rows are dead, so their lengths come off the total.
        let total = if docs * 2 >= self.docs.len() {
            let dead: u64 = (&self.docs - live)
                .iter()
                .map(|r| u64::from(self.len_of(r)))
                .sum();
            self.total_len - dead
        } else {
            (&self.docs & live)
                .iter()
                .map(|r| u64::from(self.len_of(r)))
                .sum()
        };
        (docs, total)
    }

    /// How many rows among `live` hold `term`.
    pub fn df(&self, term: &str, live: &RoaringBitmap) -> u64 {
        self.postings(term).map_or(0, |p| {
            p.rows.iter().filter(|r| live.contains(**r)).count() as u64
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tokens(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        tokenize(s, |t| out.push(t));
        out
    }

    #[test]
    fn tokens_are_lowercase_letter_and_digit_runs() {
        assert_eq!(tokens("Don't panic!"), ["don", "t", "panic"]);
        assert_eq!(
            tokens("  TLS1.3, über-Größe  "),
            ["tls1", "3", "über", "größe"]
        );
        assert_eq!(tokens("日本語 text"), ["日本語", "text"]);
        assert!(tokens(" ... ").is_empty());
    }

    #[test]
    fn query_terms_are_distinct_and_sorted() {
        assert_eq!(query_terms("b a B c a", 3).unwrap(), ["a", "b", "c"]);
        assert_eq!(query_terms("a a a a b", 2).unwrap(), ["a", "b"]);
        assert_eq!(query_terms("a b c", 2), None);
    }

    #[test]
    fn documents_join_string_values_and_skip_others() {
        let a = json!("red fox");
        let b = json!(3);
        let c = json!("Red hen");
        let (counts, len) = document(&[&a, &b, &c]);
        assert_eq!(len, 4);
        assert_eq!(counts["red"], 2);
        assert_eq!(counts["hen"], 1);
    }

    #[test]
    fn scores_match_the_formula_worked_by_hand() {
        // docs = 3, df = 1: idf = ln(1 + 2.5 / 1.5) = ln(8 / 3).
        let idf = idf(3, 1);
        assert!((idf - (8.0f64 / 3.0).ln()).abs() < 1e-12);
        // tf = 2, len = 4, avg_len = 4: 2 * 2.2 / (2 + 1.2) = 1.375.
        let s = Bm25::default().term(idf, 2, 4, 4.0);
        assert!((s - idf * 1.375).abs() < 1e-12);
        // A longer than average document scores less for the same count.
        assert!(Bm25::default().term(idf, 2, 8, 4.0) < s);
        // With b = 0 length does not matter.
        let flat = Bm25 { k1: 1.2, b: 0.0 };
        assert_eq!(flat.term(idf, 2, 8, 4.0), flat.term(idf, 2, 4, 4.0));
    }

    #[test]
    fn rare_terms_weigh_more_and_never_go_negative() {
        assert!(idf(100, 1) > idf(100, 50));
        assert!(idf(100, 100) > 0.0);
    }

    #[test]
    fn stats_count_only_live_rows() {
        let mut ix = TextIndex::default();
        let a = json!("one two");
        let b = json!("three");
        let c = json!(7);
        let d = json!("two two two two");
        ix.add(0, &[&a]);
        ix.add(1, &[&b]);
        ix.add(2, &[&c]);
        ix.add(3, &[&d]);
        let mut live = RoaringBitmap::new();
        live.insert_range(0..4);
        assert_eq!(ix.stats(&live), (3, 7));
        assert_eq!(ix.df("two", &live), 2);
        live.remove(3);
        assert_eq!(ix.stats(&live), (2, 3));
        assert_eq!(ix.df("two", &live), 1);
        live.remove(0);
        live.remove(1);
        assert_eq!(ix.stats(&live), (0, 0));
        assert_eq!(ix.postings("two").unwrap().tfs, [1, 4]);
    }

    #[test]
    fn knobs_are_checked() {
        assert!(Bm25::default().validate().is_ok());
        assert!(Bm25 { k1: -1.0, b: 0.5 }.validate().is_err());
        assert!(Bm25 { k1: 1.0, b: 1.5 }.validate().is_err());
        assert!(
            Bm25 {
                k1: f64::NAN,
                b: 0.5
            }
            .validate()
            .is_err()
        );
    }
}
