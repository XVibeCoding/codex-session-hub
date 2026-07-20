# Performance Optimization Checklist

Branch: `codex/perf-light-refresh`
Base: `main` @ `812af8b` (checklist intro commit: `3e67a0b`)
Created: 2026-07-20
Last revised: 2026-07-20

Use this list for implementation progress and review.
Check items only after the corresponding change is done and verified.

Legend:

- `[ ]` not started
- `[x]` done
- note `(partial: ...)` under an item when only part landed

---

## Goals and non-goals

### Goals

- Make the **common path** cheap: app launch scan, re-scan, session list render.
- Keep the **rare path** correct: repair apply, backup, restore, pending-op recovery.
- Preserve user-visible repair semantics (same sessions recover, same skips, same token staleness rules).

### Non-goals (this branch)

- Do not weaken operation lock / write fence / repair journal / plan-token optimistic locking.
- Do not split `core.rs` purely for readability (separate refactor if needed).
- Do not redesign non-Windows platform behavior beyond keeping stubs compiling.

### Definition of done for merge

Minimum mergeable scope: **Phase 1 complete + Phase 1 verification checked**.
Phases 2-4 may ship in the same PR if finished; otherwise leave them open and list them under Review summary as deferred.

---

## Dependency map (read before coding)

```text
1 Graded SQLite read
        |
        v
3 Shared snapshot derivation  ---- optional parallel with 1 after API is clear
        |
        v
2 Slim refresh_desktop  <---- needs 3 for cheap scan; needs explicit API for optional preview
        |
        v
4 Phase 1 verify
        |
        +--> 5 Field-trimmed scan (builds on graded read modes)
        +--> 6 O(n) analytics (independent, safe anytime after 3)
        +--> 7 Rollout structure (independent of 2; careful with 5 field needs)
        |
        v
8 Phase 2 verify
        |
        +--> 9 Slim plan_token (after refresh no longer depends on heavy token)
        +--> 10 Backup fast path
        |
        v
11 Phase 3 verify --> 12/13/14 frontend --> 15 build
```

Rules:

- Do **not** start task 2 until you know how `DesktopRefreshResult.preview` becomes optional/empty without breaking TS types.
- Do **not** use light SQLite open inside repair commit / restore validation.
- Task 9 must keep: preview token changes when planned ops change; apply rejects mismatched token.

---

## Code facts to respect (as of checklist creation)

These are current-code observations; update if code changes.

1. `DesktopRefreshResult` **requires** `preview: ProjectionPreviewResult` today (`core.rs`). Slimming refresh is an **API shape change**, not only an internal skip.
2. Frontend recovery UI already uses **`preview_projection` + `activePreview`**, not `desktop.preview`, for the recovery dialog. Refresh still pays for preview on the backend and over IPC.
3. `refresh_desktop_at` currently: reconcile pending -> optional backup maintain -> `scan_snapshot` + blocking processes -> `scan_result_for_snapshot` -> **full** `projection_preview_for_snapshot` -> `local_session_summaries` (cohorts/eligible work repeated).
4. `open_readonly` always full-copies DB (+ wal/journal) to temp and callers often run `PRAGMA quick_check`.
5. `plan_token` hashes full store + full preview JSON + all op arrays.
6. Repair correctness depends on plan recompute under lock + token match; optimisations must not skip that on apply.

---

## Phase 1 - P0: Refresh hot path (highest impact)

Primary goal: keep repair safety, make list refresh cheap.

### 0. Phase 1 design spike (do first, small)

- [x] Write a short note in Progress log: chosen light-open strategy (URI `mode=ro` vs flags), busy retry, and fallback-to-copy policy
- [x] Decide `DesktopRefreshResult.preview` contract: `Option<...>` vs placeholder empty preview vs new command
- [x] List call sites that must stay on **safe** open: repair schema validate, backup integrity, restore, sqlite backup source if required
- [x] Confirm CLI `scan` / `repair` / desktop paths each pick light vs safe intentionally

### 1. Graded SQLite read path

- [x] Introduce explicit read modes, e.g. `SqliteReadMode::Light | Safe` (names flexible)
- [x] Light mode: no full DB file copy; `READ_ONLY` / `mode=ro`; set `query_only` when applicable
- [x] Light mode: skip `PRAGMA quick_check`
- [x] Light mode: define `SQLITE_BUSY` / lock retry (count + sleep) and error mapping to existing UI-friendly busy text where possible
- [x] Safe mode: keep snapshot-copy + fingerprint retry + `quick_check` for repair/backup/restore validation
- [x] Lightweight helpers (e.g. `sqlite_user_version`) use light open
- [x] List/scan path (`read_threads` / `read_catalog` used by refresh) uses light open
- [x] Repair preflight / backup verify / restore verify stay on safe open
- [x] Temp snapshot dirs from safe open still cleaned; light open must not leak new temp junk
- [ ] Main files: `src-tauri/src/core.rs`
- [x] Automated tests: light open can read a fixture DB; safe open still used on at least one validation path; busy/locked behaviour does not panic

**Risk note:** Light open can observe a concurrent writer mid-update. Acceptable for list UX if refresh is retryable; **not** acceptable as the sole basis for apply without re-scan under lock (apply already re-scans - keep that).

### 2. Slim `refresh_desktop`

- [x] Opening UI / re-scan does **not** build full preview + `plan_token` by default
- [x] IPC / types updated: `DesktopRefresh` / `DesktopRefreshResult` preview optional or omitted; `src/app-types.ts` + `App.tsx` compile cleanly
- [x] Full preview runs on recovery open / recover action via existing `preview_projection` (already partially true in UI)
- [x] Frontend still renders session list, counts, provider footer, blockers without requiring refresh-time preview
- [x] `initialize: true` backup maintenance remains acceptable cost on first launch only (or document if deferred)
- [x] `reconcile_pending_repair_on_startup` still runs on refresh/startup
- [x] Main files: `src-tauri/src/core.rs`, `src-tauri/src/lib.rs`, `src/app-types.ts`, `src/App.tsx`
- [x] Automated tests or compile-checked fixtures for refresh result without preview

### 3. Reuse derived results within one snapshot

- [x] Shared cohorts on refresh path (`session_cohorts` once for scan + list; `*_with_cohorts` helpers)
- [x] `session_cohorts` computed once per refresh/scan pipeline
- [x] Scan path reuses cohorts for eligible provider counts; refresh no longer dual-builds cohorts for list
- [x] `local_session_summaries` reuses cohorts (no second full cohort rebuild)
- [x] `scan_result_for_snapshot` accepts precomputed cohorts/eligible instead of rebuilding blindly
- [ ] When preview **is** requested, it reuses the same shared context rather than rescanning disk (helpers ready; standalone preview still scans once)
- [x] Main files: `src-tauri/src/core.rs`
- [x] Automated tests: scan counts and local session statuses unchanged on fixtures; refresh returns `preview: None`

### 4. Phase 1 verification

- [x] `cargo test --manifest-path src-tauri/Cargo.toml` (lib: 102 passed)
- [ ] Manual: app launch scan works
- [ ] Manual: re-scan works
- [ ] Manual: recovery preview still works (dialog numbers sensible)
- [ ] Manual: recovery apply still works
- [ ] Manual: rollback still works
- [ ] Manual: with Codex running and DB busy, UI shows controlled error/retry (no crash)
- [ ] Optional timing note in Progress log (before/after re-scan on same machine)
- [ ] Update this checklist checkboxes + Progress log
- [ ] Notes:

**Phase 1 acceptance bar**

- Functional: list and repair behaviour match pre-change fixtures/tests.
- Performance: re-scan no longer copies both SQLite DBs + double `quick_check` on the list path (verify by code review and/or debug timing).
- Safety: apply path still re-plans under operation lock and still requires matching plan token.

---

## Phase 2 - P1: Scan cost and algorithms

### 5. Field-trimmed list scan

- [ ] Light scan omits repair-only heavy fields (at least `first_user_message` when not needed for list status) — **deferred**: shared `scan_snapshot` feeds list + plan/verify; field still required for visibility_mismatch post-write checks
- [ ] Repair / preview-for-apply path still loads fields required for `state_insert_from_rollout` and plan building
- [ ] Avoid dual-source inconsistency: same thread id should not show contradictory status between light list and repair scan beyond known timing races
- [ ] Main files: `src-tauri/src/core.rs`
- [ ] Tests: list path works without first_user_message; repair insert path still has metadata when needed

### 6. Fix expensive scan analytics

- [x] `provider_drift` nested `local_rows.iter().find` replaced with `HashMap` / index by `thread_id` (`O(n)`)
- [ ] Remove redundant `HashSet` rebuilds in `scan_result_for_snapshot` where safe and readable
- [ ] Prefer implementing **after** shared context (task 3) so analytics run once on shared inputs
- [ ] Main files: `src-tauri/src/core.rs`
- [ ] Tests: drift/orphan/recoverable counts stable on fixtures

### 7. Rollout scan structure / IO

- [x] Avoid unnecessary `PrimaryRollout` clones in `read_rollouts` (move into `primary_rollouts`)
- [ ] Reduce duplicated parallel maps where practical without hurting call sites
- [ ] Optional: bounded parallel jsonl primary parsing (document pool limit; preserve deterministic issue ordering in tests)
- [ ] Do not expand enrichment caps without reason (`MAX_ENRICHMENT_*` already bounded)
- [ ] Main files: `src-tauri/src/core.rs`, `src-tauri/src/rollout.rs`
- [ ] Tests: rollout inventory fixtures still pass; duplicate id issues still reported

### 8. Phase 2 verification

- [ ] `cargo test --manifest-path src-tauri/Cargo.toml`
- [ ] Manual: large local history still lists correctly
- [ ] Manual: recovery plan unchanged for a few sample sessions (spot check ids/counts)
- [ ] Notes:

---

## Phase 3 - P1/P2: Plan token and backup path

### 9. Slim `plan_token`

- [ ] Stop hashing full `ProjectionStore` object + full preview JSON blob
- [ ] Token input = canonical ordered ops + store version/fingerprint + schema version + critical counters/conflicts
- [ ] Apply still rejects stale preview tokens when sessions/providers change after preview
- [ ] Dry-run CLI still returns a token usable with `--apply --plan-token`
- [ ] Schema/version bump field in token payload if format changes (avoid silent cross-build mismatch only if needed)
- [ ] Main files: `src-tauri/src/core.rs`
- [ ] Tests: identical plans => identical token; mutating one op => different token; apply mismatch error preserved

### 10. Backup maintenance lighter path

- [ ] Backup list / automatic maintenance uses fast path (manifest + mtime/size) when safe
- [ ] Full hash / `quick_check` reserved for restore-before and write-time backup validation
- [ ] Incomplete/legacy/corrupt classification must not become more aggressive without user confirmation paths already in product
- [ ] First-launch `maintain_backups_at` should not dominate startup after change (or gate expensive verify)
- [ ] Main files: `src-tauri/src/core.rs`
- [ ] Tests: list/cleanup retention limits still enforced; restore still verifies integrity

### 11. Phase 3 verification

- [ ] `cargo test --manifest-path src-tauri/Cargo.toml`
- [ ] Manual: preview -> change data -> apply token mismatch still blocked
- [ ] Manual: backup list / cleanup / restore still healthy
- [ ] Notes:

---

## Phase 4 - P3: Frontend / IPC

### 12. Session list virtualization (or equivalent DOM limit)

- [ ] Avoid mounting thousands of session rows at once
- [ ] Search / `forceOpen` behaviour does not explode DOM
- [ ] Keyboard/focus and checkbox selection still work for visible rows
- [ ] Main files: `src/components/SessionExplorer.tsx`, `src/App.tsx`, `src/styles.css` (if needed)

### 13. Memoize list row components

- [ ] `SessionRow` memoized
- [ ] Group header memoized where beneficial
- [ ] Selection toggles do not rebuild unchanged row elements unnecessarily
- [ ] Main files: `src/components/SessionExplorer.tsx`

### 14. Fewer full refreshes during recovery

Inventory current refreshes first, then cut safe ones:

- [ ] Document current `refreshDesktop` call sites during recovery in Progress log
- [ ] Intermediate stages avoid full desktop refresh when progress channel is enough
- [ ] Final success/error path still refreshes authoritative state
- [ ] Close-processes retry path still ends with a trustworthy scan
- [ ] Main files: `src/App.tsx`

### 15. Frontend build check

- [ ] `npm run build`
- [ ] Optional: `npm run tauri -- build` only if packaging risk; not required for every intermediate commit
- [ ] Notes:

---

## Cross-cutting work items (easy to forget)

- [ ] Keep Chinese UI strings behaviour unchanged unless intentionally editing copy
- [ ] Any new public serde fields use `camelCase` and stay backward-tolerant if old UI might connect
- [ ] Update README only if user-facing behaviour/perf expectations change materially
- [ ] Each phase commit message references checklist section ids (e.g. `perf(phase1): graded sqlite read`)
- [ ] After each phase, tick boxes in this file in the **same PR/branch** so review sees truth

---

## Out of scope for this branch

Do **not** treat these as unfinished optimization work:

- [x] Weaken repair lock / journal / plan-token optimistic locking for speed - **won't do**
- [x] Pure `core.rs` file split without runtime gain - **separate task if needed**
- [x] Non-Windows platform rewrite - **won't do in this branch**
- [x] Changing Codex itself or provider switching - **won't do**
- [x] Uploading sessions or adding telemetry services - **won't do**

---

## Suggested commit slicing

1. `docs: checklist` (done)
2. `perf(phase1): graded sqlite read modes`
3. `perf(phase1): shared scan context + slim refresh_desktop`
4. `perf(phase2): scan analytics + rollout clone cleanup`
5. `perf(phase3): slim plan_token + backup fast path`
6. `perf(phase4): session list render costs`
7. `docs: mark checklist complete / review summary`

Smaller slices are fine; avoid mixing safe-open changes with frontend virtualization in one commit.

---


### Phase 1 design notes (landed)

- **Light open**: `OpenFlags::SQLITE_OPEN_READ_ONLY | NO_MUTEX` on live path; `pragma query_only=true`; 8x busy retry / 25ms sleep; no file copy; no `quick_check`.
- **Safe open**: temp copy of db + wal/journal with fingerprint retry; `quick_check` on validation paths (repair schema, backup integrity, restore).
- **Refresh preview contract**: `DesktopRefreshResult.preview = None` on list refresh; UI recovery uses `preview_projection` + `activePreview`.
- **Shared derivation**: `refresh_desktop_at` builds `session_cohorts` once and reuses via `scan_result_for_snapshot_with_cohorts` + `local_session_summaries_with_cohorts`. Preview path has `*_with_cohorts` helpers for later reuse; refresh no longer calls preview.

## Progress log

| Date | Phase | What landed | Verified by |
|------|-------|-------------|-------------|
| 2026-07-20 | - | Checklist created on branch `codex/perf-light-refresh` | - |
| 2026-07-20 | docs | Checklist revised: dependencies, code facts, risks, DoD, tests, and commit slicing | - |
| 2026-07-20 | 1 | P0: slim refresh (no preview/plan_token); shared cohorts; light URI open (immutable only without -wal) | cargo test --lib 102 ok; npm run build ok |

---

## Review summary (fill before merge)

- Completed phases:
- Intentionally deferred:
- Residual risks:
- Suggested follow-ups:
- Safe vs light open call-site audit done? (yes/no):
- Plan-token compatibility notes:
