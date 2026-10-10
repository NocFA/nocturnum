//! The operations a shard logs and applies, and their binary encoding in WAL records.
//!
//! Every operation names its points by id. Writes chosen by a filter are resolved to ids
//! before they are logged, so replaying the log always touches the same points even if
//! payloads changed in between.
//!
//! # Encoding
//!
//! All integers are little endian. An operation is a tag byte followed by its body:
//!
//! | tag | operation | body |
//! |---|---|---|
//! | 1 | upsert | `u32` count, then per point: id, `u32` dim, `dim` × `f32`, `u32` length, payload JSON |
//! | 2 | delete | ids |
//! | 3 | set payload | ids, `u32` length, payload JSON |
//! | 4 | overwrite payload | ids, `u32` length, payload JSON |
//! | 5 | delete payload keys | ids, `u32` count, then per key: `u32` length, UTF-8 |
//! | 6 | clear payload | ids |
//!
//! "ids" is a `u32` count followed by that many ids, and an id is a tag byte, `0` then a `u64`
//! for a number or `1` then 16 bytes for a UUID.

use nocturnum_core::{Payload, PointId};
use uuid::Uuid;

/// One point to insert or replace whole.
#[derive(Debug, Clone, PartialEq)]
pub struct PointWrite {
    pub id: PointId,
    /// Already checked and prepared for the collection's distance (unit length for cosine).
    pub vector: Vec<f32>,
    pub payload: Payload,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    /// Inserts each point, replacing any point with the same id.
    Upsert(Vec<PointWrite>),
    /// Removes the points, ids that do not exist are ignored.
    Delete(Vec<PointId>),
    /// Sets the given top-level keys on each point's payload, leaving other keys alone.
    SetPayload { ids: Vec<PointId>, payload: Payload },
    /// Replaces each point's payload.
    OverwritePayload { ids: Vec<PointId>, payload: Payload },
    /// Removes the given top-level keys from each point's payload.
    DeletePayloadKeys {
        ids: Vec<PointId>,
        keys: Vec<String>,
    },
    /// Empties each point's payload.
    ClearPayload(Vec<PointId>),
}

/// Why a WAL record could not be decoded into an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl Operation {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Operation::Upsert(points) => {
                out.push(1);
                put_u32(&mut out, points.len());
                for p in points {
                    put_id(&mut out, p.id);
                    put_u32(&mut out, p.vector.len());
                    for x in &p.vector {
                        out.extend_from_slice(&x.to_le_bytes());
                    }
                    put_payload(&mut out, &p.payload);
                }
            }
            Operation::Delete(ids) => {
                out.push(2);
                put_ids(&mut out, ids);
            }
            Operation::SetPayload { ids, payload } => {
                out.push(3);
                put_ids(&mut out, ids);
                put_payload(&mut out, payload);
            }
            Operation::OverwritePayload { ids, payload } => {
                out.push(4);
                put_ids(&mut out, ids);
                put_payload(&mut out, payload);
            }
            Operation::DeletePayloadKeys { ids, keys } => {
                out.push(5);
                put_ids(&mut out, ids);
                put_u32(&mut out, keys.len());
                for k in keys {
                    put_u32(&mut out, k.len());
                    out.extend_from_slice(k.as_bytes());
                }
            }
            Operation::ClearPayload(ids) => {
                out.push(6);
                put_ids(&mut out, ids);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Operation, DecodeError> {
        let mut r = Reader { bytes, at: 0 };
        let op = match r.u8()? {
            1 => {
                let n = r.count(1 + 8 + 4 + 4)?;
                let mut points = Vec::with_capacity(n);
                for _ in 0..n {
                    let id = r.id()?;
                    let dim = r.count(4)?;
                    let raw = r.take(dim * 4)?;
                    let vector = raw
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| f32::from_le_bytes(*c))
                        .collect();
                    let payload = r.payload()?;
                    points.push(PointWrite {
                        id,
                        vector,
                        payload,
                    });
                }
                Operation::Upsert(points)
            }
            2 => Operation::Delete(r.ids()?),
            3 => Operation::SetPayload {
                ids: r.ids()?,
                payload: r.payload()?,
            },
            4 => Operation::OverwritePayload {
                ids: r.ids()?,
                payload: r.payload()?,
            },
            5 => {
                let ids = r.ids()?;
                let n = r.count(4)?;
                let mut keys = Vec::with_capacity(n);
                for _ in 0..n {
                    let len = r.count(1)?;
                    let key = std::str::from_utf8(r.take(len)?)
                        .map_err(|_| DecodeError("a payload key is not valid UTF-8".into()))?;
                    keys.push(key.to_owned());
                }
                Operation::DeletePayloadKeys { ids, keys }
            }
            6 => Operation::ClearPayload(r.ids()?),
            tag => return Err(DecodeError(format!("unknown operation tag {tag}"))),
        };
        if r.at != bytes.len() {
            return Err(DecodeError(format!(
                "{} unexpected bytes after the operation",
                bytes.len() - r.at
            )));
        }
        Ok(op)
    }
}

fn put_u32(out: &mut Vec<u8>, n: usize) {
    let n = u32::try_from(n).expect("lengths in one operation fit in u32");
    out.extend_from_slice(&n.to_le_bytes());
}

pub(crate) fn put_id(out: &mut Vec<u8>, id: PointId) {
    match id {
        PointId::Num(n) => {
            out.push(0);
            out.extend_from_slice(&n.to_le_bytes());
        }
        PointId::Uuid(u) => {
            out.push(1);
            out.extend_from_slice(u.as_bytes());
        }
    }
}

fn put_ids(out: &mut Vec<u8>, ids: &[PointId]) {
    put_u32(out, ids.len());
    for id in ids {
        put_id(out, *id);
    }
}

fn put_payload(out: &mut Vec<u8>, payload: &Payload) {
    let json = serde_json::to_vec(payload).expect("a payload always serialises");
    put_u32(out, json.len());
    out.extend_from_slice(&json);
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| DecodeError("the operation ends early".into()))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    /// A count of items each at least `min_item` bytes long, checked against what is left so
    /// a damaged count cannot trigger a huge allocation.
    fn count(&mut self, min_item: usize) -> Result<usize, DecodeError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_item) > self.bytes.len() - self.at {
            return Err(DecodeError(format!(
                "a count of {n} is larger than the operation"
            )));
        }
        Ok(n)
    }

    fn id(&mut self) -> Result<PointId, DecodeError> {
        match self.u8()? {
            0 => Ok(PointId::Num(u64::from_le_bytes(
                self.take(8)?.try_into().expect("8 bytes"),
            ))),
            1 => Ok(PointId::Uuid(Uuid::from_bytes(
                self.take(16)?.try_into().expect("16 bytes"),
            ))),
            tag => Err(DecodeError(format!("unknown point id tag {tag}"))),
        }
    }

    fn ids(&mut self) -> Result<Vec<PointId>, DecodeError> {
        let n = self.count(9)?;
        (0..n).map(|_| self.id()).collect()
    }

    fn payload(&mut self) -> Result<Payload, DecodeError> {
        let len = self.count(1)?;
        serde_json::from_slice(self.take(len)?)
            .map_err(|e| DecodeError(format!("a payload is not a JSON object: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(v: serde_json::Value) -> Payload {
        serde_json::from_value(v).unwrap()
    }

    fn samples() -> Vec<Operation> {
        let uuid = PointId::Uuid(Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0));
        vec![
            Operation::Upsert(vec![
                PointWrite {
                    id: PointId::Num(u64::MAX),
                    vector: vec![1.5, -0.0, f32::MIN_POSITIVE],
                    payload: payload(json!({"a": [1, "x", null], "b": {"c": true}})),
                },
                PointWrite {
                    id: uuid,
                    vector: vec![],
                    payload: Payload::new(),
                },
            ]),
            Operation::Upsert(vec![]),
            Operation::Delete(vec![PointId::Num(0), uuid]),
            Operation::SetPayload {
                ids: vec![PointId::Num(7)],
                payload: payload(json!({"k": 1})),
            },
            Operation::OverwritePayload {
                ids: vec![],
                payload: Payload::new(),
            },
            Operation::DeletePayloadKeys {
                ids: vec![uuid],
                keys: vec!["a".into(), "é".into()],
            },
            Operation::ClearPayload(vec![PointId::Num(3)]),
        ]
    }

    #[test]
    fn round_trips() {
        for op in samples() {
            assert_eq!(Operation::decode(&op.encode()).unwrap(), op);
        }
    }

    #[test]
    fn every_truncation_and_extension_is_rejected() {
        for op in samples() {
            let bytes = op.encode();
            for cut in 0..bytes.len() {
                assert!(
                    Operation::decode(&bytes[..cut]).is_err(),
                    "{op:?} cut at {cut}"
                );
            }
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(Operation::decode(&longer).is_err());
        }
    }

    #[test]
    fn huge_counts_do_not_allocate() {
        let mut bytes = vec![2];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Operation::decode(&bytes).is_err());
    }
}
