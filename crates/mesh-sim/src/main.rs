use mesh_sim::run_full_benchmark_suite;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("================================================================================");
    println!("  MESH: Open-Source Pando Causal Git Engine + Semantic Agent VCS Benchmark");
    println!("================================================================================\n");

    let num_writers = 200;
    let pushes_per_writer = 5;
    println!(
        "Running concurrent agent simulation: {} parallel branch writers x {} pushes ({} total pushes)...\n",
        num_writers,
        pushes_per_writer,
        num_writers * pushes_per_writer
    );

    let report = run_full_benchmark_suite(num_writers, pushes_per_writer).await;

    println!("1. WRITE & READ SCALABILITY (3PC vs. Object-Store WAL vs. Mesh-Pando)");
    println!("--------------------------------------------------------------------------------");
    println!(
        "{:<48} | {:>10} | {:>10} | {:>11} | {:>9} | {:>9}",
        "Architecture", "Wall (ms)", "p50 (us)", "CAS Retries", "Adv Bytes", "Cache Hit"
    );
    println!("--------------------------------------------------------------------------------");
    for r in [
        &report.three_phase_commit,
        &report.object_store_wal,
        &report.mesh_pando,
    ] {
        println!(
            "{:<48} | {:>10.2} | {:>10} | {:>11} | {:>9} | {:>8.1}%",
            r.architecture,
            r.wall_clock_ms,
            r.p50_push_latency_us,
            r.cas_retries,
            r.ref_advertisement_bytes,
            r.express_cache_hit_rate_pct
        );
    }
    println!();

    println!("2. LSM SKIP-LIST PACK COMPACTION (L0 -> L1 -> L2 Covers)");
    println!("--------------------------------------------------------------------------------");
    let c = &report.lsm_compaction_summary;
    println!("  Trunk RefVersions pushed          : {}", c.trunk_versions);
    println!(
        "  Uncompacted L0 packs for fetch    : {} packs",
        c.uncompacted_l0_packs_needed
    );
    println!(
        "  Compacted Skip-List cover plan    : {} packs ({} metadata reads)",
        c.compacted_skip_list_packs_needed, c.metadata_reads_for_fetch_plan
    );
    println!("  Selected covers                   : {:?}", c.selected_cover_levels);
    println!();

    println!("3. LAYER 2: SERVER-SIDE CST-COMMUTATIVE MERGE (`refs/mesh/land/main`)");
    println!("--------------------------------------------------------------------------------");
    let s = &report.semantic_landing_summary;
    println!(
        "  Concurrent agents editing `src/router.rs` : {}",
        s.concurrent_agents_editing_same_file
    );
    println!(
        "  Standard Git 3-way line merge conflicts   : {}/{} failed (rebase storm)",
        s.git_line_merge_conflicts, s.concurrent_agents_editing_same_file
    );
    println!(
        "  Mesh CST-Commutative auto-landed branches : {}/{} landed cleanly (0 rebases)",
        s.mesh_cst_auto_merged_branches, s.concurrent_agents_editing_same_file
    );
    println!(
        "  Signature/Call-Graph breaks intercepted   : {}",
        s.cross_file_signature_breaks_caught
    );
    println!("================================================================================");

    Ok(())
}
