# Mesh: Architectural Scope & Engineering Plan

> **Context (November 2026):** On November 2, 2026, OpenAI announced **Pando** (*"Scaling Git at OpenAI"* by Aaron Friel, Taylor Blau, and Ted Nyman)—a custom Rust Git backend built in 10 weeks with Codex that cut median push latency by nearly 5× (~80%) and serves ~1 PB of Git data per day (peaking at 57 TB/hour with >99% regional cache hit rates). While OpenAI published client-side patches at [`github.com/openai/git`](https://github.com/openai/git), **the Pando backend itself remains closed-source**, as does Anthropic's competing internal system (**Scion**).
>
> This document scopes **Mesh**: a production-grade open-source architecture that addresses agent-scale version control across two strategic tracks—(A) an **open-source Pando-compatible distributed Git backend**, (B) an **agent-native version control system** that rethinks branches, commits, and merges, and (C) a **unified two-layer architecture** that delivers drop-in Git compatibility today while unlocking semantic, conflict-free multi-agent collaboration.

---

## 1. Current Landscape & Architectural Gap

### 1.1 What OpenAI Released vs. What Remains Closed

| Component | Status | Notes |
|---|---|---|
| **Pando Backend (Rust)** | **Closed source** | Stateless Git Smart HTTP v2 negotiation, streaming pack validator, causal ref store, LSM compaction workers, and "Express" regional pack cache. |
| **OpenAI Modified Git Client** | **Public** (`openai/git`) | Parallel `packfile-uri` download/indexing, `explicit-haves` for `--force-with-lease` against trunk-only ref advertisements, byte/latency tracing. |
| **Upstream Git Patches** | **Partially upstreamed** | Resumable/concurrent `packfile-uri` transfers, interrupted transfer cleanup, pack validation fixes. |
| **Full Pando Open-Source Release** | **Uncommitted** | *"We're currently exploring ways to share the technology that makes Pando possible with our users."* |

### 1.2 Why Existing Distributed Git Architectures Break Under Agent Workloads

When engineers orchestrate dozens of parallel coding agents (Codex, Claude Code, OpenCode), commit and push volume grows by >2,400% (as OpenAI reported between Q1 2024 and Q4 2026). Conventional Git hosting infrastructure fails because of two deeply embedded assumptions:

1. **Global Repository Consistency (The Write Bottleneck):**
   - **Three-Phase Commit (3PC) Replicas:** Every push replicates its packfile to a quorum of full repository replicas and coordinates ref locks across nodes. Pushes to completely unrelated branches (`agent/task-01` vs. `agent/task-02`) contend on replica disk I/O and multi-RTT quorum coordination.
   - **Object-Store Write-Ahead Log (WAL):** Servers stream packfiles to blob storage in parallel, but must atomically append to a **single repository-wide manifest** via Compare-And-Swap (CAS). Under high write concurrency, CAS retries spike on the shared manifest and read replicas fall behind indexing the log.
2. **Per-Client Dynamic Pack Generation (The Read Bottleneck):**
   - Standard `git-upload-pack` computes the exact reachability set difference (`WANT` commits minus `HAVE` commits) and dynamically delta-compresses a unique packfile for every `git fetch`.
   - Dynamic pack generation burns server CPU/RAM and produces bespoke responses that cannot be cached across readers.

---

## 2. Deconstructing Pando: The 5 Core Mechanisms

OpenAI's architecture disclosure provides a clear blueprint for how Pando decouples write scaling from read scaling:

```text
  Agent / Git Client                                  Mesh / Pando Server Cluster
  ==================                                  ===========================

  1. git push origin agent-branch
     (Only `main` advertised + explicit-haves)
     ------------------------------------------->  [Single-Pass Pack Stream Validator]
                                                            |                  |
                                    Stream immutable        |                  | O(1) Multi-Key CAS Tx
                                    L0 .pack + .idx         v                  v
                                                  +------------------+  +--------------------+
                                                  |   Blob Storage   |  |   Metadata Store   |
                                                  | (S3 / R2 / GCS)  |  | (Per-Ref Versions, |
                                                  +------------------+  |  Commit->RefVer)   |
                                                            ^           +--------------------+
                                                            |                  ^
                                                   Background LSM              | O(log Δ) Skip-List
                                                   Compaction (L0->L4)         | Cover Planning
                                                            |                  |
  2. git fetch origin                                       |                  |
     <------------------------------------------------------+------------------+
     Returns empty inline pack + `packfile-uri` list
     pointing to regional Express CDN caches (99%+ hit rate, 10 Gbps line rate)
```

1. **Causal Consistency (CC) & Per-Ref Serializability:**
   - Drops repository-wide point-in-time snapshots across *all* branches in favor of **per-reference serializability** and **read-your-own-writes causal consistency**.
   - Updating `refs/heads/agent-42` only performs an atomic CAS on `refs/heads/agent-42` and writes a new `RefVersion` record pointing to the uploaded packfile and the `trunk` (`main`) `RefVersion` it depends on.
   - Unrelated branch pushes never contend ($O(1)$ metadata reads and writes per push).
2. **Trunk-Only Ref Advertisement + `explicit-haves`:**
   - With millions of agent branches, advertising all refs during push negotiation exceeds 100 MiB and causes clients to produce "thin" packs that depend on arbitrary side branches (making those packs non-reusable for other readers).
   - By advertising **only `refs/heads/main`** as an exclusion base (plus `explicit-haves` so `--force-with-lease` still validates the feature branch tip without excluding its objects), every push to a topic branch contains the self-contained delta between `main@vN` and the topic tip.
   - Median topic packfile size remains **<20 KiB** (p99 **<3 MiB**), and *any* reader who has `main@vN` can fetch the entire topic branch in a single reusable packfile.
3. **Zero-Unpack Streaming Validation:**
   - Stateless negotiation nodes never maintain a local bare Git repository on disk and never run `git unpack-objects`.
   - Incoming packfiles are validated in a single streaming pass (verifying SHA-1/SHA-256 checksums, object headers, and delta base hashes against the advertised trunk index) while streaming directly to immutable blob storage.
4. **Zero-Compute Fetches via `packfile-uri`:**
   - Using Git Protocol v2's `packfile-uri` capability, the server responds to `fetch`/`clone` with an **empty inline packfile** and a list of immutable URLs for the required stored packfiles.
   - Clients download and index the packs directly from regional consistent-hashed cache nodes ("Express") in parallel.
5. **LSM-Tree Packfile Compaction (`L0` $\rightarrow$ `L4` Covers):**
   - Each push creates an immutable `L0` packfile. Background workers merge contiguous sequences of `RefVersion` packs into geometrically larger covers (`L1` through `L4`).
   - Old packfiles are **retained at every level** rather than garbage-collected; `L0`–`L4` covers act simultaneously as higher-compression byte bundles and a **skip list over ref versions**, reducing fetch planning to $O(\log \Delta)$ metadata reads.

---

## 3. Direction A: Open-Source Pando Alternative (`Mesh-Core`)

### 3.1 Thesis & Positioning
> **"Open-source, horizontally scalable Git infrastructure."** A stateless Rust Git Smart HTTP v2 backend and regional pack cache that brings causal branch consistency, zero-unpack object storage, `packfile-uri` CDN offloading, and LSM pack compaction to any engineering org or forge.

### 3.2 System Architecture

```text
+-------------------------------------------------------------------------+
|                    Stateless Mesh Gateway Node (Rust)                   |
|                                                                         |
|  +-----------------------+    +--------------------------------------+  |
|  | Git Smart HTTP v2 API |    | Read/Write-Through Upstream Proxy    |  |
|  | - /info/refs          |    | (Zero-downtime migration mode for    |  |
|  | - /git-receive-pack   |    |  existing GitHub / GitLab / Gitea)   |  |
|  | - /git-upload-pack    |    +--------------------------------------+  |
|  +-----------+-----------+                                              |
|              |                                                          |
|  +-----------v-----------+    +--------------------------------------+  |
|  | Streaming Pack Engine |    | O(log Δ) Skip-List Fetch Planner     |  |
|  | (Single-pass decode,  |    | Resolves HAVE/WANT -> Cover URIs     |  |
|  |  idx gen & validation)|    +------------------+-------------------+  |
|  +-----+-----------+-----+                       |                      |
+--------+-----------+-----------------------------+----------------------+
         |           |                             |
         |           |                             v
         |           |          +--------------------------------------+
         |           |          |   Express Regional Pack Cache Fleet  |
         |           |          |   - Consistent-hashed NVMe/RAM nodes |
         |           |          |   - Serves /packs/:pack_id.pack      |
         |           |          +------------------+-------------------+
         |           |                             | (Cache fill on miss)
         v           v                             v
+--------------------+----------+ +----------------+----------------------+
|     Immutable Blob Store      | |   Distributed Transactional KV Store  |
|     (S3 / R2 / GCS / MinIO)   | |   (FoundationDB / TiKV / Postgres)    |
|  - L0..L4 .pack & .idx blobs  | |   - ref_pointers (per-ref CAS)        |
|  - Content-addressed by hash  | |   - ref_versions & dependency graph   |
+-------------------------------+ |   - commit_to_ref_version index       |
                                  |   - pack_covers (L0..L4 skip-list)    |
                                  +---------------------------------------+
```

### 3.3 Core Data Model (Transactional Metadata Store)

```rust
/// Atomic pointer per branch — the ONLY contended key on push (per-ref CAS)
pub struct RefPointer {
    pub repo_id: RepoId,
    pub ref_name: String,                // e.g., "refs/heads/main" or "refs/heads/agent-42"
    pub current_version: u64,            // Monotonic per-ref version counter
    pub tip_commit_oid: ObjectId,        // Current SHA-1 / SHA-256 commit hash
    pub base_trunk_version: Option<u64>, // If side branch, trunk version it forked/based from
}

/// Immutable record written on every push to a ref
pub struct RefVersion {
    pub repo_id: RepoId,
    pub ref_name: String,
    pub version: u64,
    pub old_commit_oid: ObjectId,
    pub new_commit_oid: ObjectId,
    pub l0_pack_id: PackId,              // Content-addressed hash of the uploaded L0 packfile
    pub explicit_haves: Vec<ObjectId>,   // Base commits excluded during pack negotiation
    pub base_trunk_version: Option<u64>, // Causal dependency on refs/heads/main@vK
    pub created_at_ms: u64,
}

/// Inverted index mapping commit OIDs to the RefVersion that introduced them
/// Note: For very large pushes, root/tip commits are indexed synchronously (O(1))
/// and interior commits are indexed asynchronously by background workers.
pub struct CommitLocation {
    pub repo_id: RepoId,
    pub commit_oid: ObjectId,
    pub ref_name: String,
    pub ref_version: u64,
}

/// LSM-style Compaction Cover spanning [start_version, end_version] on a ref
pub struct PackCover {
    pub repo_id: RepoId,
    pub ref_name: String,
    pub level: u8,                       // L0 = 1 push, L1 = 8, L2 = 64, L3 = 512, L4 = 4096
    pub start_version: u64,
    pub end_version: u64,
    pub pack_id: PackId,                 // Consolidated packfile + .idx in Blob Store
    pub byte_size: u64,
    pub object_count: u32,
}
```

### 3.4 Key Protocol & Storage Mechanics

1. **Synchronous vs. Asynchronous Push Split ($O(1)$ Write Path):**
   - To guarantee constant-time metadata transactions regardless of push size, `git-receive-pack` streams the incoming pack to blob storage, validates object integrity in a single pass, and executes a **single multi-key transaction** that:
     1. Performs CAS on `RefPointer(repo_id, ref_name)`.
     2. Writes the new `RefVersion` and `PackCover(level = 0)`.
     3. Writes `CommitLocation` for the new branch tip (and boundary commits).
     4. Appends an outbox task for asynchronous background work (indexing interior commits for large pushes and triggering `L0` $\rightarrow$ `L1` compaction).
2. **$O(\log \Delta)$ Interval Cover Fetch Planning:**
   - Given client `have` OIDs and `want` OIDs, the planner resolves `have` to trunk version $v_{\text{have}}$ and `want` to target branch version $v_{\text{want}}$ (with dependency on trunk version $v_{\text{base}}$).
   - It computes the minimal cover of interval $(v_{\text{have}}, v_{\text{base}}]$ by greedily stepping through `PackCover` levels `L4` $\rightarrow$ `L0` like a skip list, then appends the single topic `L0` pack for $v_{\text{want}}$.
3. **Zero-Downtime Read/Write-Through Proxy Mode:**
   - Just as OpenAI deployed Pando initially as a read/write-through proxy in front of its legacy Git backend, `Mesh-Core` should support an upstream proxy mode. This allows organizations to deploy Mesh in front of GitHub Enterprise, GitLab, or Gitea to offload 99% of CI/agent read traffic and coalesce writes before cutting over as the primary system of record.

---

## 4. Direction B: Agent-Native Version Control System (`Mesh-VCS`)

### 4.1 Why Scaling Git Transport Is Only Half the Problem
Pando solves the **transport and ref-lock bottleneck** when 1,000 agents push to *separate* branches. However, preserving Git's 2005 data model leaves a second bottleneck untouched the moment those 1,000 agents attempt to merge their branches back into `main`:

1. **The Merge Queue Death Spiral:** When 100 agents finish tasks concurrently, 99 fail fast-forward merges onto `main`, triggering cascading rebase-and-retest loops ($O(N^2)$ CI compute waste).
2. **False Line-Based Conflicts:** Two agents adding independent functions at the bottom of `service.rs` or adding imports at the top of `index.ts` trigger a Git line conflict, halting autonomous workflows even though the edits are semantically orthogonal.
3. **Silent Semantic Breakages:** Agent A updates `fn verify_token(token: &str)` to `fn verify_token(token: &str, audience: &str)` in `auth.rs`. Agent B adds a call `verify_token(t)` in `billing.rs`. Git merges both cleanly with **zero textual conflict**—and breaks the build on `main`.
4. **Loss of Agent Provenance & Verification State:** A Git commit only stores a flat string message and a file tree snapshot. It discards the structured agent trajectory, prompt intent, AST read/write footprint, and cryptographic attestations of which tests/linters already passed on the isolated subtree.

### 4.2 Core Primitives of `Mesh-VCS`

| Git Primitive | `Mesh-VCS` Agent-Native Primitive | Architectural Advantage |
|---|---|---|
| **Working Directory Checkout** | **Virtual Copy-on-Write Workspace** | Sub-millisecond workspace creation via content-addressed virtual FS / overlay; 1,000 agents work concurrently without duplicating disk trees. |
| **Commit (Opaque Snapshot)** | **Typed Intent Patch (`PatchNode`)** | Records both content-addressed blobs *and* parsed `tree-sitter` AST operations (`InsertSymbol`, `ModifySymbol`, `RenameSymbol`) + signed Agent Verification Receipts. |
| **Branch (`refs/heads/*`)** | **Causal Patch Graph** | Changes depend only on the exact files/AST symbols they read or mutate, not chronological trunk order. Orthogonal patches commute automatically. |
| **3-Way Line Merge** | **AST-Commutative + Call-Graph Merge** | Non-overlapping symbol edits in the same file merge automatically; cross-file signature/call mismatches are flagged *before* merge. |

### 4.3 Semantic Commutativity & Conflict Resolution Flow

```text
                       Agent B mutates same AST symbol
                       or calls symbol mutated by Agent A?
                                /               \
                              YES                NO
                              /                   \
             [True Semantic Conflict]      [Same file touched?]
             Invoke bounded resolver           /            \
             with Intent Receipts +          YES             NO
             incremental subtree test        /                \
                                  [AST-Level Auto-Merge]  [Zero-Copy Tree
                                   Tree-sitter splices     Pointer Union]
                                   orthogonal declarations
```

### 4.4 How Direction B Stays 100% Git-Compatible (Dual-Projection Architecture)

You **do not** have to abandon Git compatibility to build an agent-native VCS. Taking inspiration from **Jujutsu (`jj`)**, **Sapling**, and **Gerrit**, `Mesh-VCS` can treat **Git objects (`blob`, `tree`, `commit`) as the canonical storage and wire-transfer substrate**, while maintaining the **Semantic Patch Graph** as the brain on top.

```text
   Human / Legacy CI                           Swarm Coding Agents
   (Standard `git` CLI)                        (Mesh CLI / MCP / gRPC)
          |                                               |
          | git push / fetch                              | apply_patch / land_workspace
          v                                               v
  +-------------------------------------------------------------------+
  |                     Mesh Dual-Projection Engine                   |
  |                                                                   |
  |  [Git Wire Adapter]  <--- Bi-directional --->  [Semantic DAG]     |
  |  - Lifts incoming Git     Projection           - Intent Patches   |
  |    commits into AST                            - Symbol Read/Write|
  |    PatchNodes via                              - Commutative AST  |
  |    tree-sitter diff                              Splicer          |
  |  - Synthesizes Git                             - Call-Graph Radar |
  |    blobs/trees/commits                                            |
  +-------------------------------------------------------------------+
                                    |
                                    v
              +-------------------------------------------+
              |   Pando-Style Causal Git Object Store     |
              |   (Standard Git Packfiles + Metadata KV)  |
              +-------------------------------------------+
```

This works through four concrete mechanisms:

1. **Commits as Materialized Projections of Semantic Patches:**
   - Every file is still stored as a standard Git `blob`, every directory is a Git `tree`, and every state on a branch is a valid Git `commit`.
   - When an agent submits an intent patch via the native Mesh API (without even having a local `.git` checkout), Mesh constructs the new `blob` and `tree` hashes in memory and writes a valid Git `commit` object into the pack store. Any human who runs `git fetch` immediately sees the agent's work as normal Git commits.
2. **Automatic "Lifting" of Standard `git push` into the Semantic Graph:**
   - When a human or existing tool runs a standard `git push`, Mesh accepts the packfile, diffs the parent and child trees using `tree-sitter`, and automatically lifts the raw commit into a `PatchNode` with extracted AST symbol reads/writes.
3. **Magic Ref Namespaces for Server-Side Commutative Merging (`refs/mesh/land/*`):**
   - Standard Git clients don't have an AST merge command—they only know `git push`.
   - By supporting a virtual target ref—`git push origin HEAD:refs/mesh/land/main` (similar to Gerrit's `refs/for/main`)—any standard Git client can push a branch based on an *older* version of `main` without pulling or rebasing first.
   - Instead of rejecting the push as a non-fast-forward (`! [rejected] main -> main (fetch first)`), Mesh intercepts pushes to `refs/mesh/land/main`, runs its **server-side AST-commutative merge** against the current `main` tip, synthesizes the merged Git `tree` and `commit`, and atomically advances `refs/heads/main`.
4. **Lossless Metadata via Git Notes & Commit Trailers:**
   - Agent provenance (prompt hash, model ID, AST scope, verification receipts) is stored in Mesh's fast metadata store for $O(1)$ queries, and simultaneously projected into `refs/notes/mesh` (or commit trailers like `Mesh-Change-Id:`). This ensures agent metadata survives round-trips through vanilla Git tools, GitHub mirrors, or local checkouts.

---

## 5. Strategic Synthesis: Choosing vs. Combining the Two Directions

### 5.1 Comparative Evaluation

| Dimension | Direction A: Open-Source Pando (`Mesh-Core`) | Direction B: Agent-Native VCS (`Mesh-VCS`) | **Unified Architecture: `Mesh` (Causal Git Engine + Semantic Plane)** |
|---|---|---|---|
| **Primary Value Prop** | Drop-in 5× faster pushes & 10 Gbps cached fetches for existing Git monorepos. | Eliminates rebase storms, false line conflicts, and merge queue bottlenecks for agent swarms. | Scales **both** parallel agent writes/reads (Layer 1) **and** concurrent agent merges into trunk (Layer 2). |
| **Specification Risk** | **Low** — Protocol v2, `packfile-uri`, and LSM covers have clear, testable invariants. | **Medium–High** — Requires ergonomic CLI/MCP interfaces and language-aware merge rules. | **Controlled** — Layer 1 provides a rock-solid, Git-compatible foundation; Layer 2 layers cleanly on top of indexed packs. |
| **Adoption Friction** | **Near Zero** — Works with standard `git` and `openai/git`; deployable as a read/write proxy. | **Higher if standalone** — Teams hesitate to abandon Git tooling and CI integrations. | **Near Zero** — Standard `git push`/`fetch` works out of the box; agents opt into semantic workspaces & merge APIs. |
| **Defensibility / Originality** | First open-source Pando implementation, but follows OpenAI's published design. | High architectural originality (competes with Anthropic's Scion, Jujutsu, Entire). | **Highest** — Solves the full lifecycle of agentic coding (transport + storage + semantic merge). |

### 5.2 Recommended Architecture: The Two-Layer `Mesh` Stack

Rather than treating Direction A and Direction B as mutually exclusive, **Mesh should be architected as a two-layer system** where Layer 1 is a Pando-compatible causal pack engine and Layer 2 is an agent-native semantic indexing and merge plane that operates directly on Layer 1's immutable packfiles:

1. **Layer 1 (`mesh-storage` & `mesh-git-gateway`):**
   - Full Pando-inspired causal Git backend: per-ref CAS, trunk-only ref advertisement + `explicit-haves`, single-pass streaming pack validation, `L0`–`L4` LSM compaction covers, and `packfile-uri` Express CDN caching.
   - Guarantees 100% compatibility with existing Git clients, CI runners, and human workflows.
2. **Layer 2 (`mesh-semantic` & `mesh-agent-api`):**
   - As Layer 1 streams and indexes incoming `L0` packfiles from agents, Layer 2 parses changed blobs with `tree-sitter` and records symbol-level read/write footprints (`SymbolDelta`) alongside each `RefVersion`.
   - Exposes three agent-native capabilities over MCP / gRPC / CLI:
     - **Pre-Merge Semantic Conflict Radar:** Real-time detection of overlapping AST symbol edits or cross-file signature/call-site conflicts across all active agent branches.
     - **Batch Commutative Trunk Landing (`mesh land`):** Combines $N$ non-conflicting agent branches into a single compacted trunk `RefVersion` without sequential rebasing—solving the Merge Queue Death Spiral even when multiple agents edited the same file.
     - **Provenance & Verification Ledger:** Attaches structured agent intent, tool traces, and subtree test receipts to every `RefVersion` so CI only re-runs tests affected by the combined semantic delta.

### 5.3 Honest Competitive Analysis: Mesh vs. Jujutsu (`jj`), Sapling, and Gerrit

To build a system that genuinely advances the state of the art, we must be clear-eyed about both **where Mesh wins** against the best existing systems and **where Mesh could be worse** if designed naively.

#### Why `jj`, Sapling, and Gerrit Fall Short for Agent Swarms
All three systems were architected for **human developers operating at human concurrency**:
- **Jujutsu (`jj`):** A brilliant **client-side** local VCS (working-copy-as-a-commit, stable `Change-Id`, algebraic conflict storage, `jj op log` / `jj undo`). However, `jj` is fundamentally a *single-player local tool*. When you run `jj git push`, it falls back to standard Git remote semantics. Furthermore, `jj`'s merge engine is still unstructured line/byte 3-way merge—not AST-aware—and it has zero server-side visibility into 500 other agents working in parallel cloud sandboxes.
- **Sapling (`sl` + Mononoke + EdenFS):** Meta's monorepo stack excels at lazy virtual filesystem checkouts (`EdenFS`) and human stacked diffs. However, it requires a heavyweight proprietary daemon (`EdenFS`) and custom wire protocol (`EdenAPI`), breaks standard drop-in `git` compatibility, and still relies on serialized landing queues (`Landcastle`) with line-based merges.
- **Gerrit:** Pioneered server-side virtual refs (`refs/for/main`) and `Change-Id` tracking, but is built on JGit with a monolithic ref database. Every patchset creates a new ref (`refs/changes/*`), leading to the exact >100 MiB ref-advertisement bloat, dynamic pack generation CPU spikes, and global lock contention that forced OpenAI to build Pando.

#### Why Mesh Will Be Better (Core Differentiators)

| Capability | Jujutsu (`jj`) | Sapling + Mononoke | Gerrit | **Mesh** |
|---|---|---|---|---|
| **Multi-Agent Write Scaling** | Client-only (uses standard Git remote) | Custom `EdenAPI` server | Bottlenecks on JGit & ref count | **Pando-style per-ref CAS ($O(1)$) + zero-unpack streaming** |
| **Read / Clone Throughput** | Standard Git pack negotiation | Custom lazy blob fetch | Dynamic JGit pack generation | **`packfile-uri` Express CDN (>99% cache hit, 10 Gbps)** |
| **Same-File Parallel Edits** | Line-based 3-way (conflicts on adjacent lines) | Line-based 3-way | Line-based 3-way (rejects submit) | **CST/AST-commutative merge (splices orthogonal symbols)** |
| **Cross-File Breaking Changes** | Silent clean merge (breaks build) | Silent clean merge (breaks build) | Silent clean merge (breaks build) | **Semantic Symbol/Import Radar catches signature drift** |
| **Drop-in Stock `git` Support** | Requires `jj` CLI locally | Requires `sl` + `EdenFS` | Yes (`refs/for/*`) | **Yes (`git` CLI, `jj` CLI, + native Agent MCP API)** |

#### Why Mesh Could Be Worse (Failure Modes & Engineering Mitigations)

1. **Risk 1: The "AST Merge Uncanny Valley" (Formatting, Comments, Macros & Broken Syntax)**
   - *Why `jj`/Git can be better:* Line-based 3-way merge is dumb, deterministic, language-agnostic, and works on broken code, C macros, YAML, and prose. Naive AST parsing & re-printing destroys formatting, drops comments, or fails completely when an agent commits a syntax error.
   - *How Mesh mitigates this:*
     - **Never pretty-print ASTs.** Use **Concrete Syntax Tree (CST) byte-span splicing** (inspired by `mergiraf` / `gumtree`): `tree-sitter` is only used to identify the exact byte ranges of top-level declarations (`fn`, `struct`, `import`, `class`) and their leading trivia/comments. The original byte slices are spliced verbatim.
     - **Graceful Degradation:** If a file has syntax errors (`ERROR` nodes in `tree-sitter`), unsupported languages, or overlapping CST spans, Mesh immediately falls back to `jj`-style algebraic conflict representation rather than guessing.

2. **Risk 2: False-Positive Semantic Conflicts (`tree-sitter` Is Not a Typechecker)**
   - *Why naive symbol tracking fails:* `tree-sitter` parses single files without cross-file type resolution. If Agent A changes `User::save()` in `user.rs` and Agent B calls `order.save()` in `order.rs`, a naive identifier matcher sees `.save()` in both and flags a false-positive conflict. Conversely, running a full compiler (`rust-analyzer`, `tsc`) on the push path would add 10–60 seconds of latency, destroying Pando's <5ms push speed.
   - *How Mesh mitigates this:*
     - Keep the synchronous `git-receive-pack` path **100% syntactic and $O(1)$**.
     - Scope cross-file conflict detection using **explicit module import/export edges** (e.g., `use crate::user::User` + symbol `save`) rather than un-scoped method names, and allow CI/LSP workers to asynchronously enrich the symbol graph via SCIP/LSIF indexes off the critical push path.

3. **Risk 3: Re-Uploading Stacked Branches Under Trunk-Only Ref Advertisement**
   - *Why standard Git/Gerrit can be better on stacked diffs:* Because Pando/Mesh only advertises `refs/heads/main` during push negotiation, if an agent pushes 20 commits to `feature-a` (or commits a 25 MiB binary asset) and then pushes `feature-b` stacked on `feature-a`, the client will re-upload `feature-a`'s objects on every push.
   - *How Mesh mitigates this:*
     - Support Pando's `explicit-haves` extension (and URL/header hints like `Git-Protocol: version=2:base-ref=refs/heads/feature-a`) so stacked branches can explicitly declare a parent topic branch as a second causal dependency in `RefVersion`, bounding pack size while keeping the ref advertisement $O(1)$.

4. **Risk 4: Competing With `jj` on Local Human CLI Ergonomics**
   - *Why `jj` is better locally:* `jj` has spent years perfecting local history manipulation (`jj absorb`, `jj split`, `jj undo`, working-copy auto-snapshot). Building a competing local human CLI from scratch would almost certainly produce an inferior local UX.
   - *How Mesh mitigates this:*
     - **Embrace `jj` as a first-class client.** Because Mesh speaks standard Git Smart HTTP v2 and preserves `Change-Id` trailers / `refs/notes`, human developers can run `jj` locally (`jj git push` / `jj git fetch`) while Mesh powers the causal backend, `packfile-uri` cache, and server-side AST landing queue (`refs/mesh/land/main`).

5. **Risk 5: Local Disk Checkout Bottlenecks on 5M-File Monorepos (vs. Sapling's `EdenFS`)**
   - *Why Sapling is better on massive working trees:* Saturating a 10 Gbps network link via `packfile-uri` downloads the packfiles in seconds, *but standard `git checkout` still has to materialize files onto the local disk*.
   - *How Mesh mitigates this:*
     - Pair `packfile-uri` with Git's native `sparse-checkout` + `--filter=blob:none` (partial clone backed by Express cache nodes for instant blob hydration), and provide a zero-checkout **Server-Side Workspace API (MCP)** for cloud agents that only read/write a handful of files per task.

---

## 6. Repository Architecture & Module Breakdown (`/Users/w3joe/Desktop/mesh`)

```text
mesh/
├── PLAN.md                        # Architectural scope & engineering roadmap
├── Cargo.toml                     # Rust workspace manifest
├── crates/
│   ├── mesh-proto/                # Shared domain types, gRPC/HTTP schemas, ObjectId/PackId primitives
│   ├── mesh-pack/                 # Single-pass streaming pack decoder, .idx builder & L0->L4 LSM compactor
│   ├── mesh-meta/                 # Pluggable transactional KV store trait (Memory, SQLite/Postgres, TiKV/FDB)
│   ├── mesh-blob/                 # Immutable packfile store (Local NVMe, S3/R2/MinIO) + Express LRU cache
│   ├── mesh-pktline/              # Git Smart HTTP v2 parser, trunk-only ref advertiser & packfile-uri framer
│   ├── mesh-semantic/             # Tree-sitter AST symbol extractor, call-graph index & commutative merger
│   ├── mesh-server/               # Stateless gateway binary + Express cache server + Agent MCP/HTTP API
│   └── mesh-sim/                  # Deterministic Simulation Testing (DST) & 1,000-writer benchmark harness
└── docs/
    ├── invariants.md              # Formal safety & causal consistency invariants (TLA+ / property specs)
    └── protocol-v2-extensions.md  # Specification for packfile-uri, explicit-haves, and Intent Receipts
```

### Key Technical Dependencies (Rust Ecosystem)
- **Git Object & Pack Handling:** `gix-pack`, `gix-object`, `gix-hash`, `gix-packetline` (from `gitoxide`—pure Rust, memory-safe, high-throughput streaming primitives).
- **Async Runtime & HTTP/2:** `tokio`, `axum`, `hyper`, `tower` with streaming body readers/writers for zero-copy pack passthrough.
- **Metadata Store Backends:** Trait-abstracted transactional KV layer (`MeshMetadataStore`) supporting:
  - Embedded/Single-Node: `sqlite` (WAL mode) or `redb` / `rocksdb` for self-hosted deployments.
  - Distributed Production: `foundationdb` or `tikv-client` / `postgres` for multi-node horizontal scaling.
- **Object Storage & Caching:** `object_store` crate (unified AWS S3, Cloudflare R2, GCS, and local filesystem API) + custom NVMe/memory consistent-hashed ring for Express cache nodes.
- **AST & Semantic Analysis:** `tree-sitter` + language grammars (`Rust`, `TypeScript`, `Python`, `Go`) for incremental symbol extraction and commutative AST splicing.

---

## 7. Verification, Reliability & Benchmarking Strategy

OpenAI noted that two practices were critical to shipping Pando in 10 weeks with high reliability: **Git conformance testing** and **Deterministic Simulation Testing (DST)** backed by formal invariant checking. Mesh should adopt this discipline from day one:

### 7.1 Core System Invariants (To Enforce in `mesh-sim`)
1. **Read-Your-Own-Writes Causal Consistency:** Once a push to `ref R` succeeds at `version V`, any subsequent fetch of `R` at `version >= V` must return a set of `PackCover` URIs whose union contains every object reachable from `R@V` (assuming the client holds the advertised `base_trunk_version`).
2. **Per-Ref Linearizability:** Concurrent pushes to the same `ref R` from the same `old_oid` must serialize via CAS such that exactly one succeeds and all others fail with a non-fast-forward / stale-lease rejection.
3. **Zero Cross-Ref Blocking:** Progress on `ref R1` must remain wait-free with respect to concurrent pushes, compactions, or failures on `ref R2`.
4. **Compaction Cover Equivalence:** Replacing a contiguous sequence of `L(k)` covers $[v_a, v_b]$ with an `L(k+1)` cover must preserve exact object reachability and bit-for-bit blob reconstruction.
5. **Semantic Merge Soundness (Layer 2):** Two `RefVersion` patches on the same file may auto-commute if and only if their parsed AST modification spans are disjoint at the declaration level and neither modifies a symbol referenced by the other.

### 7.2 Performance Benchmark Targets
- **Write Scalability:** Linear throughput scaling with 1,000+ concurrent branch writers; p50 push metadata overhead **< 5 ms** (excluding client network upload time).
- **Ref Advertisement Size:** Constant **O(1) payload (< 512 bytes)** for push negotiation on repositories with 1,000,000+ refs.
- **Read Throughput & Cache Offloading:** **> 99% of fetched bytes** served via `packfile-uri` from Express cache nodes; gateway CPU utilization independent of fetched byte volume.
- **Fetch Planning Latency:** **< 2 ms** to compute `PackCover` URI list across a 10,000-version trunk delta using `L0`–`L4` skip-list covers.
- **Merge Queue Throughput (Layer 2):** **5–10× reduction** in false-positive merge conflicts and rebase cycles when landing 50 concurrent agent branches touching shared module files.

---

## 8. Phased Engineering Roadmap

### Milestone 1: Causal Storage & Single-Pass Pack Engine (`mesh-meta`, `mesh-pack`, `mesh-blob`)
- Define the `MeshMetadataStore` trait and implement the in-memory + transactional SQLite/Postgres backends (`RefPointer` CAS, `RefVersion`, `CommitLocation`, `PackCover`).
- Build the streaming pack validator using `gix-pack` that verifies incoming pack streams on the fly, generates `.idx` indexes in memory, validates delta bases against trunk indexes, and writes immutable `L0` packs to `object_store`.
- Build the deterministic simulation harness (`mesh-sim`) to fuzz concurrent ref updates and verify causal consistency invariants.

### Milestone 2: Git Smart HTTP v2 Gateway & `packfile-uri` Express Cache (`mesh-pktline`, `mesh-server`)
- Implement Git Protocol v2 endpoints (`GET /info/refs`, `POST /git-receive-pack`, `POST /git-upload-pack`).
- Implement trunk-only ref advertisement (`refs/heads/main`) + `explicit-haves` negotiation so standard `git` and `openai/git` clients produce self-contained topic packs.
- Implement the $O(\log \Delta)$ skip-list fetch planner that emits `packfile-uri` links + an empty 32-byte inline packfile.
- Build the Express cache endpoint (`GET /packs/:pack_id.pack`) with NVMe/memory caching and Prometheus/OpenTelemetry metrics.

### Milestone 3: LSM Compaction Workers & Upstream Proxy Mode
- Implement background compaction workers (`L0` $\rightarrow$ `L1` $\rightarrow$ `L2` $\rightarrow$ `L3` $\rightarrow$ `L4`) that consolidate geometric ranges of trunk `RefVersion` packs without blocking reads or deleting active lower-level packs.
- Implement read/write-through upstream proxy mode so Mesh can sit transparently in front of an existing Git server (GitHub, GitLab, Gitea) for incremental rollout.

### Milestone 4: Agent-Native Semantic Plane (`mesh-semantic`)
- Hook `tree-sitter` symbol extraction into the async post-push pipeline to index AST declarations and symbol dependencies per `RefVersion`.
- Build the **Speculative Conflict Radar** API (queryable by coding agents via MCP or CLI) to surface overlapping symbol mutations across active branches in real time.
- Build `mesh land`: the batch commutative merge engine that splices orthogonal AST edits across multiple agent branches into a single trunk `RefVersion` without sequential checkout or rebase storms.
