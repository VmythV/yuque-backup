use std::{
    collections::HashMap,
    io::{Cursor, Read},
};

use anyhow::{Context, Result, anyhow, bail};
use roaring::RoaringBitmap;
use sha2::{Digest, Sha256};

use crate::{
    models::JobStage,
    state::{DocumentResumeState, ResumeBitmapWrite, ResumeItemWrite, StateStore, StoredBitmap},
};

const DENSE_ENCODING: &str = "dense-v1";
const ROARING_ENCODING: &str = "roaring-v1";
const AUTO_DENSE_DENSITY: f64 = 0.60;

#[derive(Debug, Clone)]
pub struct ResumeDoc {
    pub doc_id: String,
    pub slug: String,
    pub title: String,
    pub remote_updated_at: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ResumeStats {
    pub total: usize,
    pub done: usize,
    pub skipped: usize,
    pub downloaded: usize,
    pub failed: usize,
}

#[derive(Debug, Clone)]
pub struct ResumePlan {
    pub snapshot_hash: String,
    pub loaded_from_cache: bool,
    doc_count: usize,
    docs: Vec<ResumeDoc>,
    planned: DocSet,
    done: DocSet,
    skipped: DocSet,
    downloaded: DocSet,
    failed: DocSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkOutcome {
    pub was_done: bool,
    pub was_failed: bool,
}

impl ResumePlan {
    pub fn load_or_rebuild(
        state: &StateStore,
        host: &str,
        repo_id: &str,
        docs: Vec<ResumeDoc>,
        local_states: &HashMap<String, DocumentResumeState>,
    ) -> Result<Self> {
        let snapshot_hash = snapshot_hash(repo_id, &docs);
        let doc_count = docs.len();
        let loaded = state.load_resume_bitmaps(host, repo_id, &snapshot_hash)?;
        let mut plan = if let Some(snapshot) = loaded {
            if snapshot.doc_count != doc_count {
                return Self::load_or_rebuild_without_cache(
                    snapshot_hash,
                    docs,
                    doc_count,
                    local_states,
                );
            }
            let planned = DocSet::from_stored(snapshot.bitmaps.get("planned"), doc_count, || {
                DocSet::all_dense(doc_count)
            })?;
            let done = DocSet::from_stored(snapshot.bitmaps.get("done"), doc_count, || {
                DocSet::auto_empty(doc_count)
            })?;
            let skipped = DocSet::from_stored(snapshot.bitmaps.get("skipped"), doc_count, || {
                DocSet::auto_empty(doc_count)
            })?;
            let downloaded =
                DocSet::from_stored(snapshot.bitmaps.get("downloaded"), doc_count, || {
                    DocSet::auto_empty(doc_count)
                })?;
            let failed = DocSet::from_stored(snapshot.bitmaps.get("failed"), doc_count, || {
                DocSet::auto_empty(doc_count)
            })?;
            Self {
                snapshot_hash,
                loaded_from_cache: true,
                doc_count,
                docs,
                planned,
                done,
                skipped,
                downloaded,
                failed,
            }
        } else {
            Self {
                snapshot_hash,
                loaded_from_cache: false,
                doc_count,
                docs,
                planned: DocSet::all_dense(doc_count),
                done: DocSet::auto_empty(doc_count),
                skipped: DocSet::auto_empty(doc_count),
                downloaded: DocSet::auto_empty(doc_count),
                failed: DocSet::auto_empty(doc_count),
            }
        };
        plan.reconcile_with_local_states(local_states);
        Ok(plan)
    }

    fn load_or_rebuild_without_cache(
        snapshot_hash: String,
        docs: Vec<ResumeDoc>,
        doc_count: usize,
        local_states: &HashMap<String, DocumentResumeState>,
    ) -> Result<Self> {
        let mut plan = Self {
            snapshot_hash,
            loaded_from_cache: false,
            doc_count,
            docs,
            planned: DocSet::all_dense(doc_count),
            done: DocSet::auto_empty(doc_count),
            skipped: DocSet::auto_empty(doc_count),
            downloaded: DocSet::auto_empty(doc_count),
            failed: DocSet::auto_empty(doc_count),
        };
        plan.reconcile_with_local_states(local_states);
        Ok(plan)
    }

    pub fn stats(&self) -> ResumeStats {
        ResumeStats {
            total: self.doc_count,
            done: self.done.cardinality(),
            skipped: self.skipped.cardinality(),
            downloaded: self.downloaded.cardinality(),
            failed: self.failed.cardinality(),
        }
    }

    pub fn processed_count(&self) -> usize {
        self.done.cardinality() + self.failed.cardinality()
    }

    pub fn next_pending_from(&self, cursor: usize) -> Option<usize> {
        self.planned.next_set_not_in(&self.done, cursor)
    }

    pub fn mark_downloaded(&mut self, ordinal: usize) -> MarkOutcome {
        let was_done = self.done.contains(ordinal);
        let was_failed = self.failed.remove(ordinal);
        self.done.insert(ordinal);
        self.downloaded.insert(ordinal);
        self.skipped.remove(ordinal);
        MarkOutcome {
            was_done,
            was_failed,
        }
    }

    pub fn mark_failed(&mut self, ordinal: usize) -> MarkOutcome {
        let was_done = self.done.contains(ordinal);
        let was_failed = self.failed.contains(ordinal);
        if !was_done {
            self.failed.insert(ordinal);
        }
        MarkOutcome {
            was_done,
            was_failed,
        }
    }

    pub fn mark_pending(&mut self, ordinal: usize) {
        self.done.remove(ordinal);
        self.skipped.remove(ordinal);
        self.downloaded.remove(ordinal);
        self.failed.remove(ordinal);
    }

    pub fn flush(&self, state: &StateStore, host: &str, repo_id: &str) -> Result<()> {
        let bitmaps = [
            ("planned", &self.planned),
            ("done", &self.done),
            ("skipped", &self.skipped),
            ("downloaded", &self.downloaded),
            ("failed", &self.failed),
        ]
        .into_iter()
        .map(|(name, set)| {
            let (encoding, blob) = set.serialize()?;
            Ok(ResumeBitmapWrite {
                name: name.to_string(),
                encoding,
                cardinality: set.cardinality(),
                blob,
            })
        })
        .collect::<Result<Vec<_>>>()?;

        let items = self
            .docs
            .iter()
            .enumerate()
            .map(|(ordinal, doc)| ResumeItemWrite {
                ordinal,
                doc_id: doc.doc_id.clone(),
                slug: doc.slug.clone(),
                title: doc.title.clone(),
                remote_updated_at: doc.remote_updated_at.clone(),
            })
            .collect::<Vec<_>>();

        state.save_resume_bitmaps(
            host,
            repo_id,
            &self.snapshot_hash,
            self.doc_count,
            &items,
            &bitmaps,
        )
    }

    fn reconcile_with_local_states(&mut self, local_states: &HashMap<String, DocumentResumeState>) {
        for ordinal in 0..self.docs.len() {
            let doc = &self.docs[ordinal];
            let Some(local) = local_states.get(&doc.doc_id) else {
                continue;
            };
            if local.stage != JobStage::Complete {
                continue;
            }
            let unchanged = doc
                .remote_updated_at
                .as_deref()
                .map(|remote| local.remote_updated_at.as_deref() == Some(remote))
                .unwrap_or(true);
            if unchanged && !self.done.contains(ordinal) {
                self.done.insert(ordinal);
                self.skipped.insert(ordinal);
                self.failed.remove(ordinal);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum DocSet {
    Dense(DenseBitmap),
    Roaring(RoaringBitmap),
}

impl DocSet {
    pub fn all_dense(len: usize) -> Self {
        let mut bitmap = DenseBitmap::new(len);
        for ordinal in 0..len {
            bitmap.insert(ordinal);
        }
        Self::Dense(bitmap)
    }

    pub fn auto_empty(len: usize) -> Self {
        if len <= 4096 {
            Self::Dense(DenseBitmap::new(len))
        } else {
            Self::Roaring(RoaringBitmap::new())
        }
    }

    pub fn from_ordinals(len: usize, ordinals: impl IntoIterator<Item = usize>) -> Self {
        let ordinals = ordinals.into_iter().collect::<Vec<_>>();
        let density = if len == 0 {
            0.0
        } else {
            ordinals.len() as f64 / len as f64
        };
        if density >= AUTO_DENSE_DENSITY {
            let mut dense = DenseBitmap::new(len);
            for ordinal in ordinals {
                dense.insert(ordinal);
            }
            Self::Dense(dense)
        } else {
            let mut roaring = RoaringBitmap::new();
            for ordinal in ordinals {
                if let Ok(value) = u32::try_from(ordinal) {
                    roaring.insert(value);
                }
            }
            Self::Roaring(roaring)
        }
    }

    pub fn from_stored(
        stored: Option<&StoredBitmap>,
        len: usize,
        fallback: impl FnOnce() -> DocSet,
    ) -> Result<Self> {
        let Some(stored) = stored else {
            return Ok(fallback());
        };
        match stored.encoding.as_str() {
            DENSE_ENCODING => Ok(Self::Dense(DenseBitmap::deserialize(&stored.blob)?)),
            ROARING_ENCODING => {
                let mut cursor = Cursor::new(&stored.blob);
                let roaring = RoaringBitmap::deserialize_from(&mut cursor)
                    .context("Roaring Bitmap 反序列化失败")?;
                Ok(Self::Roaring(roaring))
            }
            encoding => bail!("未知 Bitmap 编码: {encoding}"),
        }
        .map(|set| set.with_len(len))
    }

    pub fn insert(&mut self, ordinal: usize) {
        match self {
            Self::Dense(bitmap) => bitmap.insert(ordinal),
            Self::Roaring(bitmap) => {
                if let Ok(value) = u32::try_from(ordinal) {
                    bitmap.insert(value);
                }
            }
        }
    }

    pub fn remove(&mut self, ordinal: usize) -> bool {
        match self {
            Self::Dense(bitmap) => bitmap.remove(ordinal),
            Self::Roaring(bitmap) => u32::try_from(ordinal)
                .map(|value| bitmap.remove(value))
                .unwrap_or(false),
        }
    }

    pub fn contains(&self, ordinal: usize) -> bool {
        match self {
            Self::Dense(bitmap) => bitmap.contains(ordinal),
            Self::Roaring(bitmap) => u32::try_from(ordinal)
                .map(|value| bitmap.contains(value))
                .unwrap_or(false),
        }
    }

    pub fn cardinality(&self) -> usize {
        match self {
            Self::Dense(bitmap) => bitmap.cardinality(),
            Self::Roaring(bitmap) => bitmap.len() as usize,
        }
    }

    pub fn next_set_not_in(&self, other: &DocSet, cursor: usize) -> Option<usize> {
        match self {
            Self::Dense(bitmap) => bitmap.next_set_not_in(other, cursor),
            Self::Roaring(bitmap) => bitmap
                .iter()
                .skip_while(|value| (*value as usize) < cursor)
                .map(|value| value as usize)
                .find(|ordinal| !other.contains(*ordinal)),
        }
    }

    pub fn serialize(&self) -> Result<(String, Vec<u8>)> {
        match self {
            Self::Dense(bitmap) => Ok((DENSE_ENCODING.into(), bitmap.serialize())),
            Self::Roaring(bitmap) => {
                let mut bytes = Vec::new();
                bitmap
                    .serialize_into(&mut bytes)
                    .context("Roaring Bitmap 序列化失败")?;
                Ok((ROARING_ENCODING.into(), bytes))
            }
        }
    }

    fn with_len(self, len: usize) -> Self {
        match self {
            Self::Dense(mut bitmap) => {
                bitmap.resize(len);
                Self::Dense(bitmap)
            }
            Self::Roaring(bitmap) => Self::Roaring(bitmap),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DenseBitmap {
    len: usize,
    words: Vec<u64>,
}

impl DenseBitmap {
    pub fn new(len: usize) -> Self {
        Self {
            len,
            words: vec![0; len.div_ceil(64)],
        }
    }

    pub fn insert(&mut self, ordinal: usize) {
        if ordinal >= self.len {
            self.resize(ordinal + 1);
        }
        let (word, bit) = word_bit(ordinal);
        self.words[word] |= 1_u64 << bit;
    }

    pub fn remove(&mut self, ordinal: usize) -> bool {
        if ordinal >= self.len {
            return false;
        }
        let (word, bit) = word_bit(ordinal);
        let mask = 1_u64 << bit;
        let existed = self.words[word] & mask != 0;
        self.words[word] &= !mask;
        existed
    }

    pub fn contains(&self, ordinal: usize) -> bool {
        if ordinal >= self.len {
            return false;
        }
        let (word, bit) = word_bit(ordinal);
        self.words
            .get(word)
            .map(|value| value & (1_u64 << bit) != 0)
            .unwrap_or(false)
    }

    pub fn cardinality(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    pub fn next_set_not_in(&self, other: &DocSet, cursor: usize) -> Option<usize> {
        if cursor >= self.len {
            return None;
        }
        for word_index in cursor / 64..self.words.len() {
            let mut word = self.words[word_index];
            if word_index == cursor / 64 {
                word &= !0_u64 << (cursor % 64);
            }
            while word != 0 {
                let bit = word.trailing_zeros() as usize;
                let ordinal = word_index * 64 + bit;
                if ordinal >= self.len {
                    return None;
                }
                if !other.contains(ordinal) {
                    return Some(ordinal);
                }
                word &= !(1_u64 << bit);
            }
        }
        None
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + self.words.len() * 8);
        bytes.extend_from_slice(&(self.len as u64).to_le_bytes());
        for word in &self.words {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 8 {
            bail!("Dense Bitmap 数据过短");
        }
        let mut len_bytes = [0_u8; 8];
        len_bytes.copy_from_slice(&bytes[..8]);
        let len = u64::from_le_bytes(len_bytes) as usize;
        let expected_words = len.div_ceil(64);
        let expected_len = 8 + expected_words * 8;
        if bytes.len() != expected_len {
            bail!(
                "Dense Bitmap 长度不匹配: expected {expected_len}, got {}",
                bytes.len()
            );
        }
        let mut cursor = Cursor::new(&bytes[8..]);
        let mut words = Vec::with_capacity(expected_words);
        for _ in 0..expected_words {
            let mut word = [0_u8; 8];
            cursor
                .read_exact(&mut word)
                .map_err(|error| anyhow!("Dense Bitmap 读取失败: {error}"))?;
            words.push(u64::from_le_bytes(word));
        }
        Ok(Self { len, words })
    }

    fn resize(&mut self, len: usize) {
        self.len = len;
        self.words.resize(len.div_ceil(64), 0);
        if !len.is_multiple_of(64) && !self.words.is_empty() {
            let valid_bits = len % 64;
            let mask = (1_u64 << valid_bits) - 1;
            if let Some(last) = self.words.last_mut() {
                *last &= mask;
            }
        }
    }
}

fn word_bit(ordinal: usize) -> (usize, usize) {
    (ordinal / 64, ordinal % 64)
}

fn snapshot_hash(repo_id: &str, docs: &[ResumeDoc]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_id.as_bytes());
    hasher.update([0]);
    for doc in docs {
        hasher.update(doc.doc_id.as_bytes());
        hasher.update([0]);
        hasher.update(doc.slug.as_bytes());
        hasher.update([0]);
        if let Some(updated) = &doc.remote_updated_at {
            hasher.update(updated.as_bytes());
        }
        hasher.update([0xff]);
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_bitmap_finds_first_gap_against_done() {
        let planned = DocSet::all_dense(130);
        let mut done = DocSet::all_dense(130);
        done.remove(65);
        assert_eq!(planned.next_set_not_in(&done, 0), Some(65));
        assert_eq!(planned.next_set_not_in(&done, 66), None);
    }

    #[test]
    fn dense_bitmap_round_trips() {
        let mut bitmap = DenseBitmap::new(100);
        bitmap.insert(0);
        bitmap.insert(63);
        bitmap.insert(99);
        let decoded = DenseBitmap::deserialize(&bitmap.serialize()).unwrap();
        assert!(decoded.contains(0));
        assert!(decoded.contains(63));
        assert!(decoded.contains(99));
        assert_eq!(decoded.cardinality(), 3);
    }

    #[test]
    fn roaring_docset_round_trips() {
        let set = DocSet::from_ordinals(100_000, [1_usize, 999, 88_888]);
        let (encoding, blob) = set.serialize().unwrap();
        assert_eq!(encoding, ROARING_ENCODING);
        let stored = StoredBitmap {
            encoding,
            cardinality: 3,
            blob,
        };
        let decoded =
            DocSet::from_stored(Some(&stored), 100_000, || DocSet::auto_empty(100_000)).unwrap();
        assert!(decoded.contains(1));
        assert!(decoded.contains(999));
        assert!(decoded.contains(88_888));
    }

    #[test]
    fn resume_plan_reconciles_local_completed_docs() {
        let docs = vec![
            ResumeDoc {
                doc_id: "a".into(),
                slug: "a".into(),
                title: "A".into(),
                remote_updated_at: Some("1".into()),
            },
            ResumeDoc {
                doc_id: "b".into(),
                slug: "b".into(),
                title: "B".into(),
                remote_updated_at: Some("2".into()),
            },
        ];
        let mut local = HashMap::new();
        local.insert(
            "a".into(),
            DocumentResumeState {
                stage: JobStage::Complete,
                remote_updated_at: Some("1".into()),
            },
        );
        let hash = snapshot_hash("repo", &docs);
        let mut plan = ResumePlan {
            snapshot_hash: hash,
            loaded_from_cache: false,
            doc_count: docs.len(),
            docs,
            planned: DocSet::all_dense(2),
            done: DocSet::auto_empty(2),
            skipped: DocSet::auto_empty(2),
            downloaded: DocSet::auto_empty(2),
            failed: DocSet::auto_empty(2),
        };
        plan.reconcile_with_local_states(&local);
        assert_eq!(plan.next_pending_from(0), Some(1));
        assert_eq!(plan.stats().skipped, 1);
    }
}
