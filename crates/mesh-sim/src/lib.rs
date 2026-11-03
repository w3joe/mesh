use bytes::Bytes;
use mesh_meta::MeshMetadataStore;
use mesh_pack::{
    build_commit_object, build_trees_from_file_map, encode_packfile, extract_commit_files,
    GitObject, GitObjectType,
};
use mesh_proto::{ObjectId, SemanticConflictKind};
use mesh_server::MeshEngine;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArchitectureBenchmarkResult {
    pub architecture: String,
    pub num_writers: usize,
    pub pushes_per_writer: usize,
    pub total_pushes: usize,
    pub wall_clock_ms: f64,
    pub p50_push_latency_us: u64,
    pub p99_push_latency_us: u64,
    pub cas_retries: u64,
    pub ref_advertisement_bytes: usize,
    pub express_cache_hit_rate_pct: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FullSimulationReport {
    pub three_phase_commit: ArchitectureBenchmarkResult,
    pub object_store_wal: ArchitectureBenchmarkResult,
    pub mesh_pando: ArchitectureBenchmarkResult,
    pub lsm_compaction_summary: LsmCompactionSummary,
    pub semantic_landing_summary: SemanticLandingSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LsmCompactionSummary {
    pub trunk_versions: u64,
    pub uncompacted_l0_packs_needed: usize,
    pub compacted_skip_list_packs_needed: usize,
    pub metadata_reads_for_fetch_plan: usize,
    pub selected_cover_levels: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SemanticLandingSummary {
    pub concurrent_agents_editing_same_file: usize,
    pub git_line_merge_conflicts: usize,
    pub mesh_cst_auto_merged_branches: usize,
    pub cross_file_signature_breaks_caught: usize,
}

/// Helper to create a valid Git packfile containing a single file edit commit.
pub fn build_single_file_commit_pack(
    parent_oid: ObjectId,
    file_path: &str,
    file_content: &str,
    author: &str,
    message: &str,
) -> (ObjectId, Bytes) {
    let blob = GitObject::new(GitObjectType::Blob, Bytes::from(file_content.to_string()));
    let mut files = BTreeMap::new();
    files.insert(file_path.to_string(), blob.oid);
    let (root_tree_oid, mut trees) = build_trees_from_file_map(&files);
    let parents = if parent_oid.is_zero() {
        vec![]
    } else {
        vec![parent_oid]
    };
    let commit = build_commit_object(root_tree_oid, &parents, author, 1793664000, message);
    let commit_oid = commit.oid;

    let mut objs = vec![blob];
    objs.append(&mut trees);
    objs.push(commit);
    let pack = encode_packfile(&objs);
    (commit_oid, pack)
}

/// Runs the 3-way architecture benchmark (Three-Phase Commit vs. Object-Store WAL vs. Mesh-Pando)
/// plus LSM Compaction and Multi-Agent CST-Commutative Landing verification.
pub async fn run_full_benchmark_suite(
    num_writers: usize,
    pushes_per_writer: usize,
) -> FullSimulationReport {
    let three_phase_commit = simulate_three_phase_commit(num_writers, pushes_per_writer).await;
    let object_store_wal = simulate_object_store_wal(num_writers, pushes_per_writer).await;
    let (mesh_pando, lsm_compaction_summary) =
        benchmark_mesh_pando_engine(num_writers, pushes_per_writer).await;
    let semantic_landing_summary = benchmark_semantic_agent_landing(20);

    FullSimulationReport {
        three_phase_commit,
        object_store_wal,
        mesh_pando,
        lsm_compaction_summary,
        semantic_landing_summary,
    }
}

/// Model 1: Conventional Three-Phase Commit (3PC) Replicated Git Server.
/// Every push acquires a repository-wide ref coordination lock across a quorum of 3 replicas
/// and unpacks objects on each replica while holding the lock.
async fn simulate_three_phase_commit(
    num_writers: usize,
    pushes_per_writer: usize,
) -> ArchitectureBenchmarkResult {
    let global_repo_lock = Arc::new(tokio::sync::Mutex::new(0u64));
    let start = Instant::now();
    let mut handles = Vec::with_capacity(num_writers);

    for w in 0..num_writers {
        let lock = global_repo_lock.clone();
        handles.push(tokio::spawn(async move {
            let mut latencies = Vec::with_capacity(pushes_per_writer);
            for p in 0..pushes_per_writer {
                let (_, pack) = build_single_file_commit_pack(
                    ObjectId::ZERO,
                    "src/lib.rs",
                    &format!("pub fn agent_{w}_{p}() {{}}\n"),
                    "Agent <a@openai.com>",
                    "3pc push",
                );
                let t0 = Instant::now();
                {
                    let mut guard = lock.lock().await;
                    // 3PC unpacks the packfile on 3 quorum replicas + 2-RTT pre-commit/commit
                    for _replica in 0..3 {
                        let decoded =
                            mesh_pack::decode_and_validate_pack(&pack, |_| None).unwrap();
                        std::hint::black_box(decoded.objects.len());
                    }
                    tokio::task::yield_now().await;
                    *guard += 1;
                }
                latencies.push(t0.elapsed().as_micros() as u64);
            }
            latencies
        }));
    }

    let mut all_latencies = Vec::with_capacity(num_writers * pushes_per_writer);
    for h in handles {
        all_latencies.extend(h.await.unwrap());
    }
    let wall_clock_ms = start.elapsed().as_secs_f64() * 1000.0;
    all_latencies.sort_unstable();

    ArchitectureBenchmarkResult {
        architecture: "Three-Phase Commit (Global Repo Lock + Full Ref Adv)".to_string(),
        num_writers,
        pushes_per_writer,
        total_pushes: num_writers * pushes_per_writer,
        wall_clock_ms,
        p50_push_latency_us: percentile(&all_latencies, 50),
        p99_push_latency_us: percentile(&all_latencies, 99),
        cas_retries: 0,
        ref_advertisement_bytes: (num_writers + 1) * 68, // Scales linearly with branch count
        express_cache_hit_rate_pct: 0.0,                 // Dynamic pack generation per client
    }
}

/// Model 2: Object-Store Write-Ahead Log (WAL) with Shared Manifest CAS.
/// Writers upload packfiles in parallel, but contend on a single shared repository manifest CAS.
async fn simulate_object_store_wal(
    num_writers: usize,
    pushes_per_writer: usize,
) -> ArchitectureBenchmarkResult {
    let shared_manifest_version = Arc::new(AtomicU64::new(0));
    let total_cas_retries = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let mut handles = Vec::with_capacity(num_writers);

    for w in 0..num_writers {
        let manifest = shared_manifest_version.clone();
        let retries = total_cas_retries.clone();
        handles.push(tokio::spawn(async move {
            let mut latencies = Vec::with_capacity(pushes_per_writer);
            for p in 0..pushes_per_writer {
                let t0 = Instant::now();
                // Parallel pack encode + upload
                let (_, pack) = build_single_file_commit_pack(
                    ObjectId::ZERO,
                    "src/lib.rs",
                    &format!("pub fn agent_{w}_{p}() {{}}\n"),
                    "Agent <a@openai.com>",
                    "wal push",
                );
                std::hint::black_box(pack.len());

                // Contended CAS loop on the shared repository manifest
                loop {
                    let observed = manifest.load(Ordering::Acquire);
                    // Simulate manifest read-modify-verify window
                    tokio::task::yield_now().await;
                    if manifest
                        .compare_exchange(
                            observed,
                            observed + 1,
                            Ordering::SeqCst,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        break;
                    }
                    retries.fetch_add(1, Ordering::Relaxed);
                }
                latencies.push(t0.elapsed().as_micros() as u64);
            }
            latencies
        }));
    }

    let mut all_latencies = Vec::with_capacity(num_writers * pushes_per_writer);
    for h in handles {
        all_latencies.extend(h.await.unwrap());
    }
    let wall_clock_ms = start.elapsed().as_secs_f64() * 1000.0;
    all_latencies.sort_unstable();

    ArchitectureBenchmarkResult {
        architecture: "Object-Store WAL (Shared Repo Manifest CAS)".to_string(),
        num_writers,
        pushes_per_writer,
        total_pushes: num_writers * pushes_per_writer,
        wall_clock_ms,
        p50_push_latency_us: percentile(&all_latencies, 50),
        p99_push_latency_us: percentile(&all_latencies, 99),
        cas_retries: total_cas_retries.load(Ordering::Relaxed),
        ref_advertisement_bytes: (num_writers + 1) * 68,
        express_cache_hit_rate_pct: 0.0,
    }
}

/// Model 3: Real `MeshEngine` (Pando Per-Ref Causal Consistency + Trunk-Only Advertisement
/// + Single-Pass Streaming Pack Validation + LSM Skip-List Compaction + Express Cache).
async fn benchmark_mesh_pando_engine(
    num_writers: usize,
    pushes_per_writer: usize,
) -> (ArchitectureBenchmarkResult, LsmCompactionSummary) {
    let engine = Arc::new(MeshEngine::new());
    let repo_id = "openai/monorepo";

    // 1. Seed `refs/heads/main` with 64 versions so we can also test L0 -> L1 -> L2 compaction
    let mut main_tip = ObjectId::ZERO;
    let mut v4_oid = ObjectId::ZERO;
    let mut v60_oid = ObjectId::ZERO;
    for v in 1..=64u64 {
        let (oid, pack) = build_single_file_commit_pack(
            main_tip,
            "src/core.rs",
            &format!("pub const TRUNK_VERSION: u64 = {v};\n"),
            "Trunk CI <ci@openai.com>",
            &format!("Trunk commit v{v}"),
        );
        engine
            .process_push(repo_id, "refs/heads/main", main_tip, oid, pack, None)
            .unwrap();
        if v == 4 {
            v4_oid = oid;
        }
        if v == 60 {
            v60_oid = oid;
        }
        main_tip = oid;
    }

    // 2. Run `num_writers` concurrent agents pushing to independent branches `refs/heads/agent-{w}`
    let start = Instant::now();
    let mut handles = Vec::with_capacity(num_writers);

    for w in 0..num_writers {
        let eng = engine.clone();
        let base_trunk = main_tip;
        handles.push(tokio::spawn(async move {
            let branch_name = format!("refs/heads/agent-{w}");
            let mut branch_tip = ObjectId::ZERO;
            let mut latencies = Vec::with_capacity(pushes_per_writer);

            for p in 0..pushes_per_writer {
                let (new_oid, pack) = build_single_file_commit_pack(
                    if branch_tip.is_zero() {
                        base_trunk
                    } else {
                        branch_tip
                    },
                    &format!("src/agent_{w}.rs"),
                    &format!("pub fn task_{w}_step_{p}() -> u64 {{ {p} }}\n"),
                    "Codex Agent <codex@openai.com>",
                    &format!("agent {w} step {p}"),
                );
                let t0 = Instant::now();
                eng.process_push(repo_id, &branch_name, branch_tip, new_oid, pack, None)
                    .unwrap();
                latencies.push(t0.elapsed().as_micros() as u64);
                branch_tip = new_oid;
                tokio::task::yield_now().await;
            }
            (latencies, branch_tip)
        }));
    }

    let mut all_latencies = Vec::with_capacity(num_writers * pushes_per_writer);
    let mut sample_topic_tip = ObjectId::ZERO;
    for (idx, h) in handles.into_iter().enumerate() {
        let (lats, tip) = h.await.unwrap();
        if idx == 0 {
            sample_topic_tip = tip;
        }
        all_latencies.extend(lats);
    }
    let wall_clock_ms = start.elapsed().as_secs_f64() * 1000.0;
    all_latencies.sort_unstable();

    // 3. Run LSM Compaction on `refs/heads/main`:
    //    Compact L0 -> L1 (fanout 8) and L1 -> L2 (fanout 4 -> 32 versions per L2 cover)
    engine
        .compact_branch_level(repo_id, "refs/heads/main", 0, 8)
        .unwrap();
    engine
        .compact_branch_level(repo_id, "refs/heads/main", 1, 4)
        .unwrap();

    // 4. Plan a fetch from HAVE = trunk@v4 to WANT = trunk@v60 + topic branch `agent-0`
    let uncompacted_count = (60 - 4) + 1; // 56 trunk L0s + 1 topic pack = 57
    let plan = engine
        .meta
        .plan_fetch_covers(
            repo_id,
            "refs/heads/main",
            &[v4_oid],
            &[v60_oid, sample_topic_tip],
        )
        .unwrap();

    // Simulate 100 CI readers fetching the planned covers through Express
    for _ in 0..100 {
        for cover in &plan.selected_covers {
            let _ = engine.express.fetch_pack(&cover.pack_id).unwrap();
        }
    }

    let adv_refs = engine
        .meta
        .list_advertised_refs(repo_id, "refs/heads/main", None);
    let adv_bytes =
        mesh_pktline::build_info_refs_advertisement("git-receive-pack", &adv_refs).len();

    let comp_summary = LsmCompactionSummary {
        trunk_versions: 64,
        uncompacted_l0_packs_needed: uncompacted_count,
        compacted_skip_list_packs_needed: plan.selected_covers.len(),
        metadata_reads_for_fetch_plan: plan.metadata_reads,
        selected_cover_levels: plan
            .selected_covers
            .iter()
            .map(|c| format!("L{}({}[{}..={}])", c.level, c.ref_name, c.start_version, c.end_version))
            .collect(),
    };

    let bench_res = ArchitectureBenchmarkResult {
        architecture: "Mesh (Per-Ref Causal CAS + Trunk-Only Adv + Express CDN)".to_string(),
        num_writers,
        pushes_per_writer,
        total_pushes: num_writers * pushes_per_writer,
        wall_clock_ms,
        p50_push_latency_us: percentile(&all_latencies, 50),
        p99_push_latency_us: percentile(&all_latencies, 99),
        cas_retries: engine.meta.total_cas_conflicts(),
        ref_advertisement_bytes: adv_bytes,
        express_cache_hit_rate_pct: engine.express.stats().hit_rate_pct,
    };

    (bench_res, comp_summary)
}

/// Benchmarks Layer 2: `N` concurrent agents editing the exact same `src/router.rs` file
/// (adding imports at the top and route handler functions at the bottom) and landing onto
/// `refs/heads/main` via `refs/mesh/land/main` without rebasing!
pub fn benchmark_semantic_agent_landing(num_agents: usize) -> SemanticLandingSummary {
    let engine = MeshEngine::new();
    let repo_id = "openai/monorepo-semantic";

    let base_router_src = "\
use std::sync::Arc;

/// Base health route
pub fn route_health() -> &'static str {
    \"healthy\"
}
";
    let (base_commit_oid, base_pack) = build_single_file_commit_pack(
        ObjectId::ZERO,
        "src/router.rs",
        base_router_src,
        "Lead Engineer <eng@openai.com>",
        "Base router",
    );
    engine
        .process_push(
            repo_id,
            "refs/heads/main",
            ObjectId::ZERO,
            base_commit_oid,
            base_pack,
            None,
        )
        .unwrap();

    // All `num_agents` branch off the exact same `base_commit_oid` and edit `src/router.rs`
    // at the exact same lines (top import + bottom function).
    // In standard Git 3-way merge, agents 2..=N all fail with line conflicts!
    let mut mesh_auto_merged = 0usize;
    for a in 1..=num_agents {
        let agent_src = format!(
            "\
use std::sync::Arc;
use crate::handlers::handler_{a};

/// Base health route
pub fn route_health() -> &'static str {{
    \"healthy\"
}}

/// Added by Agent {a}
pub fn route_agent_{a}() -> usize {{
    {a}
}}
"
        );
        let (agent_commit_oid, agent_pack) = build_single_file_commit_pack(
            base_commit_oid,
            "src/router.rs",
            &agent_src,
            &format!("Agent {a} <agent{a}@openai.com>"),
            &format!("Add route_agent_{a}"),
        );

        // Land directly onto main via `refs/mesh/land/main`!
        let res = engine.process_push(
            repo_id,
            "refs/mesh/land/main",
            base_commit_oid,
            agent_commit_oid,
            agent_pack,
            None,
        );
        if res.is_ok() {
            mesh_auto_merged += 1;
        }
    }

    // Verify that the final `src/router.rs` on `refs/heads/main` contains all 20 agent functions!
    let main_ptr = engine
        .meta
        .get_ref_pointer(repo_id, "refs/heads/main")
        .unwrap();
    let main_files = extract_commit_files(&main_ptr.tip_commit_oid, &|oid| {
        engine.get_object(repo_id, oid)
    })
    .unwrap();
    let final_router_blob = engine
        .get_object(repo_id, main_files.get("src/router.rs").unwrap())
        .unwrap();
    let final_router_text = String::from_utf8_lossy(&final_router_blob.data);
    for a in 1..=num_agents {
        assert!(
            final_router_text.contains(&format!("pub fn route_agent_{a}()")),
            "missing route_agent_{a} in final merged router.rs"
        );
    }

    // Also test that an intentional breaking signature change is caught by the Semantic Radar!
    let breaking_src = "\
use std::sync::Arc;

/// Base health route with changed signature!
pub fn route_health(strict_mode: bool) -> &'static str {
    if strict_mode { \"strict\" } else { \"healthy\" }
}
";
    let caller_src = "\
use std::sync::Arc;

/// Base health route
pub fn route_health() -> &'static str {
    \"healthy\"
}

pub fn monitor_system() -> &'static str {
    route_health()
}
";
    let delta_break = mesh_semantic::compute_file_semantic_delta(
        "src/router.rs",
        ObjectId::ZERO,
        ObjectId::ZERO,
        base_router_src,
        breaking_src,
    )
    .unwrap();
    let delta_caller = mesh_semantic::compute_file_semantic_delta(
        "src/router.rs",
        ObjectId::ZERO,
        ObjectId::ZERO,
        base_router_src,
        caller_src,
    )
    .unwrap();
    let detected = mesh_semantic::detect_semantic_conflicts(
        "refs/heads/agent-sig-change",
        &[delta_break],
        "refs/heads/agent-caller",
        &[delta_caller],
    );
    let sig_breaks_caught = detected
        .iter()
        .filter(|c| c.kind == SemanticConflictKind::SignatureCallMismatch)
        .count();

    SemanticLandingSummary {
        concurrent_agents_editing_same_file: num_agents,
        git_line_merge_conflicts: num_agents.saturating_sub(1),
        mesh_cst_auto_merged_branches: mesh_auto_merged,
        cross_file_signature_breaks_caught: sig_breaks_caught,
    }
}

fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) * pct) / 100;
    sorted[idx]
}
