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
use mesh_blob::{ExpressCacheFleet, ExpressCacheStats, MemoryBlobStore, PackBlobStore};
use mesh_meta::{CausalMetadataStore, MeshMetadataStore, MetaError, PushTransaction};
use mesh_pack::{
    build_commit_object, build_trees_from_file_map, compact_packs, decode_and_validate_pack,
    encode_packfile, extract_commit_files, parse_commit, GitObject, GitObjectType,
};
use mesh_pktline::{
    build_info_refs_advertisement, encode_packfile_uri_response, encode_receive_pack_report,
    encode_standard_upload_pack_response, parse_receive_pack_request, parse_upload_pack_request,
    RefUpdateResult,
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

/// Core Pando + Semantic Agent VCS Engine.
pub struct MeshEngine {
    pub meta: Arc<CausalMetadataStore>,
    pub blobs: Arc<dyn PackBlobStore>,
    pub express: Arc<ExpressCacheFleet>,
    /// Fast in-memory content-addressed object index `(repo_id, ObjectId) -> GitObject`
    /// populated during single-pass streaming pack validation.
    pub objects: DashMap<(String, ObjectId), GitObject>,
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
            256 * 1024 * 1024, // 256 MiB per Express cache node
        ));
        Self {
            meta: Arc::new(CausalMetadataStore::new()),
            blobs,
            express,
            objects: DashMap::new(),
        }
    }

    pub fn get_object(&self, repo_id: &str, oid: &ObjectId) -> Option<GitObject> {
        self.objects
            .get(&(repo_id.to_string(), *oid))
            .map(|r| r.value().clone())
    }

    /// Single-pass validation, pack storage, semantic lifting, and causal per-ref CAS commit.
    /// Also handles `refs/mesh/land/<branch>` for server-side CST-commutative merging!
    pub fn process_push(
        &self,
        repo_id: &str,
        ref_name: &str,
        expected_old_oid: ObjectId,
        new_commit_oid: ObjectId,
        pack_bytes: Bytes,
        intent_receipt: Option<AgentIntentReceipt>,
    ) -> Result<(RefVersion, Option<String>), EngineError> {
        let decoded =
            decode_and_validate_pack(&pack_bytes, |oid| self.get_object(repo_id, oid))?;

        // Index objects in memory ($O(1)$ per pushed object)
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

        // Store immutable L0 packfile in Blob Store & warm Express cache
        let l0_pack_id = self.blobs.put_pack(pack_bytes.clone())?;
        self.express.warm_pack(&l0_pack_id, pack_bytes.clone());

        // Determine base commit for semantic lifting (either expected_old_oid or commit's first parent)
        let diff_base_oid = if !expected_old_oid.is_zero() {
            expected_old_oid
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
    /// If `target_ref` has advanced past the pushed commit's parent, automatically splices
    /// orthogonal CST declarations and checks cross-file signature/call safety!
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

        // Fast-forward case: target_ref is still at base_oid
        if current_target_oid == base_oid {
            let mut commit_objs = Vec::new();
            let files = extract_commit_files(&pushed_commit_oid, &lookup)?;
            for blob_oid in files.values() {
                if let Some(b) = lookup(blob_oid) {
                    commit_objs.push(b);
                }
            }
            let (_, mut trees) = build_trees_from_file_map(&files);
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

        // Non-fast-forward case: `target_ref` moved from `base_oid` to `current_target_oid`.
        // Run cross-file Semantic Conflict Radar + CST-Commutative 3-Way Merge!
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

        let base_files = extract_commit_files(&base_oid, &lookup)?;
        let trunk_files = extract_commit_files(&current_target_oid, &lookup)?;
        let incoming_files = extract_commit_files(&pushed_commit_oid, &lookup)?;

        let mut all_paths: BTreeMap<String, ()> = BTreeMap::new();
        for k in base_files
            .keys()
            .chain(trunk_files.keys())
            .chain(incoming_files.keys())
        {
            all_paths.insert(k.clone(), ());
        }

        let mut merged_file_map = BTreeMap::new();
        let mut new_blobs = Vec::new();
        let mut spliced_symbols = Vec::new();

        for path in all_paths.keys() {
            let b_oid = base_files.get(path).copied();
            let t_oid = trunk_files.get(path).copied();
            let i_oid = incoming_files.get(path).copied();

            match (b_oid, t_oid, i_oid) {
                (_, Some(t), Some(i)) if t == i => {
                    merged_file_map.insert(path.clone(), t);
                }
                (Some(b), Some(t), Some(i)) if b == t => {
                    merged_file_map.insert(path.clone(), i);
                }
                (Some(b), Some(t), Some(i)) if b == i => {
                    merged_file_map.insert(path.clone(), t);
                }
                (None, Some(t), None) => {
                    merged_file_map.insert(path.clone(), t);
                }
                (None, None, Some(i)) => {
                    merged_file_map.insert(path.clone(), i);
                }
                (_, Some(t), Some(i)) => {
                    // Both trunk and incoming modified or created `path`!
                    let base_src = b_oid
                        .and_then(|id| lookup(&id))
                        .map(|o| String::from_utf8_lossy(&o.data).to_string())
                        .unwrap_or_default();
                    let trunk_src = lookup(&t)
                        .map(|o| String::from_utf8_lossy(&o.data).to_string())
                        .unwrap_or_default();
                    let incoming_src = lookup(&i)
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
                            merged_file_map.insert(path.clone(), merged_blob.oid);
                            self.objects.insert(
                                (repo_id.to_string(), merged_blob.oid),
                                merged_blob.clone(),
                            );
                            new_blobs.push(merged_blob);
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
                _ => {}
            }
        }

        let (root_tree_oid, created_trees) = build_trees_from_file_map(&merged_file_map);
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
        for oid in merged_file_map.values() {
            if let Some(entry) = self.objects.get(&(repo_id.to_string(), *oid)) {
                pack_objs.push(entry.value().clone());
            }
        }
        pack_objs.extend(created_trees);
        pack_objs.push(merged_commit.clone());

        let pack_bytes = encode_packfile(&pack_objs);
        let l0_pack_id = self.blobs.put_pack(pack_bytes.clone())?;
        self.express.warm_pack(&l0_pack_id, pack_bytes.clone());

        let final_deltas = self.compute_commit_semantic_deltas(
            repo_id,
            &current_target_oid,
            &merged_commit.oid,
        );

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
            extract_commit_files(old_commit_oid, &lookup).unwrap_or_default()
        };
        let new_files = extract_commit_files(new_commit_oid, &lookup).unwrap_or_default();

        let mut deltas = Vec::new();
        for (path, new_blob_oid) in &new_files {
            let old_blob_oid = old_files.get(path).copied().unwrap_or(ObjectId::ZERO);
            if old_blob_oid == *new_blob_oid {
                continue;
            }
            let old_src = if old_blob_oid.is_zero() {
                String::new()
            } else {
                lookup(&old_blob_oid)
                    .map(|o| String::from_utf8_lossy(&o.data).to_string())
                    .unwrap_or_default()
            };
            let new_src = lookup(new_blob_oid)
                .map(|o| String::from_utf8_lossy(&o.data).to_string())
                .unwrap_or_default();

            if let Some(delta) =
                compute_file_semantic_delta(path, old_blob_oid, *new_blob_oid, &old_src, &new_src)
            {
                deltas.push(delta);
            }
        }
        deltas
    }

    /// Background LSM Compaction Worker:
    /// Combines contiguous sequences of `fanout` covers at `source_level` on `ref_name`
    /// into consolidated `source_level + 1` covers without deleting lower-level packs!
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
            // Verify chunk covers a contiguous range of ref versions
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

    /// Scans all active topic branches in `repo_id` and returns any pre-merge semantic conflicts.
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

    /// Batch Commutative Trunk Landing (`mesh land`):
    /// Merges multiple independent agent branches onto `refs/heads/main` in a single cycle
    /// without sequential client-side rebases.
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
        // Agent-Native Semantic & Admin API
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
) -> Response {
    let service = query.service.as_deref().unwrap_or("git-upload-pack");
    // Pando Trunk-Only Ref Advertisement on push; all advertised refs or trunk+target on fetch
    let advertised = if service == "git-receive-pack" {
        state.engine.meta.list_advertised_refs(
            &repo,
            "refs/heads/main",
            query.target_ref.as_deref(),
        )
    } else {
        state.engine.meta.list_all_refs(&repo)
    };

    let body = build_info_refs_advertisement(service, &advertised);
    let content_type = format!("application/x-{service}-advertisement");

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
    body: Bytes,
) -> Response {
    let req = match parse_upload_pack_request(&body) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("upload-pack error: {e}")).into_response();
        }
    };

    if req.wants.is_empty() {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
            .body(Body::from(Bytes::from_static(b"0000")))
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

    // Pando `packfile-uri` mode: return Express CDN URLs + 32-byte empty inline packfile
    if req.supports_packfile_uris {
        let uris: Vec<String> = plan
            .selected_covers
            .iter()
            .map(|c| format!("{}/express/packs/{}", state.public_base_url, c.pack_id))
            .collect();
        let response_bytes = encode_packfile_uri_response(&uris);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
            .body(Body::from(response_bytes))
            .unwrap();
    }

    // Standard Smart HTTP v1 fallback for stock `git` clients:
    // Fetch each planned cover from Express (recording cache hits!) and consolidate.
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
    let response_bytes = encode_standard_upload_pack_response(
        &consolidated_pack,
        req.haves.first().copied(),
        Some(&info_msg),
        req.supports_sideband,
    );

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
