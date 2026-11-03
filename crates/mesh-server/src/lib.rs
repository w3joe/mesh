use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use dashmap::DashMap;
use mesh_blob::{
    DiskBlobStore, ExpressCacheFleet, ExpressCacheStats, MemoryBlobStore, PackBlobStore,
};
use mesh_meta::{CausalMetadataStore, MeshMetadataStore, MetaError, PushTransaction};
use mesh_pack::{
    build_commit_object, build_trees_from_file_entries, compact_packs, decode_and_validate_pack,
    encode_packfile, extract_commit_file_entries, parse_commit, FileTreeEntry, GitObject,
    GitObjectType,
};
use mesh_pktline::{
    build_info_refs_advertisement, build_protocol_v2_capability_advertisement,
    encode_packfile_uri_response, encode_protocol_v2_fetch_response,
    encode_protocol_v2_ls_refs_response, encode_receive_pack_report,
    encode_standard_upload_pack_response, encode_v1_negotiation_only, parse_git_protocol_header,
    parse_receive_pack_request, parse_upload_pack_request, RefUpdateResult,
};
use mesh_proto::{
    AgentIntentReceipt, FileSemanticDelta, ObjectId, PackCover, PackId, RefVersion,
    SemanticConflict,
};
use mesh_semantic::{
    compute_file_semantic_delta, cst_merge_3way, detect_semantic_conflicts, CstMergeOutcome,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("pack validation error: {0}")]
    Pack(#[from] mesh_pack::PackError),
    #[error("metadata error: {0}")]
    Meta(#[from] MetaError),
    #[error("blob store error: {0}")]
    Blob(#[from] mesh_blob::BlobError),
    #[error("semantic conflict prevented landing: {0}")]
    SemanticConflict(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VirtualWorkspace {
    pub workspace_id: String,
    pub repo_id: String,
    pub agent_id: String,
    pub base_commit_oid: ObjectId,
    pub base_trunk_version: u64,
    pub files: BTreeMap<String, String>,
}

/// Core Pando + Semantic Agent VCS Engine.
pub struct MeshEngine {
    pub meta: Arc<CausalMetadataStore>,
    pub blobs: Arc<dyn PackBlobStore>,
    pub express: Arc<ExpressCacheFleet>,
    pub objects: DashMap<(String, ObjectId), GitObject>,
    pub workspaces: DashMap<String, VirtualWorkspace>,
    pub upstream_git_url: Option<String>,
}

impl Default for MeshEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MeshEngine {
    pub fn new() -> Self {
        let blobs: Arc<dyn PackBlobStore> = Arc::new(MemoryBlobStore::new());
        let express = Arc::new(ExpressCacheFleet::new(
            blobs.clone(),
            8,
            256 * 1024 * 1024,
        ));
        Self {
            meta: Arc::new(CausalMetadataStore::new()),
            blobs,
            express,
            objects: DashMap::new(),
            workspaces: DashMap::new(),
            upstream_git_url: None,
        }
    }

    /// Initializes a durable disk-backed `MeshEngine` storing immutable `.pack` files under
    /// `<data_dir>/packs` and transactional ref metadata under `<data_dir>/meta.journal.jsonl`.
    pub fn with_data_dir(
        data_dir: impl AsRef<FsPath>,
        upstream_git_url: Option<String>,
    ) -> Result<Self, EngineError> {
        let root = data_dir.as_ref();
        let packs_dir = root.join("packs");
        let journal_path = root.join("meta.journal.jsonl");

        let disk_blobs = Arc::new(DiskBlobStore::new(&packs_dir)?);
        let blobs: Arc<dyn PackBlobStore> = disk_blobs;
        let express = Arc::new(ExpressCacheFleet::new(
            blobs.clone(),
            8,
            256 * 1024 * 1024,
        ));
        let meta = Arc::new(CausalMetadataStore::with_journal_path(&journal_path)?);
        let objects = DashMap::new();

        let engine = Self {
            meta,
            blobs,
            express,
            objects,
            workspaces: DashMap::new(),
            upstream_git_url,
        };
        engine.rehydrate_objects_from_packs(&packs_dir);
        Ok(engine)
    }

    fn rehydrate_objects_from_packs(&self, packs_dir: &FsPath) {
        let Ok(entries) = std::fs::read_dir(packs_dir) else {
            return;
        };
        let mut pack_buffers = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("pack") {
                if let Ok(bytes) = std::fs::read(&path) {
                    pack_buffers.push(Bytes::from(bytes));
                }
            }
        }
        // Decode all stored packfiles and populate `(repo_id, oid)` for known repos
        let mut decoded_all = Vec::new();
        for buf in &pack_buffers {
            if let Ok(dec) = decode_and_validate_pack(buf, |oid| {
                decoded_all
                    .iter()
                    .find(|(id, _): &&(ObjectId, GitObject)| id == oid)
                    .map(|(_, o)| o.clone())
            }) {
                for obj in dec.objects {
                    decoded_all.push((obj.oid, obj));
                }
            }
        }
        // Associate objects with global wildcard `("*", oid)` fallback so any repo can look them up
        for (oid, obj) in decoded_all {
            self.objects.insert(("*".to_string(), oid), obj);
        }
    }

    pub fn get_object(&self, repo_id: &str, oid: &ObjectId) -> Option<GitObject> {
        self.objects
            .get(&(repo_id.to_string(), *oid))
            .or_else(|| self.objects.get(&("*".to_string(), *oid)))
            .map(|r| r.value().clone())
    }

    /// Single-pass validation, pack storage, semantic lifting, and causal per-ref CAS commit.
    /// Supports branch deletion (`new_commit_oid.is_zero()`) and server-side CST-commutative
    /// landing via `refs/mesh/land/<branch>`.
    pub fn process_push(
        &self,
        repo_id: &str,
        ref_name: &str,
        expected_old_oid: ObjectId,
        new_commit_oid: ObjectId,
        pack_bytes: Bytes,
        intent_receipt: Option<AgentIntentReceipt>,
    ) -> Result<(RefVersion, Option<String>), EngineError> {
        // Bug 5 Fix: Handle branch deletion (`new_commit_oid == ZERO`) without decoding an empty pack
        if new_commit_oid.is_zero() {
            self.meta
                .delete_ref(repo_id, ref_name, expected_old_oid)?;
            let tombstone = RefVersion {
                repo_id: repo_id.to_string(),
                ref_name: ref_name.to_string(),
                version: 0,
                old_commit_oid: expected_old_oid,
                new_commit_oid: ObjectId::ZERO,
                l0_pack_id: PackId("deleted".to_string()),
                explicit_haves: vec![],
                base_trunk_version: None,
                created_at_ms: 0,
                intent_receipt,
                semantic_deltas: vec![],
            };
            return Ok((tombstone, Some(format!("Deleted ref {ref_name}"))));
        }

        let decoded = if pack_bytes.is_empty() {
            mesh_pack::DecodedPack {
                objects: Vec::new(),
                index_entries: Vec::new(),
                pack_checksum: ObjectId::ZERO,
            }
        } else {
            decode_and_validate_pack(&pack_bytes, |oid| self.get_object(repo_id, oid))?
        };

        let mut introduced_commits = Vec::new();
        for obj in &decoded.objects {
            if obj.obj_type == GitObjectType::Commit {
                introduced_commits.push(obj.oid);
            }
            self.objects
                .insert((repo_id.to_string(), obj.oid), obj.clone());
        }

        // Check if this is a server-side AST-commutative landing request (`refs/mesh/land/<target>`)
        if let Some(short_target) = ref_name.strip_prefix("refs/mesh/land/") {
            let target_ref = if short_target.starts_with("refs/") {
                short_target.to_string()
            } else {
                format!("refs/heads/{short_target}")
            };
            return self.land_commit_onto_target(
                repo_id,
                &target_ref,
                new_commit_oid,
                intent_receipt,
            );
        }

        let l0_pack_id = self.blobs.put_pack(pack_bytes.clone())?;
        self.express.warm_pack(&l0_pack_id, pack_bytes.clone());

        // Bug 9 Fix: For a new topic branch (`expected_old_oid == ZERO`), diff against the trunk
        // base commit (`refs/heads/main`) so multi-commit branch pushes capture all branch deltas.
        let diff_base_oid = if !expected_old_oid.is_zero() {
            expected_old_oid
        } else if ref_name != "refs/heads/main" {
            self.meta
                .get_ref_pointer(repo_id, "refs/heads/main")
                .map(|p| p.tip_commit_oid)
                .or_else(|| {
                    self.get_object(repo_id, &new_commit_oid)
                        .and_then(|o| parse_commit(&o.data).ok())
                        .and_then(|c| c.parent_oids.first().copied())
                })
                .unwrap_or(ObjectId::ZERO)
        } else if let Some(commit_obj) = self.get_object(repo_id, &new_commit_oid) {
            parse_commit(&commit_obj.data)
                .ok()
                .and_then(|c| c.parent_oids.first().copied())
                .unwrap_or(ObjectId::ZERO)
        } else {
            ObjectId::ZERO
        };

        let semantic_deltas =
            self.compute_commit_semantic_deltas(repo_id, &diff_base_oid, &new_commit_oid);

        let rv = self.meta.commit_push(PushTransaction {
            repo_id: repo_id.to_string(),
            ref_name: ref_name.to_string(),
            expected_old_oid,
            new_commit_oid,
            introduced_commits,
            l0_pack_id,
            pack_byte_size: pack_bytes.len() as u64,
            pack_object_count: decoded.objects.len() as u32,
            explicit_haves: vec![],
            base_trunk_version: None,
            intent_receipt,
            semantic_deltas,
        })?;

        Ok((rv, None))
    }

    /// Server-side CST-Commutative Landing (`git push origin HEAD:refs/mesh/land/main`).
    fn land_commit_onto_target(
        &self,
        repo_id: &str,
        target_ref: &str,
        pushed_commit_oid: ObjectId,
        intent_receipt: Option<AgentIntentReceipt>,
    ) -> Result<(RefVersion, Option<String>), EngineError> {
        let lookup = |oid: &ObjectId| self.get_object(repo_id, oid);
        let current_target_ptr = self.meta.get_ref_pointer(repo_id, target_ref);
        let current_target_oid = current_target_ptr
            .as_ref()
            .map(|p| p.tip_commit_oid)
            .unwrap_or(ObjectId::ZERO);

        let pushed_commit_obj = lookup(&pushed_commit_oid)
            .ok_or_else(|| MetaError::UnknownCommit(pushed_commit_oid, repo_id.to_string()))?;
        let parsed_pushed = parse_commit(&pushed_commit_obj.data)?;
        let base_oid = parsed_pushed
            .parent_oids
            .first()
            .copied()
            .unwrap_or(ObjectId::ZERO);

        // Fast-forward case
        if current_target_oid == base_oid {
            let mut commit_objs = Vec::new();
            let files = extract_commit_file_entries(&pushed_commit_oid, &lookup)?;
            for entry in files.values() {
                if let Some(b) = lookup(&entry.oid) {
                    commit_objs.push(b);
                }
            }
            let (_, mut trees) = build_trees_from_file_entries(&files);
            commit_objs.append(&mut trees);
            commit_objs.push(pushed_commit_obj.clone());

            let pack_bytes = encode_packfile(&commit_objs);
            let l0_pack_id = self.blobs.put_pack(pack_bytes.clone())?;
            self.express.warm_pack(&l0_pack_id, pack_bytes.clone());
            let deltas =
                self.compute_commit_semantic_deltas(repo_id, &base_oid, &pushed_commit_oid);

            let rv = self.meta.commit_push(PushTransaction {
                repo_id: repo_id.to_string(),
                ref_name: target_ref.to_string(),
                expected_old_oid: current_target_oid,
                new_commit_oid: pushed_commit_oid,
                introduced_commits: vec![pushed_commit_oid],
                l0_pack_id,
                pack_byte_size: pack_bytes.len() as u64,
                pack_object_count: commit_objs.len() as u32,
                explicit_haves: vec![],
                base_trunk_version: None,
                intent_receipt,
                semantic_deltas: deltas,
            })?;
            return Ok((rv, Some(format!("Fast-forward landed onto {target_ref}"))));
        }

        // Non-fast-forward case: run Semantic Conflict Radar + CST-Commutative 3-Way Merge
        let trunk_deltas =
            self.compute_commit_semantic_deltas(repo_id, &base_oid, &current_target_oid);
        let incoming_deltas =
            self.compute_commit_semantic_deltas(repo_id, &base_oid, &pushed_commit_oid);

        let semantic_conflicts = detect_semantic_conflicts(
            target_ref,
            &trunk_deltas,
            "incoming-land",
            &incoming_deltas,
        );
        if !semantic_conflicts.is_empty() {
            return Err(EngineError::SemanticConflict(
                semantic_conflicts[0].description.clone(),
            ));
        }

        // Bug 4 Fix: Use `extract_commit_file_entries` so executable (`100755`) & symlink (`120000`) modes are preserved!
        let base_files = extract_commit_file_entries(&base_oid, &lookup)?;
        let trunk_files = extract_commit_file_entries(&current_target_oid, &lookup)?;
        let incoming_files = extract_commit_file_entries(&pushed_commit_oid, &lookup)?;

        let mut all_paths: BTreeMap<String, ()> = BTreeMap::new();
        for k in base_files
            .keys()
            .chain(trunk_files.keys())
            .chain(incoming_files.keys())
        {
            all_paths.insert(k.clone(), ());
        }

        let mut merged_file_entries: BTreeMap<String, FileTreeEntry> = BTreeMap::new();
        let mut spliced_symbols = Vec::new();

        for path in all_paths.keys() {
            let b_entry = base_files.get(path);
            let t_entry = trunk_files.get(path);
            let i_entry = incoming_files.get(path);

            match (b_entry, t_entry, i_entry) {
                (_, Some(t), Some(i)) if t == i => {
                    merged_file_entries.insert(path.clone(), t.clone());
                }
                (Some(b), Some(t), Some(i)) if b == t => {
                    merged_file_entries.insert(path.clone(), i.clone());
                }
                (Some(b), Some(t), Some(i)) if b == i => {
                    merged_file_entries.insert(path.clone(), t.clone());
                }
                (None, Some(t), None) => {
                    merged_file_entries.insert(path.clone(), t.clone());
                }
                (None, None, Some(i)) => {
                    merged_file_entries.insert(path.clone(), i.clone());
                }
                // Bug 2 Fix: Explicit Modify/Delete and Delete/Modify conflict checks!
                (Some(b), Some(t), None) => {
                    if b.oid != t.oid {
                        return Err(EngineError::SemanticConflict(format!(
                            "FileModifyDeleteConflict on '{path}': modified on '{target_ref}' but deleted by incoming branch"
                        )));
                    }
                    // Otherwise b.oid == t.oid: clean deletion by incoming branch
                }
                (Some(b), None, Some(i)) => {
                    if b.oid != i.oid {
                        return Err(EngineError::SemanticConflict(format!(
                            "FileModifyDeleteConflict on '{path}': deleted on '{target_ref}' but modified by incoming branch"
                        )));
                    }
                    // Otherwise b.oid == i.oid: clean deletion on trunk
                }
                (Some(_), None, None) | (None, None, None) => {}
                (_, Some(t), Some(i)) => {
                    let base_src = b_entry
                        .and_then(|e| lookup(&e.oid))
                        .map(|o| String::from_utf8_lossy(&o.data).to_string())
                        .unwrap_or_default();
                    let trunk_src = lookup(&t.oid)
                        .map(|o| String::from_utf8_lossy(&o.data).to_string())
                        .unwrap_or_default();
                    let incoming_src = lookup(&i.oid)
                        .map(|o| String::from_utf8_lossy(&o.data).to_string())
                        .unwrap_or_default();

                    match cst_merge_3way(
                        path,
                        target_ref,
                        "incoming-land",
                        &base_src,
                        &trunk_src,
                        &incoming_src,
                    ) {
                        CstMergeOutcome::Clean {
                            merged_source,
                            merged_symbols,
                        } => {
                            spliced_symbols.extend(merged_symbols);
                            let merged_blob =
                                GitObject::new(GitObjectType::Blob, Bytes::from(merged_source));
                            let merged_mode = if i.mode != "100644" {
                                i.mode.clone()
                            } else {
                                t.mode.clone()
                            };
                            merged_file_entries.insert(
                                path.clone(),
                                FileTreeEntry {
                                    mode: merged_mode,
                                    oid: merged_blob.oid,
                                },
                            );
                            self.objects.insert(
                                (repo_id.to_string(), merged_blob.oid),
                                merged_blob,
                            );
                        }
                        CstMergeOutcome::Conflict { conflicts, .. } => {
                            let msg = conflicts
                                .first()
                                .map(|c| c.description.clone())
                                .unwrap_or_else(|| format!("CST conflict in {path}"));
                            return Err(EngineError::SemanticConflict(msg));
                        }
                    }
                }
            }
        }

        let (root_tree_oid, created_trees) = build_trees_from_file_entries(&merged_file_entries);
        for t in &created_trees {
            self.objects
                .insert((repo_id.to_string(), t.oid), t.clone());
        }

        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let merged_commit = build_commit_object(
            root_tree_oid,
            &[current_target_oid],
            "Mesh Semantic Merger <mesh@openai-compatible.local>",
            now_sec,
            &format!(
                "{}\n\nMesh-Source-Commit: {}\nMesh-Commutative-Land: spliced {:?}\n",
                parsed_pushed.message.trim(),
                pushed_commit_oid.to_hex(),
                spliced_symbols
            ),
        );
        self.objects.insert(
            (repo_id.to_string(), merged_commit.oid),
            merged_commit.clone(),
        );

        let mut pack_objs = Vec::new();
        for fe in merged_file_entries.values() {
            if let Some(entry) = self.objects.get(&(repo_id.to_string(), fe.oid)) {
                pack_objs.push(entry.value().clone());
            }
        }
        pack_objs.extend(created_trees);
        pack_objs.push(merged_commit.clone());

        let pack_bytes = encode_packfile(&pack_objs);
        let l0_pack_id = self.blobs.put_pack(pack_bytes.clone())?;
        self.express.warm_pack(&l0_pack_id, pack_bytes.clone());

        let final_deltas =
            self.compute_commit_semantic_deltas(repo_id, &current_target_oid, &merged_commit.oid);

        let rv = self.meta.commit_push(PushTransaction {
            repo_id: repo_id.to_string(),
            ref_name: target_ref.to_string(),
            expected_old_oid: current_target_oid,
            new_commit_oid: merged_commit.oid,
            introduced_commits: vec![pushed_commit_oid, merged_commit.oid],
            l0_pack_id,
            pack_byte_size: pack_bytes.len() as u64,
            pack_object_count: pack_objs.len() as u32,
            explicit_haves: vec![current_target_oid],
            base_trunk_version: None,
            intent_receipt,
            semantic_deltas: final_deltas,
        })?;

        let summary = format!(
            "Mesh CST-Commutative Merge landed onto {target_ref}@v{} (spliced symbols: {:?})",
            rv.version, spliced_symbols
        );
        Ok((rv, Some(summary)))
    }

    pub fn compute_commit_semantic_deltas(
        &self,
        repo_id: &str,
        old_commit_oid: &ObjectId,
        new_commit_oid: &ObjectId,
    ) -> Vec<FileSemanticDelta> {
        let lookup = |oid: &ObjectId| self.get_object(repo_id, oid);
        let old_files = if old_commit_oid.is_zero() {
            BTreeMap::new()
        } else {
            extract_commit_file_entries(old_commit_oid, &lookup).unwrap_or_default()
        };
        let new_files = extract_commit_file_entries(new_commit_oid, &lookup).unwrap_or_default();

        let mut deltas = Vec::new();
        for (path, new_entry) in &new_files {
            let old_blob_oid = old_files
                .get(path)
                .map(|e| e.oid)
                .unwrap_or(ObjectId::ZERO);
            if old_blob_oid == new_entry.oid {
                continue;
            }
            let old_src = if old_blob_oid.is_zero() {
                String::new()
            } else {
                lookup(&old_blob_oid)
                    .map(|o| String::from_utf8_lossy(&o.data).to_string())
                    .unwrap_or_default()
            };
            let new_src = lookup(&new_entry.oid)
                .map(|o| String::from_utf8_lossy(&o.data).to_string())
                .unwrap_or_default();

            if let Some(delta) = compute_file_semantic_delta(
                path,
                old_blob_oid,
                new_entry.oid,
                &old_src,
                &new_src,
            ) {
                deltas.push(delta);
            }
        }
        deltas
    }

    /// Creates a sub-millisecond Zero-Checkout Virtual Workspace anchored at `refs/heads/main`.
    pub fn create_virtual_workspace(
        &self,
        repo_id: &str,
        agent_id: &str,
    ) -> Result<VirtualWorkspace, EngineError> {
        let ptr = self
            .meta
            .get_ref_pointer(repo_id, "refs/heads/main")
            .unwrap_or(RefPointerPlaceholder::empty(repo_id));
        let lookup = |oid: &ObjectId| self.get_object(repo_id, oid);
        let file_entries = if ptr.tip_commit_oid.is_zero() {
            BTreeMap::new()
        } else {
            extract_commit_file_entries(&ptr.tip_commit_oid, &lookup)?
        };

        let mut files = BTreeMap::new();
        for (path, entry) in file_entries {
            if let Some(blob) = lookup(&entry.oid) {
                files.insert(path, String::from_utf8_lossy(&blob.data).to_string());
            }
        }

        let ws_id = format!("ws-{}-v{}", agent_id, ptr.current_version);
        let ws = VirtualWorkspace {
            workspace_id: ws_id.clone(),
            repo_id: repo_id.to_string(),
            agent_id: agent_id.to_string(),
            base_commit_oid: ptr.tip_commit_oid,
            base_trunk_version: ptr.current_version,
            files,
        };
        self.workspaces.insert(ws_id, ws.clone());
        Ok(ws)
    }

    /// Applies an Agent Intent Patch directly in memory on the server without requiring a local `.git` checkout.
    pub fn apply_agent_intent_patch(
        &self,
        repo_id: &str,
        ref_name: &str,
        file_updates: BTreeMap<String, String>,
        message: &str,
        intent_receipt: Option<AgentIntentReceipt>,
    ) -> Result<RefVersion, EngineError> {
        let base_ptr = if ref_name.starts_with("refs/mesh/land/") {
            self.meta.get_ref_pointer(repo_id, "refs/heads/main")
        } else {
            self.meta
                .get_ref_pointer(repo_id, ref_name)
                .or_else(|| self.meta.get_ref_pointer(repo_id, "refs/heads/main"))
        };
        let parent_oid = base_ptr
            .as_ref()
            .map(|p| p.tip_commit_oid)
            .unwrap_or(ObjectId::ZERO);
        let expected_old_oid = self
            .meta
            .get_ref_pointer(repo_id, ref_name)
            .map(|p| p.tip_commit_oid)
            .unwrap_or(ObjectId::ZERO);

        let lookup = |oid: &ObjectId| self.get_object(repo_id, oid);
        let mut entries = if parent_oid.is_zero() {
            BTreeMap::new()
        } else {
            extract_commit_file_entries(&parent_oid, &lookup)?
        };

        let mut objects_to_pack = Vec::new();
        for (path, content) in file_updates {
            let blob = GitObject::new(GitObjectType::Blob, Bytes::from(content));
            let mode = entries
                .get(&path)
                .map(|e| e.mode.clone())
                .unwrap_or_else(|| "100644".to_string());
            entries.insert(
                path,
                FileTreeEntry {
                    mode,
                    oid: blob.oid,
                },
            );
            objects_to_pack.push(blob);
        }

        let (root_tree_oid, mut trees) = build_trees_from_file_entries(&entries);
        objects_to_pack.append(&mut trees);

        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let author = intent_receipt
            .as_ref()
            .map(|r| format!("{} <{}@agent.mesh>", r.agent_id, r.agent_id))
            .unwrap_or_else(|| "Mesh Agent <agent@mesh.local>".to_string());
        let parents = if parent_oid.is_zero() {
            vec![]
        } else {
            vec![parent_oid]
        };
        let commit = build_commit_object(root_tree_oid, &parents, &author, now_sec, message);
        let new_commit_oid = commit.oid;
        objects_to_pack.push(commit);

        let pack_bytes = encode_packfile(&objects_to_pack);
        let (rv, _) = self.process_push(
            repo_id,
            ref_name,
            expected_old_oid,
            new_commit_oid,
            pack_bytes,
            intent_receipt,
        )?;
        Ok(rv)
    }

    pub fn compact_branch_level(
        &self,
        repo_id: &str,
        ref_name: &str,
        source_level: u8,
        fanout: usize,
    ) -> Result<Vec<PackCover>, EngineError> {
        let covers = self.meta.list_pack_covers(repo_id, ref_name);
        let mut level_covers: Vec<PackCover> = covers
            .into_iter()
            .filter(|c| c.level == source_level)
            .collect();
        level_covers.sort_by_key(|c| c.start_version);

        let mut created = Vec::new();

        for chunk in level_covers.chunks(fanout.max(2)) {
            if chunk.len() < fanout.max(2) {
                break;
            }
            let mut contiguous = true;
            for w in chunk.windows(2) {
                if w[1].start_version != w[0].end_version + 1 {
                    contiguous = false;
                    break;
                }
            }
            if !contiguous {
                continue;
            }

            let start_v = chunk.first().unwrap().start_version;
            let end_v = chunk.last().unwrap().end_version;

            let mut raw_packs = Vec::with_capacity(chunk.len());
            for c in chunk {
                raw_packs.push(self.express.fetch_pack(&c.pack_id)?);
            }

            let (compacted_bytes, combined_objs) =
                compact_packs(&raw_packs, |oid| self.get_object(repo_id, oid))?;
            let pack_id = self.blobs.put_pack(compacted_bytes.clone())?;
            self.express.warm_pack(&pack_id, compacted_bytes.clone());

            let new_cover = PackCover {
                repo_id: repo_id.to_string(),
                ref_name: ref_name.to_string(),
                level: source_level + 1,
                start_version: start_v,
                end_version: end_v,
                pack_id,
                byte_size: compacted_bytes.len() as u64,
                object_count: combined_objs.len() as u32,
            };
            self.meta.insert_pack_cover(new_cover.clone());
            created.push(new_cover);
        }

        Ok(created)
    }

    pub fn scan_conflict_radar(&self, repo_id: &str) -> Vec<SemanticConflict> {
        let refs = self.meta.list_all_refs(repo_id);
        let mut branch_deltas: Vec<(String, Vec<FileSemanticDelta>)> = Vec::new();

        for ptr in refs {
            if ptr.ref_name == "refs/heads/main" {
                continue;
            }
            if let Some(rv) = self
                .meta
                .get_ref_version(repo_id, &ptr.ref_name, ptr.current_version)
            {
                if !rv.semantic_deltas.is_empty() {
                    branch_deltas.push((ptr.ref_name.clone(), rv.semantic_deltas));
                } else if let Some(base_ver) = rv.base_trunk_version {
                    if let Some(trunk_rv) =
                        self.meta.get_ref_version(repo_id, "refs/heads/main", base_ver)
                    {
                        let deltas = self.compute_commit_semantic_deltas(
                            repo_id,
                            &trunk_rv.new_commit_oid,
                            &ptr.tip_commit_oid,
                        );
                        branch_deltas.push((ptr.ref_name.clone(), deltas));
                    }
                }
            }
        }

        let mut all_conflicts = Vec::new();
        for i in 0..branch_deltas.len() {
            for j in (i + 1)..branch_deltas.len() {
                let (ref_a, deltas_a) = &branch_deltas[i];
                let (ref_b, deltas_b) = &branch_deltas[j];
                let mut found = detect_semantic_conflicts(ref_a, deltas_a, ref_b, deltas_b);
                all_conflicts.append(&mut found);
            }
        }
        all_conflicts
    }

    pub fn batch_commutative_land(
        &self,
        repo_id: &str,
        branch_refs: &[String],
    ) -> Result<RefVersion, EngineError> {
        let mut last_rv = None;
        for br in branch_refs {
            let ptr = self
                .meta
                .get_ref_pointer(repo_id, br)
                .ok_or_else(|| MetaError::RefNotFound(repo_id.to_string(), br.clone()))?;
            let (rv, _) = self.land_commit_onto_target(
                repo_id,
                "refs/heads/main",
                ptr.tip_commit_oid,
                None,
            )?;
            last_rv = Some(rv);
        }
        last_rv.ok_or_else(|| {
            EngineError::Meta(MetaError::RefNotFound(
                repo_id.to_string(),
                "refs/heads/main".to_string(),
            ))
        })
    }
}

struct RefPointerPlaceholder;
impl RefPointerPlaceholder {
    fn empty(repo_id: &str) -> mesh_proto::RefPointer {
        mesh_proto::RefPointer {
            repo_id: repo_id.to_string(),
            ref_name: "refs/heads/main".to_string(),
            current_version: 0,
            tip_commit_oid: ObjectId::ZERO,
            base_trunk_version: None,
        }
    }
}

// ============================================================================
// HTTP Server & Routes (Git Smart HTTP v1/v2 + Express CDN + Agent REST API)
// ============================================================================

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<MeshEngine>,
    pub public_base_url: String,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Git Smart HTTP v1/v2 Endpoints
        .route("/git/:repo/info/refs", get(handle_info_refs))
        .route("/git/:repo/git-receive-pack", post(handle_receive_pack))
        .route("/git/:repo/git-upload-pack", post(handle_upload_pack))
        // Express Consistent-Hashed Pack CDN
        .route("/express/packs/:pack_id", get(handle_express_pack_download))
        // Agent-Native Semantic & Zero-Checkout Workspace API
        .route("/api/v1/repos/:repo/workspaces", post(handle_create_workspace))
        .route(
            "/api/v1/repos/:repo/workspaces/:ws_id",
            get(handle_get_workspace),
        )
        .route("/api/v1/repos/:repo/patches", post(handle_apply_patch))
        .route("/api/v1/repos/:repo/radar", get(handle_conflict_radar))
        .route("/api/v1/repos/:repo/land", post(handle_batch_land))
        .route("/api/v1/repos/:repo/compact", post(handle_compact))
        .route("/api/v1/repos/:repo/stats", get(handle_repo_stats))
        .with_state(state)
}

#[derive(Deserialize)]
pub struct InfoRefsQuery {
    pub service: Option<String>,
    pub target_ref: Option<String>,
}

async fn handle_info_refs(
    State(state): State<AppState>,
    Path(repo): Path<String>,
    Query(query): Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> Response {
    let service = query.service.as_deref().unwrap_or("git-upload-pack");
    let git_proto = parse_git_protocol_header(
        headers
            .get("git-protocol")
            .and_then(|v| v.to_str().ok()),
    );

    let content_type = format!("application/x-{service}-advertisement");

    // Bug 6 Fix: If client sends `Git-Protocol: version=2` for `git-upload-pack`,
    // respond with Protocol v2 capability advertisement (`ls-refs`, `fetch=packfile-uris`)!
    if service == "git-upload-pack" && git_proto.version == 2 {
        let body = build_protocol_v2_capability_advertisement(service);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(body))
            .unwrap();
    }

    // Bug 13 Fix: Resolve `target_ref` from query param OR `Git-Protocol: target-ref=...`
    let target_ref = query
        .target_ref
        .as_deref()
        .or(git_proto.target_ref.as_deref());

    let advertised = if service == "git-receive-pack" {
        state
            .engine
            .meta
            .list_advertised_refs(&repo, "refs/heads/main", target_ref)
    } else {
        state.engine.meta.list_all_refs(&repo)
    };

    let body = build_info_refs_advertisement(service, &advertised);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(body))
        .unwrap()
}

async fn handle_receive_pack(
    State(state): State<AppState>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_receive_pack_request(&body) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("pktline error: {e}")).into_response();
        }
    };

    let use_sideband = parsed
        .capabilities
        .iter()
        .any(|c| c == "side-band-64k" || c == "side-band");

    let agent_id = headers
        .get("x-mesh-agent-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let prompt_summary = headers
        .get("x-mesh-intent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let receipt = agent_id.map(|id| AgentIntentReceipt {
        agent_id: id,
        model_id: "gpt-6-astra".to_string(),
        prompt_summary: prompt_summary.unwrap_or_else(|| "git push".to_string()),
        verified_tests: vec![],
        timestamp_ms: 0,
    });

    let mut results = Vec::new();
    let mut progress_msgs = Vec::new();

    for cmd in parsed.commands {
        match state.engine.process_push(
            &repo,
            &cmd.ref_name,
            cmd.old_oid,
            cmd.new_oid,
            parsed.pack_bytes.clone(),
            receipt.clone(),
        ) {
            Ok((rv, summary)) => {
                if let Some(msg) = summary {
                    progress_msgs.push(format!("[mesh] {msg}"));
                } else {
                    progress_msgs.push(format!(
                        "[mesh] Causal commit {}@{} (pack: {}, deltas: {} files)",
                        rv.ref_name,
                        rv.version,
                        rv.l0_pack_id,
                        rv.semantic_deltas.len()
                    ));
                }
                results.push(RefUpdateResult {
                    ref_name: cmd.ref_name,
                    ok: true,
                    message: None,
                });
            }
            Err(e) => {
                progress_msgs.push(format!("[mesh] Push rejected: {e}"));
                results.push(RefUpdateResult {
                    ref_name: cmd.ref_name,
                    ok: false,
                    message: Some(e.to_string()),
                });
            }
        }
    }

    let report = encode_receive_pack_report(&results, &progress_msgs, use_sideband);
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "application/x-git-receive-pack-result",
        )
        .body(Body::from(report))
        .unwrap()
}

async fn handle_upload_pack(
    State(state): State<AppState>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let req = match parse_upload_pack_request(&body) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("upload-pack error: {e}")).into_response();
        }
    };

    // Bug 6 Fix: Handle Protocol v2 `command=ls-refs`
    if req.is_protocol_v2_ls_refs {
        let all_refs = state.engine.meta.list_all_refs(&repo);
        let resp = encode_protocol_v2_ls_refs_response(&all_refs, &req.ref_prefixes);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
            .body(Body::from(resp))
            .unwrap();
    }

    if req.wants.is_empty() {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
            .body(Body::from(Bytes::from_static(b"0000")))
            .unwrap();
    }

    // Bug 7 Fix: Multi-round Smart HTTP v1 negotiation when `done == false` and `no-done` is not set
    if !req.is_protocol_v2_fetch && !req.done && !req.no_done {
        let common: Vec<ObjectId> = req
            .haves
            .iter()
            .copied()
            .filter(|oid| state.engine.meta.resolve_commit(&repo, oid).is_some())
            .collect();
        let ack_nak = encode_v1_negotiation_only(&common);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
            .body(Body::from(ack_nak))
            .unwrap();
    }

    let plan = match state.engine.meta.plan_fetch_covers(
        &repo,
        "refs/heads/main",
        &req.haves,
        &req.wants,
    ) {
        Ok(p) => p,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("fetch plan error: {e}")).into_response();
        }
    };

    // Pando `packfile-uri` mode: return Express CDN URLs + 32-byte empty inline packfile.
    // Resolve base URL from incoming `Host` / `X-Forwarded-Proto` header so remote devices get
    // reachable URLs rather than 127.0.0.1.
    if req.supports_packfile_uris {
        let scheme = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("http");
        let base_url = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(|host| format!("{scheme}://{host}"))
            .unwrap_or_else(|| state.public_base_url.clone());

        let uris: Vec<String> = plan
            .selected_covers
            .iter()
            .map(|c| format!("{base_url}/express/packs/{}", c.pack_id))
            .collect();
        let response_bytes = encode_packfile_uri_response(&uris);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
            .body(Body::from(response_bytes))
            .unwrap();
    }

    let mut raw_packs = Vec::with_capacity(plan.selected_covers.len());
    for cover in &plan.selected_covers {
        if let Ok(p) = state.engine.express.fetch_pack(&cover.pack_id) {
            raw_packs.push(p);
        }
    }
    let (consolidated_pack, _) =
        compact_packs(&raw_packs, |oid| state.engine.get_object(&repo, oid))
            .unwrap_or_else(|_| (mesh_pack::empty_packfile(), vec![]));

    let info_msg = format!(
        "[mesh] Served {} cover pack(s) via Express cache ({} metadata reads)",
        plan.selected_covers.len(),
        plan.metadata_reads
    );

    let response_bytes = if req.is_protocol_v2_fetch {
        encode_protocol_v2_fetch_response(
            &consolidated_pack,
            req.haves.first().copied(),
            req.done,
            Some(&info_msg),
        )
    } else {
        encode_standard_upload_pack_response(
            &consolidated_pack,
            req.haves.first().copied(),
            Some(&info_msg),
            req.supports_sideband,
        )
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
        .body(Body::from(response_bytes))
        .unwrap()
}

async fn handle_express_pack_download(
    State(state): State<AppState>,
    Path(pack_id): Path<String>,
) -> Response {
    let pid = PackId(pack_id.trim_end_matches(".pack").to_string());
    match state.engine.express.fetch_pack(&pid) {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/x-git-packed-objects")
            .header(
                header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            )
            .body(Body::from(bytes))
            .unwrap(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct CreateWorkspaceRequest {
    pub agent_id: String,
}

async fn handle_create_workspace(
    State(state): State<AppState>,
    Path(repo): Path<String>,
    Json(req): Json<CreateWorkspaceRequest>,
) -> Result<Json<VirtualWorkspace>, (StatusCode, String)> {
    state
        .engine
        .create_virtual_workspace(&repo, &req.agent_id)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn handle_get_workspace(
    State(state): State<AppState>,
    Path((_repo, ws_id)): Path<(String, String)>,
) -> Result<Json<VirtualWorkspace>, (StatusCode, String)> {
    state
        .engine
        .workspaces
        .get(&ws_id)
        .map(|r| Json(r.value().clone()))
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("workspace '{ws_id}' not found")))
}

#[derive(Deserialize)]
pub struct ApplyPatchRequest {
    pub ref_name: String,
    pub file_updates: BTreeMap<String, String>,
    pub message: String,
    pub intent_receipt: Option<AgentIntentReceipt>,
}

async fn handle_apply_patch(
    State(state): State<AppState>,
    Path(repo): Path<String>,
    Json(req): Json<ApplyPatchRequest>,
) -> Result<Json<RefVersion>, (StatusCode, String)> {
    state
        .engine
        .apply_agent_intent_patch(
            &repo,
            &req.ref_name,
            req.file_updates,
            &req.message,
            req.intent_receipt,
        )
        .map(Json)
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))
}

async fn handle_conflict_radar(
    State(state): State<AppState>,
    Path(repo): Path<String>,
) -> Json<Vec<SemanticConflict>> {
    Json(state.engine.scan_conflict_radar(&repo))
}

#[derive(Deserialize)]
pub struct BatchLandRequest {
    pub branches: Vec<String>,
}

async fn handle_batch_land(
    State(state): State<AppState>,
    Path(repo): Path<String>,
    Json(req): Json<BatchLandRequest>,
) -> Result<Json<RefVersion>, (StatusCode, String)> {
    state
        .engine
        .batch_commutative_land(&repo, &req.branches)
        .map(Json)
        .map_err(|e| (StatusCode::CONFLICT, e.to_string()))
}

#[derive(Deserialize)]
pub struct CompactRequest {
    pub ref_name: String,
    pub source_level: u8,
    pub fanout: usize,
}

async fn handle_compact(
    State(state): State<AppState>,
    Path(repo): Path<String>,
    Json(req): Json<CompactRequest>,
) -> Result<Json<Vec<PackCover>>, (StatusCode, String)> {
    state
        .engine
        .compact_branch_level(&repo, &req.ref_name, req.source_level, req.fanout)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[derive(Serialize, Deserialize)]
pub struct RepoStatsResponse {
    pub repo_id: String,
    pub total_refs: usize,
    pub advertised_refs_on_push: usize,
    pub push_advertisement_bytes: usize,
    pub trunk_covers: Vec<PackCover>,
    pub express_cache: ExpressCacheStats,
    pub active_semantic_conflicts: usize,
}

async fn handle_repo_stats(
    State(state): State<AppState>,
    Path(repo): Path<String>,
) -> Json<RepoStatsResponse> {
    let all_refs = state.engine.meta.list_all_refs(&repo);
    let adv_refs = state
        .engine
        .meta
        .list_advertised_refs(&repo, "refs/heads/main", None);
    let adv_payload = build_info_refs_advertisement("git-receive-pack", &adv_refs);
    let trunk_covers = state.engine.meta.list_pack_covers(&repo, "refs/heads/main");
    let conflicts = state.engine.scan_conflict_radar(&repo);

    Json(RepoStatsResponse {
        repo_id: repo,
        total_refs: all_refs.len(),
        advertised_refs_on_push: adv_refs.len(),
        push_advertisement_bytes: adv_payload.len(),
        trunk_covers,
        express_cache: state.engine.express.stats(),
        active_semantic_conflicts: conflicts.len(),
    })
}
