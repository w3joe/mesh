# Mesh Formal Safety & Consistency Invariants

This document specifies the five core invariants enforced by `mesh-meta`, `mesh-pack`, and `mesh-semantic`.

## 1. Per-Ref Linearizability (`I1_PER_REF_CAS`)
For any repository $R$ and reference $r \in \text{Refs}(R)$, let $V(R, r) = \langle v_1, v_2, \dots, v_k \rangle$ be the sequence of committed `RefVersion` records.
- Every state transition $v_{i} \to v_{i+1}$ must satisfy:
  $$\text{old\_commit\_oid}(v_{i+1}) = \text{new\_commit\_oid}(v_i)$$
- If two concurrent writers attempt to transition $r$ from the same `expected_old_oid`, at most one succeeds and all others fail with `MetaError::CasConflict`.

## 2. Wait-Free Cross-Ref Independence (`I2_CROSS_REF_WAIT_FREE`)
For any two distinct references $r_1 \neq r_2$ in repository $R$:
- The lock acquisition set of `commit_push(R, r_1)` is disjoint from `commit_push(R, r_2)`.
- A push, compaction, or deletion on $r_1$ never blocks or causes a CAS retry on $r_2$.

## 3. Read-Your-Own-Writes Causal Completeness (`I3_CAUSAL_COMPLETENESS`)
When a push to topic reference $r$ at version $v$ succeeds against advertised trunk version $T_{\text{base}}$:
- The uploaded `L0` packfile $P(r, v)$ plus the union of trunk covers covering $(0, T_{\text{base}}]$ contains every Git object (`commit`, `tree`, `blob`, `tag`) reachable from $\text{new\_commit\_oid}(r, v)$.

## 4. LSM Skip-List Cover Equivalence (`I4_COVER_EQUIVALENCE`)
For any contiguous interval of versions $[a, b]$ on reference $r$:
- If a level-$L$ cover $C_{L}(a, b)$ is registered via `compact_branch_level`, the set of decoded Git objects in $C_{L}(a, b)$ equals the set union of decoded objects across all level-0 packs $\bigcup_{i=a}^{b} P_{0}(i)$:
  $$\text{Objects}(C_L(a, b)) = \bigcup_{i=a}^{b} \text{Objects}(P_0(i))$$
- Lower-level covers $C_{0..L-1}$ remain immutable and readable after $C_L(a, b)$ is published.

## 5. CST Commutative Merge Soundness (`I5_CST_COMMUTATIVITY`)
Given base source $S_0$ and two branch modifications $S_A, S_B$ of file $F$:
- `cst_merge_3way(F, S_0, S_A, S_B)` returns `CstMergeOutcome::Clean` if and only if:
  1. $S_0, S_A, S_B$ parse into `tree-sitter` Concrete Syntax Trees with zero `ERROR` or `MISSING` nodes (`!root.has_error()`).
  2. For every non-import top-level declaration key $k = (\text{SymbolKind}, \text{Name})$, if both $S_A$ and $S_B$ mutate $k$ relative to $S_0$, their resulting declaration byte hashes are identical ($\text{hash}(S_A[k]) = \text{hash}(S_B[k])$).
  3. Neither branch mutates the signature of a symbol $s$ that is invoked by a newly inserted or modified symbol in the other branch (`SignatureCallMismatch`).
