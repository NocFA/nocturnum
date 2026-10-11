use super::*;
use nocturnum_core::{Payload, PayloadIndexKind, PointId};
use serde_json::json;

/// `n` rows where `tag` is indexed and `free` is not, row `r` has `tag` "hit" when
/// `r % tag_every == 0` and `free` 1 when `r % free_every == 0`.
fn segment(n: u32, tag_every: u32, free_every: u32) -> (PayloadIndex, Vec<Payload>) {
    let payloads: Vec<Payload> = (0..n)
        .map(|r| {
            serde_json::from_value(json!({
                "tag": if r % tag_every == 0 { "hit" } else { "miss" },
                "free": if r % free_every == 0 { 1 } else { 0 },
            }))
            .unwrap()
        })
        .collect();
    let index = PayloadIndex::build(
        n,
        &[("tag".parse().unwrap(), PayloadIndexKind::Keyword)],
        payloads
            .iter()
            .enumerate()
            .map(|(r, p)| (r as u32, PointId::Num(r as u64), p)),
    );
    (index, payloads)
}

fn filter(v: serde_json::Value) -> Filter {
    serde_json::from_value(v).unwrap()
}

fn options(enumerate_up_to: u64) -> PlanOptions {
    PlanOptions {
        enumerate_up_to,
        samples: 64,
    }
}

#[test]
fn indexed_filters_plan_without_reading_payloads() {
    let (index, _) = segment(10_000, 100, 1);
    let f = filter(json!({"must": [{"key": "tag", "match": "hit"}]}));
    let never = |_: u32| -> Result<bool, ()> { panic!("no payload should be read") };
    let p = plan(&index, &f, None, &options(500), never).unwrap();
    assert_eq!(p.strategy, Strategy::Enumerate);
    assert_eq!(
        p.estimate,
        Estimate {
            min: 100,
            expected: 100,
            max: 100
        }
    );
    let p = plan(&index, &f, None, &options(50), never).unwrap();
    assert_eq!(p.strategy, Strategy::Graph);
}

#[test]
fn a_straddling_estimate_is_sampled() {
    let (index, payloads) = segment(10_000, 1, 10);
    let f = filter(json!({"must": [{"key": "free", "match": 1}]}));
    let mut reads = 0;
    let check = |r: u32| -> Result<bool, ()> {
        reads += 1;
        Ok(f.matches(PointId::Num(r as u64), &payloads[r as usize]))
    };
    let p = plan(&index, &f, None, &options(5_000), check).unwrap();
    assert_eq!(reads, 64);
    assert_eq!((p.estimate.min, p.estimate.max), (0, 10_000));
    // A tenth of the rows match, and the sample is fixed, so this lands at the same count
    // every run, where an even stride of 156 rows lined up with the period of 10 and saw 12.5%.
    assert!(
        (600..=1_400).contains(&p.estimate.expected),
        "{:?}",
        p.estimate
    );
    assert_eq!(p.strategy, Strategy::Enumerate);

    let f = filter(json!({"must_not": [{"key": "free", "match": 1}]}));
    let check = |r: u32| -> Result<bool, ()> {
        Ok(f.matches(PointId::Num(r as u64), &payloads[r as usize]))
    };
    let p = plan(&index, &f, None, &options(5_000), check).unwrap();
    assert_eq!(p.strategy, Strategy::Graph);
}

#[test]
fn sampling_every_uncertain_row_counts_exactly() {
    let (index, payloads) = segment(300, 3, 2);
    let f = filter(json!({"must": [
        {"key": "tag", "match": "hit"},
        {"key": "free", "match": 1}
    ]}));
    let bounds = index.bounds(&f);
    assert_eq!((bounds.sure.len(), bounds.maybe.len()), (0, 100));
    let e = estimate(&bounds, 1_000, |r| -> Result<bool, ()> {
        Ok(f.matches(PointId::Num(r as u64), &payloads[r as usize]))
    })
    .unwrap();
    // Rows divisible by both 3 and 2.
    assert_eq!(e.expected, 50);
}

#[test]
fn only_live_rows_are_planned() {
    let (index, _) = segment(1_000, 1, 1);
    let live: RoaringBitmap = (0..1_000).step_by(4).collect();
    let p = plan(
        &index,
        &Filter::default(),
        Some(&live),
        &options(100),
        |_| -> Result<bool, ()> { unreachable!() },
    )
    .unwrap();
    assert_eq!(p.estimate.max, 250);
    assert_eq!(p.bounds.maybe, live);
    assert_eq!(p.strategy, Strategy::Graph);
}

#[test]
fn errors_from_reading_a_payload_come_back() {
    let (index, _) = segment(100, 1, 1);
    let f = filter(json!({"must": [{"key": "free", "match": 1}]}));
    let got = plan(&index, &f, None, &options(10), |_| Err("unreadable"));
    assert_eq!(got.unwrap_err(), "unreadable");
}

#[test]
fn the_default_threshold_scales_with_dimension() {
    assert_eq!(PlanOptions::for_dim(768).enumerate_up_to, 3_413);
    assert_eq!(PlanOptions::for_dim(65_536).enumerate_up_to, 1_000);
}
