//! The shard manifest: which segments are live and how much of the WAL they cover.
//!
//! The manifest is the single point where a flush becomes real. It lives in a file called
//! `MANIFEST`, holding the JSON body on its first line and `crc32c <hex>` of that line on the
//! second. A new manifest is written to `MANIFEST.tmp`, synced, renamed over `MANIFEST` and
//! the directory synced, so after a crash the file is either the old manifest or the new one,
//! never a mix.
//!
//! Anything in the shard directory that the manifest does not mention, a segment built but
//! not yet published or one replaced by a merge, can be deleted at startup.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::Path;

use nocturnum_core::{PayloadIndexSpec, VectorConfig};
use serde::{Deserialize, Serialize};

use crate::crash::crash_point;
use crate::error::{Error, IoContext, Result};
use crate::sync::{sync_dir, sync_file};

const FORMAT_VERSION: u32 = 1;
pub const FILE_NAME: &str = "MANIFEST";
const TMP_NAME: &str = "MANIFEST.tmp";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    /// Always 0 in v1, recorded so clustering can split collections into shards later
    /// without changing the format.
    pub shard: u32,
    /// Goes up by one with every manifest written.
    pub generation: u64,
    pub vectors: VectorConfig,
    /// Every WAL record up to and including this sequence number is in the segments below,
    /// so recovery replays from the one after it.
    pub wal_upto: u64,
    pub next_segment_id: u64,
    /// Live segments, oldest first. A point's newest copy is in the last segment holding it.
    pub segments: Vec<u64>,
    /// The payload indexes every segment builds in memory when it is loaded, see
    /// `docs/decisions/0003-payload-indexes.md`. Left out of the file when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payload_indexes: Vec<PayloadIndexSpec>,
}

impl Manifest {
    pub fn new(vectors: VectorConfig) -> Manifest {
        Manifest {
            format: FORMAT_VERSION,
            shard: 0,
            generation: 0,
            vectors,
            wal_upto: 0,
            next_segment_id: 1,
            segments: Vec::new(),
            payload_indexes: Vec::new(),
        }
    }

    /// Reads the manifest in `dir`, `None` when there is none yet.
    pub fn read(dir: &Path) -> Result<Option<Manifest>> {
        let path = dir.join(FILE_NAME);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).ctx(|| format!("reading {}", path.display())),
        };
        let corrupt = |reason: &str| Error::Corrupt {
            path: path.clone(),
            offset: 0,
            reason: reason.into(),
        };
        let mut lines = text.lines();
        let (Some(body), Some(check), None) = (lines.next(), lines.next(), lines.next()) else {
            return Err(corrupt("expected a JSON line and a checksum line"));
        };
        let want = check
            .strip_prefix("crc32c ")
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or_else(|| corrupt("the checksum line is malformed"))?;
        if crc32c::crc32c(body.as_bytes()) != want {
            return Err(corrupt("the checksum does not match"));
        }
        let manifest: Manifest = serde_json::from_str(body)
            .map_err(|e| corrupt(&format!("the manifest does not parse: {e}")))?;
        if manifest.format != FORMAT_VERSION {
            return Err(corrupt(&format!(
                "manifest format {} is not supported by this build, which reads {FORMAT_VERSION}",
                manifest.format
            )));
        }
        Ok(Some(manifest))
    }

    /// Replaces the manifest in `dir` with this one, durably.
    pub fn write(&self, dir: &Path) -> Result<()> {
        let body = serde_json::to_string(self).expect("a manifest serialises");
        let text = format!("{body}\ncrc32c {:08x}\n", crc32c::crc32c(body.as_bytes()));
        let tmp = dir.join(TMP_NAME);
        let mut f = File::create(&tmp).ctx(|| format!("creating {}", tmp.display()))?;
        f.write_all(text.as_bytes())
            .ctx(|| format!("writing {}", tmp.display()))?;
        sync_file(&f).ctx(|| format!("syncing {}", tmp.display()))?;
        crash_point("manifest.tmp_written", || {});
        let path = dir.join(FILE_NAME);
        fs::rename(&tmp, &path).ctx(|| format!("renaming {}", tmp.display()))?;
        sync_dir(dir).ctx(|| format!("syncing {}", dir.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nocturnum_core::Distance;
    use tempfile::TempDir;

    fn sample() -> Manifest {
        let mut m = Manifest::new(VectorConfig {
            dim: 4,
            distance: Distance::Dot,
        });
        m.generation = 3;
        m.wal_upto = 99;
        m.segments = vec![1, 2, 5];
        m.next_segment_id = 6;
        m.payload_indexes = vec![PayloadIndexSpec {
            path: "a.b".parse().unwrap(),
            kind: nocturnum_core::PayloadIndexKind::Keyword,
        }];
        m
    }

    #[test]
    fn no_payload_indexes_leaves_the_field_out() {
        let dir = TempDir::new().unwrap();
        let m = Manifest::new(VectorConfig {
            dim: 2,
            distance: Distance::Dot,
        });
        m.write(dir.path()).unwrap();
        let text = fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(!text.contains("payload_indexes"), "{text}");
        assert_eq!(Manifest::read(dir.path()).unwrap(), Some(m));
    }

    #[test]
    fn round_trips_and_replaces() {
        let dir = TempDir::new().unwrap();
        assert_eq!(Manifest::read(dir.path()).unwrap(), None);
        let mut m = sample();
        m.write(dir.path()).unwrap();
        assert_eq!(Manifest::read(dir.path()).unwrap(), Some(m.clone()));
        m.generation += 1;
        m.write(dir.path()).unwrap();
        assert_eq!(Manifest::read(dir.path()).unwrap(), Some(m));
        assert!(!dir.path().join(TMP_NAME).exists());
    }

    #[test]
    fn damage_is_caught() {
        let dir = TempDir::new().unwrap();
        sample().write(dir.path()).unwrap();
        let path = dir.path().join(FILE_NAME);
        let original = fs::read(&path).unwrap();
        for i in 0..original.len() {
            let mut bytes = original.clone();
            bytes[i] ^= 0x01;
            fs::write(&path, &bytes).unwrap();
            assert!(Manifest::read(dir.path()).is_err(), "byte {i}");
        }
        fs::write(&path, &original[..original.len() / 2]).unwrap();
        assert!(Manifest::read(dir.path()).is_err());
    }
}
