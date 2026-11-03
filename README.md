# Mesh

**Mesh** is an open-source, Pando-inspired causal Git backend and agent-native semantic version control engine written in Rust.

It combines two tightly integrated architectural layers while remaining **100% compatible with the standard `git` CLI**:

1. **Layer 1 — Causal Git Pack Engine (Open-Source Pando Alternative):**
   - **Per-Ref Causal Consistency ($O(1)$ Write Path):** Replaces global repository locks and shared WAL manifests with striped per-ref Compare-And-Swap (`RefPointer` + `RefVersion`). Concurrent pushes to distinct agent branches never contend.
   - **Trunk-Only Ref Advertisement (`< 256 bytes`):** Advertises only `refs/heads/main` (plus optional `explicit-haves` target refs) during `git-receive-pack`, keeping push negotiation constant-size even on repositories with millions of branches and ensuring topic packfiles are self-contained and reusable.
   - **Zero-Unpack Single-Pass Streaming Validation:** Validates incoming Git Packfile v2 streams (`PACK`, `OFS_DELTA`, `REF_DELTA`, SHA-1 trailers) in memory without ever invoking `git unpack-objects` or maintaining a bare repository on disk.
   - **LSM-Tree Skip-List Compaction (`L0` $\rightarrow$ `L4` Covers):** Background compaction consolidates contiguous `RefVersion` packs into higher-level covers while retaining lower-level packs, enabling $O(\log \Delta)$ fetch planning.
   - **Express Consistent-Hashed Pack CDN (`packfile-uri`):** Offloads packfile byte delivery to consistent-hashed regional LRU cache nodes.

2. **Layer 2 — Agent-Native Semantic Plane (Dual-Projection VCS):**
   - **Automatic CST Semantic Lifting:** Every incoming `git push` is automatically diffed at the Concrete Syntax Tree (CST) declaration level (`Import`, `Function`, `Struct`, `Enum`, `Trait`, `Impl`, `Class`, `Interface`, `Const`), indexing both declaration byte spans and call-site references.
   - **Server-Side CST-Commutative Landing (`refs/mesh/land/main`):** Standard Git clients can push branches based on an older trunk commit directly to `git push origin HEAD:refs/mesh/land/main` without pulling or rebasing. When multiple agents edit the same file (e.g. adding imports at the top and functions at the bottom), Mesh splices their verbatim CST byte spans on the server—preserving comments and formatting—and advances `refs/heads/main` with zero rebase storms.
   - **Cross-File Semantic Call-Graph Radar (`/api/v1/repos/:repo/radar`):** Detects silent semantic breakages (e.g. Agent A changing the signature of `charge()` in `src/billing.rs` while Agent B adds a call to `charge()` in `src/checkout.rs`) that standard line-based 3-way merges miss.

---

## Workspace Structure

```text
mesh/
├── PLAN.md                   # Full architectural scope & competitive analysis
├── Cargo.toml                # Rust workspace manifest
└── crates/
    ├── mesh-proto/           # Domain types: ObjectId, PackId, RefPointer, RefVersion, PackCover, SymbolDelta
    ├── mesh-pack/            # Single-pass Git Packfile v2 validator, delta resolver, tree builder & compactor
    ├── mesh-meta/            # Per-ref CAS causal metadata store & O(log Δ) skip-list interval cover planner
    ├── mesh-blob/            # Content-addressed packfile store + Express consistent-hashed LRU cache fleet
    ├── mesh-pktline/         # Git Smart HTTP v1/v2 pkt-line parser, trunk-only advertiser & packfile-uri framer
    ├── mesh-semantic/        # Verbatim CST byte-span 3-way merger & cross-file call-graph conflict radar
    ├── mesh-server/          # Axum Git Smart HTTP gateway + Express CDN + Agent Semantic REST API
    └── mesh-sim/             # 3PC vs. WAL vs. Mesh benchmark suite + real `git` CLI conformance tests
```

---

## Quick Start

### 1. Run the Full Test & Conformance Suite
Runs unit tests across all crates plus end-to-end integration tests invoking the system `git` binary (`git clone`, `git push`, `git push origin HEAD:refs/mesh/land/main`, and `/api/v1/repos/:repo/radar`):

```bash
cargo test --workspace
```

### 2. Run the Architecture Benchmark Suite (`3PC` vs. `WAL` vs. `Mesh`)
Simulates 200 concurrent agent writers pushing 1,000 commits across parallel branches, runs `L0 -> L1 -> L2` LSM compaction, measures Express cache hit rates, and lands 20 concurrent same-file agent edits onto `main` via CST-commutative splicing:

```bash
cargo run --release -p mesh-sim
```

### 3. Run the Live `mesh-server` Gateway
```bash
PORT=7474 cargo run --release -p mesh-server
```

Once running, standard `git` commands work directly against `http://127.0.0.1:7474/git/<repo>`:
```bash
# Push initial trunk
git push http://127.0.0.1:7474/git/monorepo main

# Land a concurrent agent branch via server-side CST-commutative merge (no local rebase needed)
git push http://127.0.0.1:7474/git/monorepo HEAD:refs/mesh/land/main

# Inspect pre-merge semantic conflicts & Express cache stats
curl http://127.0.0.1:7474/api/v1/repos/monorepo/radar
curl http://127.0.0.1:7474/api/v1/repos/monorepo/stats
```
