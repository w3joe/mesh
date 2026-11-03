use dashmap::DashMap;
use mesh_proto::{
    AgentIntentReceipt, CommitLocation, FileSemanticDelta, ObjectId, PackCover, PackId, RefPointer,
    RefVersion,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MetaError {
    #[error("CAS conflict on ref '{ref_name}': expected old OID {expected_old}, but current tip is {actual_current}")]
    CasConflict {
        ref_name: String,
        expected_old: ObjectId,
        actual_current: ObjectId,
    },
    #[error("unknown commit OID {0} in repository '{1}'")]
    UnknownCommit(ObjectId, String),
    #[error("ref '{1}' not found in repository '{0}'")]
    RefNotFound(String, String),
    #[error("missing cover for ref '{ref_name}' at version {version}")]
    MissingCover { ref_name: String, version: u64 },
    #[error("persistence io error: {0}")]
    Io(String),
}

#[derive(Clone, Debug)]
pub struct PushTransaction {
    pub repo_id: String,
    pub ref_name: String,
    pub expected_old_oid: ObjectId,
    pub new_commit_oid: ObjectId,
    pub introduced_commits: Vec<ObjectId>,
    pub l0_pack_id: PackId,
    pub pack_byte_size: u64,
    pub pack_object_count: u32,
    pub explicit_haves: Vec<ObjectId>,
    pub base_trunk_version: Option<u64>,
    pub intent_receipt: Option<AgentIntentReceipt>,
    pub semantic_deltas: Vec<FileSemanticDelta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchPlan {
    pub repo_id: String,
    pub selected_covers: Vec<PackCover>,
    pub metadata_reads: usize,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum JournalRecord {
    Push {
        pointer: RefPointer,
        ref_version: RefVersion,
        l0_cover: PackCover,
        commit_locations: Vec<CommitLocation>,
    },
    DeleteRef {
        repo_id: String,
        ref_name: String,
    },
    InsertCover {
        cover: PackCover,
    },
}

pub trait MeshMetadataStore: Send + Sync {
    /// Fetch the current pointer for a single branch ($O(1)$).
    fn get_ref_pointer(&self, repo_id: &str, ref_name: &str) -> Option<RefPointer>;

    /// Pando's Trunk-Only Ref Advertisement:
    /// Returns only the designated `trunk_ref` (e.g. `refs/heads/main`) plus an optional
    /// `target_ref` so push negotiation payload stays $O(1)$ (<256B) even with 1M branches.
    fn list_advertised_refs(
        &self,
        repo_id: &str,
        trunk_ref: &str,
        target_ref: Option<&str>,
    ) -> Vec<RefPointer>;

    /// Lists all refs in a repository (used by admin stats & the Semantic Conflict Radar).
    fn list_all_refs(&self, repo_id: &str) -> Vec<RefPointer>;

    /// Executes an $O(1)$ per-ref CAS transaction to publish a push.
    /// Pushes to distinct branches never contend on locks.
    fn commit_push(&self, tx: PushTransaction) -> Result<RefVersion, MetaError>;

    /// Atomically deletes a branch reference after verifying `expected_old_oid` via CAS.
    fn delete_ref(
        &self,
        repo_id: &str,
        ref_name: &str,
        expected_old_oid: ObjectId,
    ) -> Result<(), MetaError>;

    /// Lookup a specific `RefVersion` record.
    fn get_ref_version(&self, repo_id: &str, ref_name: &str, version: u64) -> Option<RefVersion>;

    /// List all `RefVersion` records for a branch in `[start_version, end_version]`.
    fn list_ref_versions(
        &self,
        repo_id: &str,
        ref_name: &str,
        start_version: u64,
        end_version: u64,
    ) -> Vec<RefVersion>;

    /// Resolve a commit hash to the earliest `(ref_name, ref_version)` that introduced it
    /// (with trunk `refs/heads/main` taking precedence once merged).
    fn resolve_commit(&self, repo_id: &str, commit_oid: &ObjectId) -> Option<CommitLocation>;

    /// Register a compacted `PackCover` at level `L1..L4` without deleting lower levels.
    fn insert_pack_cover(&self, cover: PackCover);

    /// List all `PackCover`s for a branch across all LSM levels.
    fn list_pack_covers(&self, repo_id: &str, ref_name: &str) -> Vec<PackCover>;

    /// Pando's $O(\log \Delta)$ Skip-List Fetch Planner:
    /// Maps client `have` and `want` commit OIDs to a minimal covering sequence of `PackCover`s
    /// across trunk dependencies and requested feature branches.
    fn plan_fetch_covers(
        &self,
        repo_id: &str,
        trunk_ref: &str,
        have_oids: &[ObjectId],
        want_oids: &[ObjectId],
    ) -> Result<FetchPlan, MetaError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RefKey {
    repo_id: String,
    ref_name: String,
}

#[derive(Default)]
struct BranchState {
    pointer: Option<RefPointer>,
    versions: BTreeMap<u64, RefVersion>,
    covers_by_start: BTreeMap<u64, Vec<PackCover>>,
}

/// High-concurrency Causal Metadata Store with per-ref lock striping, LSM skip-list covers,
/// and optional durable append-only journal persistence.
pub struct CausalMetadataStore {
    branches: DashMap<RefKey, Arc<Mutex<BranchState>>>,
    repo_refs: DashMap<String, DashMap<String, ()>>,
    commit_index: DashMap<(String, ObjectId), CommitLocation>,
    journal_file: Option<Mutex<std::fs::File>>,
    total_cas_commits: AtomicU64,
    total_cas_conflicts: AtomicU64,
}

impl Default for CausalMetadataStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CausalMetadataStore {
    pub fn new() -> Self {
        Self {
            branches: DashMap::new(),
            repo_refs: DashMap::new(),
            commit_index: DashMap::new(),
            journal_file: None,
            total_cas_commits: AtomicU64::new(0),
            total_cas_conflicts: AtomicU64::new(0),
        }
    }

    /// Opens or creates a durable journal-backed `CausalMetadataStore` at `journal_path`,
    /// replaying all historical ref versions, covers, and commit locations on startup.
    pub fn with_journal_path(journal_path: impl AsRef<Path>) -> Result<Self, MetaError> {
        let path: PathBuf = journal_path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| MetaError::Io(e.to_string()))?;
        }

        let store = Self::new();
        if path.exists() {
            let file =
                fs::File::open(&path).map_err(|e| MetaError::Io(format!("open journal: {e}")))?;
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let line = line.map_err(|e| MetaError::Io(e.to_string()))?;
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(record) = serde_json::from_str::<JournalRecord>(&line) {
                    store.apply_journal_record(record);
                }
            }
        }

        let writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| MetaError::Io(format!("open journal append: {e}")))?;

        Ok(Self {
            branches: store.branches,
            repo_refs: store.repo_refs,
            commit_index: store.commit_index,
            journal_file: Some(Mutex::new(writer)),
            total_cas_commits: store.total_cas_commits,
            total_cas_conflicts: store.total_cas_conflicts,
        })
    }

    fn apply_journal_record(&self, record: JournalRecord) {
        match record {
            JournalRecord::Push {
                pointer,
                ref_version,
                l0_cover,
                commit_locations,
            } => {
                let lock = self.branch_lock(&pointer.repo_id, &pointer.ref_name);
                {
                    let mut state = lock.lock();
                    let v = ref_version.version;
                    state.pointer = Some(pointer.clone());
                    state.versions.insert(v, ref_version);
                    state.covers_by_start.entry(v).or_default().push(l0_cover);
                }
                self.repo_refs
                    .entry(pointer.repo_id.clone())
                    .or_default()
                    .insert(pointer.ref_name.clone(), ());
                for loc in commit_locations {
                    self.index_commit_location(loc);
                }
            }
            JournalRecord::DeleteRef { repo_id, ref_name } => {
                let lock = self.branch_lock(&repo_id, &ref_name);
                {
                    let mut state = lock.lock();
                    state.pointer = None;
                }
                if let Some(refs) = self.repo_refs.get(&repo_id) {
                    refs.remove(&ref_name);
                }
            }
            JournalRecord::InsertCover { cover } => {
                self.insert_pack_cover_in_memory(cover);
            }
        }
    }

    fn append_journal(&self, record: &JournalRecord) -> Result<(), MetaError> {
        if let Some(ref mutex) = self.journal_file {
            let mut line =
                serde_json::to_string(record).map_err(|e| MetaError::Io(e.to_string()))?;
            line.push('\n');
            let mut file = mutex.lock();
            file.write_all(line.as_bytes())
                .map_err(|e| MetaError::Io(e.to_string()))?;
            file.flush().map_err(|e| MetaError::Io(e.to_string()))?;
        }
        Ok(())
    }

    fn index_commit_location(&self, loc: CommitLocation) {
        let key = (loc.repo_id.clone(), loc.commit_oid);
        let is_trunk = loc.ref_name == "refs/heads/main";
        match self.commit_index.entry(key) {
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(loc);
            }
            dashmap::mapref::entry::Entry::Occupied(mut o) => {
                // Bug 8 Fix: If a commit was previously indexed on a topic branch and is now
                // pushed/merged onto `refs/heads/main`, promote its location to `refs/heads/main`!
                if is_trunk && o.get().ref_name != "refs/heads/main" {
                    o.insert(loc);
                }
            }
        }
    }

    fn branch_lock(&self, repo_id: &str, ref_name: &str) -> Arc<Mutex<BranchState>> {
        let key = RefKey {
            repo_id: repo_id.to_string(),
            ref_name: ref_name.to_string(),
        };
        self.branches
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(BranchState::default())))
            .value()
            .clone()
    }

    pub fn total_cas_commits(&self) -> u64 {
        self.total_cas_commits.load(Ordering::Relaxed)
    }

    pub fn total_cas_conflicts(&self) -> u64 {
        self.total_cas_conflicts.load(Ordering::Relaxed)
    }

    fn insert_pack_cover_in_memory(&self, cover: PackCover) {
        let lock = self.branch_lock(&cover.repo_id, &cover.ref_name);
        let mut state = lock.lock();
        let list = state
            .covers_by_start
            .entry(cover.start_version)
            .or_default();
        if !list
            .iter()
            .any(|c| c.level == cover.level && c.end_version == cover.end_version)
        {
            list.push(cover);
        }
    }

    /// Greedily covers the version range `(have_version, want_version]` on a single branch
    /// using the highest-level LSM `PackCover`s available (Skip-List traversal in $O(\log \Delta)$).
    fn select_interval_covers(
        state: &BranchState,
        ref_name: &str,
        have_version: u64,
        want_version: u64,
        metadata_reads: &mut usize,
    ) -> Result<Vec<PackCover>, MetaError> {
        if have_version >= want_version {
            return Ok(Vec::new());
        }
        let mut selected = Vec::new();
        let mut cursor = have_version + 1;

        while cursor <= want_version {
            *metadata_reads += 1;
            let mut best_cover: Option<&PackCover> = None;

            if let Some(covers_at_cursor) = state.covers_by_start.get(&cursor) {
                for c in covers_at_cursor {
                    if c.end_version <= want_version {
                        match best_cover {
                            None => best_cover = Some(c),
                            Some(prev) if c.end_version > prev.end_version => best_cover = Some(c),
                            Some(prev)
                                if c.end_version == prev.end_version && c.level > prev.level =>
                            {
                                best_cover = Some(c)
                            }
                            _ => {}
                        }
                    }
                }
            }

            if let Some(chosen) = best_cover {
                cursor = chosen.end_version + 1;
                selected.push(chosen.clone());
            } else {
                return Err(MetaError::MissingCover {
                    ref_name: ref_name.to_string(),
                    version: cursor,
                });
            }
        }

        Ok(selected)
    }
}

impl MeshMetadataStore for CausalMetadataStore {
    fn get_ref_pointer(&self, repo_id: &str, ref_name: &str) -> Option<RefPointer> {
        let key = RefKey {
            repo_id: repo_id.to_string(),
            ref_name: ref_name.to_string(),
        };
        let lock = self.branches.get(&key)?.value().clone();
        let state = lock.lock();
        state.pointer.clone()
    }

    fn list_advertised_refs(
        &self,
        repo_id: &str,
        trunk_ref: &str,
        target_ref: Option<&str>,
    ) -> Vec<RefPointer> {
        let mut out = Vec::with_capacity(2);
        if let Some(trunk) = self.get_ref_pointer(repo_id, trunk_ref) {
            out.push(trunk);
        }
        if let Some(target) = target_ref {
            if target != trunk_ref {
                if let Some(ptr) = self.get_ref_pointer(repo_id, target) {
                    out.push(ptr);
                }
            }
        }
        out
    }

    fn list_all_refs(&self, repo_id: &str) -> Vec<RefPointer> {
        let Some(ref_set) = self.repo_refs.get(repo_id) else {
            return Vec::new();
        };
        let mut pointers = Vec::new();
        for entry in ref_set.iter() {
            if let Some(ptr) = self.get_ref_pointer(repo_id, entry.key()) {
                pointers.push(ptr);
            }
        }
        pointers.sort_by(|a, b| a.ref_name.cmp(&b.ref_name));
        pointers
    }

    fn commit_push(&self, tx: PushTransaction) -> Result<RefVersion, MetaError> {
        let trunk_ver = if tx.base_trunk_version.is_some() {
            tx.base_trunk_version
        } else if tx.ref_name != "refs/heads/main" {
            self.get_ref_pointer(&tx.repo_id, "refs/heads/main")
                .map(|p| p.current_version)
        } else {
            None
        };

        let branch_mutex = self.branch_lock(&tx.repo_id, &tx.ref_name);
        let (ptr, ref_version, l0_cover) = {
            let mut state = branch_mutex.lock();
            let current_oid = state
                .pointer
                .as_ref()
                .map(|p| p.tip_commit_oid)
                .unwrap_or(ObjectId::ZERO);

            if current_oid != tx.expected_old_oid {
                self.total_cas_conflicts.fetch_add(1, Ordering::Relaxed);
                return Err(MetaError::CasConflict {
                    ref_name: tx.ref_name,
                    expected_old: tx.expected_old_oid,
                    actual_current: current_oid,
                });
            }

            let next_version = state
                .version_counter_next();

            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

            let ptr = RefPointer {
                repo_id: tx.repo_id.clone(),
                ref_name: tx.ref_name.clone(),
                current_version: next_version,
                tip_commit_oid: tx.new_commit_oid,
                base_trunk_version: trunk_ver,
            };

            let rv = RefVersion {
                repo_id: tx.repo_id.clone(),
                ref_name: tx.ref_name.clone(),
                version: next_version,
                old_commit_oid: tx.expected_old_oid,
                new_commit_oid: tx.new_commit_oid,
                l0_pack_id: tx.l0_pack_id.clone(),
                explicit_haves: tx.explicit_haves,
                base_trunk_version: trunk_ver,
                created_at_ms: now_ms,
                intent_receipt: tx.intent_receipt,
                semantic_deltas: tx.semantic_deltas,
            };

            let l0_cover = PackCover {
                repo_id: tx.repo_id.clone(),
                ref_name: tx.ref_name.clone(),
                level: 0,
                start_version: next_version,
                end_version: next_version,
                pack_id: tx.l0_pack_id,
                byte_size: tx.pack_byte_size,
                object_count: tx.pack_object_count,
            };

            state.pointer = Some(ptr.clone());
            state.versions.insert(next_version, rv.clone());
            state
                .covers_by_start
                .entry(next_version)
                .or_default()
                .push(l0_cover.clone());

            (ptr, rv, l0_cover)
        };

        self.repo_refs
            .entry(tx.repo_id.clone())
            .or_default()
            .insert(tx.ref_name.clone(), ());

        let mut commits_to_index = tx.introduced_commits;
        if !commits_to_index.contains(&tx.new_commit_oid) {
            commits_to_index.push(tx.new_commit_oid);
        }
        let mut commit_locations = Vec::new();
        for oid in commits_to_index {
            if !oid.is_zero() {
                let loc = CommitLocation {
                    repo_id: tx.repo_id.clone(),
                    commit_oid: oid,
                    ref_name: tx.ref_name.clone(),
                    ref_version: ref_version.version,
                };
                self.index_commit_location(loc.clone());
                commit_locations.push(loc);
            }
        }

        self.append_journal(&JournalRecord::Push {
            pointer: ptr,
            ref_version: ref_version.clone(),
            l0_cover,
            commit_locations,
        })?;

        self.total_cas_commits.fetch_add(1, Ordering::Relaxed);
        Ok(ref_version)
    }

    fn delete_ref(
        &self,
        repo_id: &str,
        ref_name: &str,
        expected_old_oid: ObjectId,
    ) -> Result<(), MetaError> {
        let branch_mutex = self.branch_lock(repo_id, ref_name);
        {
            let mut state = branch_mutex.lock();
            let current_oid = state
                .pointer
                .as_ref()
                .map(|p| p.tip_commit_oid)
                .unwrap_or(ObjectId::ZERO);

            if current_oid != expected_old_oid {
                self.total_cas_conflicts.fetch_add(1, Ordering::Relaxed);
                return Err(MetaError::CasConflict {
                    ref_name: ref_name.to_string(),
                    expected_old: expected_old_oid,
                    actual_current: current_oid,
                });
            }
            state.pointer = None;
        }
        if let Some(refs) = self.repo_refs.get(repo_id) {
            refs.remove(ref_name);
        }
        self.append_journal(&JournalRecord::DeleteRef {
            repo_id: repo_id.to_string(),
            ref_name: ref_name.to_string(),
        })?;
        self.total_cas_commits.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn get_ref_version(&self, repo_id: &str, ref_name: &str, version: u64) -> Option<RefVersion> {
        let key = RefKey {
            repo_id: repo_id.to_string(),
            ref_name: ref_name.to_string(),
        };
        let lock = self.branches.get(&key)?.value().clone();
        let state = lock.lock();
        state.versions.get(&version).cloned()
    }

    fn list_ref_versions(
        &self,
        repo_id: &str,
        ref_name: &str,
        start_version: u64,
        end_version: u64,
    ) -> Vec<RefVersion> {
        let key = RefKey {
            repo_id: repo_id.to_string(),
            ref_name: ref_name.to_string(),
        };
        let Some(lock) = self.branches.get(&key) else {
            return Vec::new();
        };
        let state = lock.value().lock();
        state
            .versions
            .range(start_version..=end_version)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn resolve_commit(&self, repo_id: &str, commit_oid: &ObjectId) -> Option<CommitLocation> {
        self.commit_index
            .get(&(repo_id.to_string(), *commit_oid))
            .map(|r| r.value().clone())
    }

    fn insert_pack_cover(&self, cover: PackCover) {
        self.insert_pack_cover_in_memory(cover.clone());
        let _ = self.append_journal(&JournalRecord::InsertCover { cover });
    }

    fn list_pack_covers(&self, repo_id: &str, ref_name: &str) -> Vec<PackCover> {
        let key = RefKey {
            repo_id: repo_id.to_string(),
            ref_name: ref_name.to_string(),
        };
        let Some(lock) = self.branches.get(&key) else {
            return Vec::new();
        };
        let state = lock.value().lock();
        let mut all = Vec::new();
        for covers in state.covers_by_start.values() {
            all.extend(covers.iter().cloned());
        }
        all.sort_by_key(|c| (std::cmp::Reverse(c.level), c.start_version, c.end_version));
        all
    }

    fn plan_fetch_covers(
        &self,
        repo_id: &str,
        trunk_ref: &str,
        have_oids: &[ObjectId],
        want_oids: &[ObjectId],
    ) -> Result<FetchPlan, MetaError> {
        let mut metadata_reads = 0usize;
        let mut have_trunk_version = 0u64;
        let mut have_branch_versions: BTreeMap<String, u64> = BTreeMap::new();

        for have in have_oids {
            if have.is_zero() {
                continue;
            }
            metadata_reads += 1;
            if let Some(loc) = self.resolve_commit(repo_id, have) {
                if loc.ref_name == trunk_ref {
                    have_trunk_version = have_trunk_version.max(loc.ref_version);
                } else {
                    let entry = have_branch_versions.entry(loc.ref_name.clone()).or_insert(0);
                    *entry = (*entry).max(loc.ref_version);
                    if let Some(rv) = self.get_ref_version(repo_id, &loc.ref_name, loc.ref_version)
                    {
                        if let Some(base_v) = rv.base_trunk_version {
                            have_trunk_version = have_trunk_version.max(base_v);
                        }
                    }
                }
            }
        }

        let mut required_trunk_version = have_trunk_version;
        let mut wanted_branch_targets: BTreeMap<String, u64> = BTreeMap::new();

        for want in want_oids {
            if want.is_zero() {
                continue;
            }
            metadata_reads += 1;
            let loc = self
                .resolve_commit(repo_id, want)
                .ok_or_else(|| MetaError::UnknownCommit(*want, repo_id.to_string()))?;

            if loc.ref_name == trunk_ref {
                required_trunk_version = required_trunk_version.max(loc.ref_version);
            } else {
                let entry = wanted_branch_targets
                    .entry(loc.ref_name.clone())
                    .or_insert(0);
                *entry = (*entry).max(loc.ref_version);
                metadata_reads += 1;
                if let Some(rv) = self.get_ref_version(repo_id, &loc.ref_name, loc.ref_version) {
                    if let Some(base_v) = rv.base_trunk_version {
                        required_trunk_version = required_trunk_version.max(base_v);
                    }
                }
            }
        }

        let mut selected_covers = Vec::new();
        let mut seen_packs = HashSet::new();

        if required_trunk_version > have_trunk_version {
            let key = RefKey {
                repo_id: repo_id.to_string(),
                ref_name: trunk_ref.to_string(),
            };
            let lock = self
                .branches
                .get(&key)
                .ok_or_else(|| MetaError::RefNotFound(repo_id.to_string(), trunk_ref.to_string()))?
                .value()
                .clone();
            let state = lock.lock();
            let trunk_covers = Self::select_interval_covers(
                &state,
                trunk_ref,
                have_trunk_version,
                required_trunk_version,
                &mut metadata_reads,
            )?;
            for c in trunk_covers {
                if seen_packs.insert(c.pack_id.clone()) {
                    selected_covers.push(c);
                }
            }
        }

        for (branch_ref, target_ver) in wanted_branch_targets {
            let held_ver = have_branch_versions.get(&branch_ref).copied().unwrap_or(0);
            if target_ver <= held_ver {
                continue;
            }
            let key = RefKey {
                repo_id: repo_id.to_string(),
                ref_name: branch_ref.clone(),
            };
            let lock = self
                .branches
                .get(&key)
                .ok_or_else(|| MetaError::RefNotFound(repo_id.to_string(), branch_ref.clone()))?
                .value()
                .clone();
            let state = lock.lock();
            metadata_reads += 1;
            if let Some(covers_at_tip) = state.covers_by_start.get(&target_ver) {
                if let Some(tip_cover) = covers_at_tip.first() {
                    if seen_packs.insert(tip_cover.pack_id.clone()) {
                        selected_covers.push(tip_cover.clone());
                        continue;
                    }
                }
            }
            let branch_covers = Self::select_interval_covers(
                &state,
                &branch_ref,
                held_ver,
                target_ver,
                &mut metadata_reads,
            )?;
            for c in branch_covers {
                if seen_packs.insert(c.pack_id.clone()) {
                    selected_covers.push(c);
                }
            }
        }

        let total_bytes = selected_covers.iter().map(|c| c.byte_size).sum();
        Ok(FetchPlan {
            repo_id: repo_id.to_string(),
            selected_covers,
            metadata_reads,
            total_bytes,
        })
    }
}

impl BranchState {
    fn version_counter_next(&self) -> u64 {
        self.versions.keys().next_back().copied().unwrap_or(0) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_oid(n: u64) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[12..20].copy_from_slice(&n.to_be_bytes());
        ObjectId::from_bytes(bytes)
    }

    #[test]
    fn test_per_ref_cas_and_skip_list_cover_planning() {
        let store = CausalMetadataStore::new();
        let repo = "openai/monorepo";

        let mut prev_oid = ObjectId::ZERO;
        for v in 1..=32u64 {
            let new_oid = fake_oid(v);
            store
                .commit_push(PushTransaction {
                    repo_id: repo.to_string(),
                    ref_name: "refs/heads/main".to_string(),
                    expected_old_oid: prev_oid,
                    new_commit_oid: new_oid,
                    introduced_commits: vec![new_oid],
                    l0_pack_id: PackId(format!("pack-l0-{v}")),
                    pack_byte_size: 1000,
                    pack_object_count: 5,
                    explicit_haves: vec![],
                    base_trunk_version: None,
                    intent_receipt: None,
                    semantic_deltas: vec![],
                })
                .unwrap();
            prev_oid = new_oid;
        }

        store.insert_pack_cover(PackCover {
            repo_id: repo.to_string(),
            ref_name: "refs/heads/main".to_string(),
            level: 2,
            start_version: 5,
            end_version: 20,
            pack_id: PackId("pack-l2-5-20".to_string()),
            byte_size: 8000,
            object_count: 50,
        });
        store.insert_pack_cover(PackCover {
            repo_id: repo.to_string(),
            ref_name: "refs/heads/main".to_string(),
            level: 1,
            start_version: 21,
            end_version: 28,
            pack_id: PackId("pack-l1-21-28".to_string()),
            byte_size: 4000,
            object_count: 25,
        });

        let topic_oid = fake_oid(999);
        store
            .commit_push(PushTransaction {
                repo_id: repo.to_string(),
                ref_name: "refs/heads/agent-99".to_string(),
                expected_old_oid: ObjectId::ZERO,
                new_commit_oid: topic_oid,
                introduced_commits: vec![topic_oid],
                l0_pack_id: PackId("pack-topic-99".to_string()),
                pack_byte_size: 1500,
                pack_object_count: 4,
                explicit_haves: vec![fake_oid(31)],
                base_trunk_version: Some(31),
                intent_receipt: None,
                semantic_deltas: vec![],
            })
            .unwrap();

        let plan = store
            .plan_fetch_covers(repo, "refs/heads/main", &[fake_oid(4)], &[topic_oid])
            .unwrap();

        let pack_ids: Vec<&str> = plan
            .selected_covers
            .iter()
            .map(|c| c.pack_id.as_str())
            .collect();
        assert_eq!(
            pack_ids,
            vec![
                "pack-l2-5-20",
                "pack-l1-21-28",
                "pack-l0-29",
                "pack-l0-30",
                "pack-l0-31",
                "pack-topic-99"
            ]
        );

        // Verify trunk commit promotion when `topic_oid` is fast-forwarded onto `refs/heads/main`
        store
            .commit_push(PushTransaction {
                repo_id: repo.to_string(),
                ref_name: "refs/heads/main".to_string(),
                expected_old_oid: fake_oid(32),
                new_commit_oid: topic_oid,
                introduced_commits: vec![topic_oid],
                l0_pack_id: PackId("pack-l0-33".to_string()),
                pack_byte_size: 1500,
                pack_object_count: 4,
                explicit_haves: vec![],
                base_trunk_version: None,
                intent_receipt: None,
                semantic_deltas: vec![],
            })
            .unwrap();
        let promoted = store.resolve_commit(repo, &topic_oid).unwrap();
        assert_eq!(promoted.ref_name, "refs/heads/main");
        assert_eq!(promoted.ref_version, 33);

        // Verify branch deletion
        store
            .delete_ref(repo, "refs/heads/agent-99", topic_oid)
            .unwrap();
        assert!(store.get_ref_pointer(repo, "refs/heads/agent-99").is_none());
    }
}
