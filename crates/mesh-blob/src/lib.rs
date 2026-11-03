use bytes::Bytes;
use dashmap::DashMap;
use mesh_proto::PackId;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BlobError {
    #[error("packfile '{0}' not found in blob store")]
    PackNotFound(PackId),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub trait PackBlobStore: Send + Sync {
    fn put_pack(&self, pack_bytes: Bytes) -> Result<PackId, BlobError>;
    fn put_pack_with_id(&self, pack_id: &PackId, pack_bytes: Bytes) -> Result<(), BlobError>;
    fn get_pack(&self, pack_id: &PackId) -> Result<Bytes, BlobError>;
    fn has_pack(&self, pack_id: &PackId) -> bool;
    fn total_packs(&self) -> usize;
    fn total_bytes(&self) -> u64;
}

/// In-memory immutable packfile blob store.
#[derive(Default)]
pub struct MemoryBlobStore {
    packs: DashMap<PackId, Bytes>,
    stored_bytes: AtomicU64,
}

impl MemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PackBlobStore for MemoryBlobStore {
    fn put_pack(&self, pack_bytes: Bytes) -> Result<PackId, BlobError> {
        let pack_id = PackId::from_pack_bytes(&pack_bytes);
        self.put_pack_with_id(&pack_id, pack_bytes)?;
        Ok(pack_id)
    }

    fn put_pack_with_id(&self, pack_id: &PackId, pack_bytes: Bytes) -> Result<(), BlobError> {
        let len = pack_bytes.len() as u64;
        if self.packs.insert(pack_id.clone(), pack_bytes).is_none() {
            self.stored_bytes.fetch_add(len, Ordering::Relaxed);
        }
        Ok(())
    }

    fn get_pack(&self, pack_id: &PackId) -> Result<Bytes, BlobError> {
        self.packs
            .get(pack_id)
            .map(|r| r.value().clone())
            .ok_or_else(|| BlobError::PackNotFound(pack_id.clone()))
    }

    fn has_pack(&self, pack_id: &PackId) -> bool {
        self.packs.contains_key(pack_id)
    }

    fn total_packs(&self) -> usize {
        self.packs.len()
    }

    fn total_bytes(&self) -> u64 {
        self.stored_bytes.load(Ordering::Relaxed)
    }
}

/// Disk-backed content-addressed immutable packfile blob store.
pub struct DiskBlobStore {
    root_dir: PathBuf,
    memory_index: DashMap<PackId, u64>,
    stored_bytes: AtomicU64,
}

impl DiskBlobStore {
    pub fn new(root_dir: impl AsRef<Path>) -> Result<Self, BlobError> {
        let root_dir = root_dir.as_ref().to_path_buf();
        fs::create_dir_all(&root_dir)?;
        let memory_index = DashMap::new();
        let mut total = 0u64;
        for entry in fs::read_dir(&root_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("pack") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    let len = entry.metadata()?.len();
                    memory_index.insert(PackId(stem.to_string()), len);
                    total += len;
                }
            }
        }
        Ok(Self {
            root_dir,
            memory_index,
            stored_bytes: AtomicU64::new(total),
        })
    }

    fn pack_path(&self, pack_id: &PackId) -> PathBuf {
        self.root_dir.join(format!("{}.pack", pack_id.as_str()))
    }
}

impl PackBlobStore for DiskBlobStore {
    fn put_pack(&self, pack_bytes: Bytes) -> Result<PackId, BlobError> {
        let pack_id = PackId::from_pack_bytes(&pack_bytes);
        self.put_pack_with_id(&pack_id, pack_bytes)?;
        Ok(pack_id)
    }

    fn put_pack_with_id(&self, pack_id: &PackId, pack_bytes: Bytes) -> Result<(), BlobError> {
        if self.memory_index.contains_key(pack_id) {
            return Ok(());
        }
        let path = self.pack_path(pack_id);
        fs::write(&path, &pack_bytes)?;
        let len = pack_bytes.len() as u64;
        if self.memory_index.insert(pack_id.clone(), len).is_none() {
            self.stored_bytes.fetch_add(len, Ordering::Relaxed);
        }
        Ok(())
    }

    fn get_pack(&self, pack_id: &PackId) -> Result<Bytes, BlobError> {
        let path = self.pack_path(pack_id);
        if !path.exists() {
            return Err(BlobError::PackNotFound(pack_id.clone()));
        }
        let data = fs::read(path)?;
        Ok(Bytes::from(data))
    }

    fn has_pack(&self, pack_id: &PackId) -> bool {
        self.memory_index.contains_key(pack_id)
    }

    fn total_packs(&self) -> usize {
        self.memory_index.len()
    }

    fn total_bytes(&self) -> u64 {
        self.stored_bytes.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExpressCacheStats {
    pub node_count: usize,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub bytes_from_cache: u64,
    pub bytes_from_origin: u64,
    pub hit_rate_pct: f64,
}

struct LruNodeCache {
    capacity_bytes: u64,
    used_bytes: u64,
    order: VecDeque<PackId>,
    entries: BTreeMap<PackId, Bytes>,
}

impl LruNodeCache {
    fn new(capacity_bytes: u64) -> Self {
        Self {
            capacity_bytes,
            used_bytes: 0,
            order: VecDeque::new(),
            entries: BTreeMap::new(),
        }
    }

    fn get(&mut self, pack_id: &PackId) -> Option<Bytes> {
        let bytes = self.entries.get(pack_id)?.clone();
        if let Some(pos) = self.order.iter().position(|x| x == pack_id) {
            self.order.remove(pos);
            self.order.push_back(pack_id.clone());
        }
        Some(bytes)
    }

    fn insert(&mut self, pack_id: PackId, data: Bytes) {
        let size = data.len() as u64;
        if size > self.capacity_bytes {
            return;
        }
        if self.entries.contains_key(&pack_id) {
            return;
        }
        while self.used_bytes + size > self.capacity_bytes {
            if let Some(evicted) = self.order.pop_front() {
                if let Some(old) = self.entries.remove(&evicted) {
                    self.used_bytes = self.used_bytes.saturating_sub(old.len() as u64);
                }
            } else {
                break;
            }
        }
        self.used_bytes += size;
        self.order.push_back(pack_id.clone());
        self.entries.insert(pack_id, data);
    }
}

/// Pando's "Express" consistent-hashed regional packfile cache layer.
/// Routes requests for a `PackId` to a deterministic node on a consistent hash ring.
pub struct ExpressCacheFleet {
    origin: Arc<dyn PackBlobStore>,
    ring: BTreeMap<u64, usize>,
    nodes: Vec<Mutex<LruNodeCache>>,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    bytes_from_cache: AtomicU64,
    bytes_from_origin: AtomicU64,
}

impl ExpressCacheFleet {
    pub fn new(
        origin: Arc<dyn PackBlobStore>,
        node_count: usize,
        per_node_capacity_bytes: u64,
    ) -> Self {
        let node_count = node_count.max(1);
        let mut ring = BTreeMap::new();
        let mut nodes = Vec::with_capacity(node_count);
        for node_idx in 0..node_count {
            nodes.push(Mutex::new(LruNodeCache::new(per_node_capacity_bytes)));
            // 64 virtual nodes per physical Express cache node
            for vnode in 0..64 {
                let key = format!("express-node-{node_idx}-vnode-{vnode}");
                let token = Self::hash_token(&key);
                ring.insert(token, node_idx);
            }
        }
        Self {
            origin,
            ring,
            nodes,
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            bytes_from_cache: AtomicU64::new(0),
            bytes_from_origin: AtomicU64::new(0),
        }
    }

    fn hash_token(key: &str) -> u64 {
        let mut hasher = Sha256::new();
        hasher.update(key.as_bytes());
        let digest = hasher.finalize();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&digest[..8]);
        u64::from_be_bytes(buf)
    }

    pub fn route_node(&self, pack_id: &PackId) -> usize {
        let token = Self::hash_token(pack_id.as_str());
        if let Some((_, &node_idx)) = self.ring.range(token..).next() {
            node_idx
        } else {
            *self.ring.values().next().unwrap_or(&0)
        }
    }

    /// Fetch a packfile through the Express consistent-hashed regional cache.
    pub fn fetch_pack(&self, pack_id: &PackId) -> Result<Bytes, BlobError> {
        let node_idx = self.route_node(pack_id);
        {
            let mut node = self.nodes[node_idx].lock();
            if let Some(cached) = node.get(pack_id) {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                self.bytes_from_cache
                    .fetch_add(cached.len() as u64, Ordering::Relaxed);
                return Ok(cached);
            }
        }
        // Cache miss -> pull from origin blob store and populate Express node
        let bytes = self.origin.get_pack(pack_id)?;
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
        self.bytes_from_origin
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        {
            let mut node = self.nodes[node_idx].lock();
            node.insert(pack_id.clone(), bytes.clone());
        }
        Ok(bytes)
    }

    /// Warm the Express cache on push if write-through warming is enabled.
    pub fn warm_pack(&self, pack_id: &PackId, bytes: Bytes) {
        let node_idx = self.route_node(pack_id);
        let mut node = self.nodes[node_idx].lock();
        node.insert(pack_id.clone(), bytes);
    }

    pub fn origin(&self) -> &Arc<dyn PackBlobStore> {
        &self.origin
    }

    pub fn stats(&self) -> ExpressCacheStats {
        let hits = self.cache_hits.load(Ordering::Relaxed);
        let misses = self.cache_misses.load(Ordering::Relaxed);
        let total = hits + misses;
        let hit_rate_pct = if total == 0 {
            100.0
        } else {
            (hits as f64 / total as f64) * 100.0
        };
        ExpressCacheStats {
            node_count: self.nodes.len(),
            cache_hits: hits,
            cache_misses: misses,
            bytes_from_cache: self.bytes_from_cache.load(Ordering::Relaxed),
            bytes_from_origin: self.bytes_from_origin.load(Ordering::Relaxed),
            hit_rate_pct,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_express_consistent_hash_cache_hit_rate() {
        let origin = Arc::new(MemoryBlobStore::new());
        let pack_id = origin.put_pack(Bytes::from_static(b"PACK-DATA-123")).unwrap();
        let express = ExpressCacheFleet::new(origin, 4, 10 * 1024 * 1024);

        // First read is a cache miss (origin fill)
        let b1 = express.fetch_pack(&pack_id).unwrap();
        assert_eq!(b1.as_ref(), b"PACK-DATA-123");

        // Subsequent 99 reads hit the same consistent-hashed Express node
        for _ in 0..99 {
            let b = express.fetch_pack(&pack_id).unwrap();
            assert_eq!(b.as_ref(), b"PACK-DATA-123");
        }

        let stats = express.stats();
        assert_eq!(stats.cache_misses, 1);
        assert_eq!(stats.cache_hits, 99);
        assert!((stats.hit_rate_pct - 99.0).abs() < 1e-6);
    }
}
