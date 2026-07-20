# Performance Optimization Checklist

Branch: `codex/perf-light-refresh`  
Base: `main` @ `812af8b`  
Created: 2026-07-20

Use this list for implementation progress and review.  
Check items only after the corresponding change is done and verified.

Legend:

- `[ ]` not started
- `[x]` done
- `(partial)` if needed, note under the item

---

## Phase 1 — P0: Refresh hot path (highest impact)

Primary goal: keep repair safety, make list refresh cheap.

### 1. Graded SQLite read path

- [ ] List / light preview path uses `READ_ONLY` (or `mode=ro`) + `query_only`, without full DB copy
- [ ] List / light preview path skips `PRAGMA quick_check`
- [ ] Repair / backup / restore validation keeps snapshot-copy + `quick_check`
- [ ] Lightweight helpers (e.g. `sqlite_user_version`) no longer pay full-copy cost
- [ ] Main files: `src-tauri/src/core.rs`

### 2. Slim `refresh_desktop`

- [ ] Opening UI / re-scan does **not** build full preview + `plan_token` by default
- [ ] Full preview runs on recovery open / recover action via `preview_projection`
- [ ] Frontend still receives enough data to render session list and basic status
- [ ] Main files: `src-tauri/src/core.rs`, `src-tauri/src/lib.rs`, `src/App.tsx` (if needed)

### 3. Reuse derived results within one snapshot

- [ ] `session_cohorts` computed once per refresh/scan pipeline
- [ ] Eligible/projection session derivation computed once where shared
- [ ] `local_session_summaries` reuses shared intermediate results
- [ ] `scan` / list / optional preview derived from shared intermediate structure
- [ ] Main files: `src-tauri/src/core.rs`

### 4. Phase 1 verification

- [ ] `cargo test --manifest-path src-tauri/Cargo.toml`
- [ ] Manual: app launch scan works
- [ ] Manual: re-scan works and feels faster / not regressing
- [ ] Manual: recovery preview still works
- [ ] Manual: recovery apply still works
- [ ] Manual: rollback still works
- [ ] Notes:

---

## Phase 2 — P1: Scan cost and algorithms

### 5. Field-trimmed list scan

- [ ] Light scan omits repair-only heavy fields (e.g. `first_user_message`)
- [ ] Repair scan keeps full fields needed for plan/apply
- [ ] Main files: `src-tauri/src/core.rs`

### 6. Fix expensive scan analytics

- [ ] `provider_drift` nested scan replaced with `HashMap` lookup (`O(n)`)
- [ ] Remove redundant `HashSet` rebuilds in `scan_result_for_snapshot` where safe
- [ ] Main files: `src-tauri/src/core.rs`

### 7. Rollout scan structure / IO

- [ ] Avoid unnecessary `PrimaryRollout` clones in `read_rollouts`
- [ ] Reduce duplicated parallel maps where practical
- [ ] Optional: bounded parallel jsonl primary parsing
- [ ] Main files: `src-tauri/src/core.rs`, `src-tauri/src/rollout.rs`

### 8. Phase 2 verification

- [ ] `cargo test --manifest-path src-tauri/Cargo.toml`
- [ ] Manual: large local history still lists correctly
- [ ] Manual: recovery plan unchanged for sample sessions
- [ ] Notes:

---

## Phase 3 — P1/P2: Plan token and backup path

### 9. Slim `plan_token`

- [ ] Stop hashing full `ProjectionStore` + full preview JSON blob
- [ ] Token based on canonical ops + store version/fingerprint
- [ ] Apply still rejects stale preview tokens
- [ ] Main files: `src-tauri/src/core.rs`

### 10. Backup maintenance lighter path

- [ ] Backup list / automatic maintenance uses fast path (manifest + mtime/size) when safe
- [ ] Full hash / `quick_check` reserved for restore-before and write-time backup validation
- [ ] Main files: `src-tauri/src/core.rs`

### 11. Phase 3 verification

- [ ] `cargo test --manifest-path src-tauri/Cargo.toml`
- [ ] Manual: preview → apply token mismatch still blocked after data change
- [ ] Manual: backup list / cleanup / restore still healthy
- [ ] Notes:

---

## Phase 4 — P3: Frontend / IPC

### 12. Session list virtualization (or equivalent DOM limit)

- [ ] Avoid rendering thousands of session rows at once
- [ ] Search/open-all behavior does not explode DOM
- [ ] Main files: `src/components/SessionExplorer.tsx`, `src/App.tsx`, `src/styles.css` (if needed)

### 13. Memoize list row components

- [ ] `SessionRow` memoized
- [ ] Group header memoized where beneficial
- [ ] Selection toggles no longer re-render whole tree unnecessarily
- [ ] Main files: `src/components/SessionExplorer.tsx`

### 14. Fewer full refreshes during recovery

- [ ] Intermediate recovery stages avoid full desktop refresh when progress events are enough
- [ ] Final success/error path still refreshes authoritative state
- [ ] Main files: `src/App.tsx`

### 15. Frontend build check

- [ ] `npm run build`
- [ ] Notes:

---

## Out of scope for this branch

Do **not** treat these as unfinished optimization work:

- [x] Weaken repair lock / journal / plan-token optimistic locking for speed — **won't do**
- [x] Pure `core.rs` file split without runtime gain — **separate task if needed**
- [x] Non-Windows platform rewrite — **won't do in this branch**

---

## Progress log

| Date | Phase | What landed | Verified by |
|------|-------|-------------|-------------|
| 2026-07-20 | — | Checklist created on branch `codex/perf-light-refresh` | — |

---

## Review summary (fill before merge)

- Completed phases:
- Intentionally deferred:
- Residual risks:
- Suggested follow-ups:
