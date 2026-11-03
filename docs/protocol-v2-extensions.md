# Mesh Git Protocol v2 Extensions & Agent API Specification

## 1. Trunk-Only Ref Advertisement & `target-ref` Negotiation
During `GET /git/:repo/info/refs?service=git-receive-pack`:
- By default, Mesh advertises only `refs/heads/main` so that the advertisement payload remains $O(1)$ (`< 256 bytes`) regardless of whether the repository has 10 or 10,000,000 agent branches.
- Clients updating an existing topic branch with `--force-with-lease` may supply the target branch via either:
  - Query parameter: `?service=git-receive-pack&target_ref=refs/heads/agent-42`
  - `Git-Protocol` header: `Git-Protocol: version=2:target-ref=refs/heads/agent-42`

## 2. `packfile-uri` Offloading to Express Regional Caches
During `POST /git/:repo/git-upload-pack`:
- When the client negotiates `packfile-uris`, Mesh plans the minimal covering set of `L4..L0` `PackCover`s in $O(\log \Delta)$ metadata reads and responds with:
  ```text
  0012packfile-uris\n
  0041http://127.0.0.1:7474/express/packs/pack-<sha256>.pack\n
  0001
  000dpackfile\n
  <32-byte empty packfile on sideband channel 1>
  0000
  ```
- Each URI is served by the consistent-hashed **Express** cache layer (`/express/packs/:pack_id`) with `Cache-Control: public, max-age=31536000, immutable`.

## 3. Server-Side CST-Commutative Landing (`refs/mesh/land/<target>`)
Any standard Git client can land a commit onto `<target>` (e.g. `main`) without pulling or rebasing locally:
```bash
git push origin HEAD:refs/mesh/land/main
```
- If `refs/heads/main` has advanced since the pushed commit's parent:
  1. Mesh runs the **Cross-File Semantic Call-Graph Radar** (`detect_semantic_conflicts`).
  2. For any file modified on both `main` and the pushed commit, Mesh runs `cst_merge_3way` using `tree-sitter` Concrete Syntax Tree byte spans.
  3. Orthogonal symbol insertions/modifications and imports are spliced verbatim on the server, preserving file modes (`100644`, `100755`, `120000`) and doc-comments.

## 4. Zero-Checkout Cloud Agent Workspace & Intent Patch REST API
Cloud coding agents can read and patch repositories without cloning a local `.git` directory:
- `POST /api/v1/repos/:repo/workspaces` — Creates a copy-on-write virtual workspace anchored at the current `refs/heads/main` version.
- `GET /api/v1/repos/:repo/workspaces/:ws_id` — Returns the workspace snapshot and file map.
- `POST /api/v1/repos/:repo/patches` — Synthesizes Git `blob`, `tree`, and `commit` objects in memory from a JSON map of file updates + `AgentIntentReceipt`, packs them into an `L0` packfile, and commits or lands them onto `refs/mesh/land/main`.
- `GET /api/v1/repos/:repo/radar` — Returns all active pre-merge `SemanticConflict`s across in-flight branches.
