---
create_time: 2026-06-29 08:48:50
status: wip
prompt: sdd/prompts/202606/actstat_1_remaining.md
---
# Remaining Work Plan: actstat-1

## Context

The actstat initialization epic is mostly complete: all child phase beads are closed, the related actstat commits are
present, and the managed chezmoi config exists with the planned sources. The remaining gap found during verification is
that workflow run duration was specified in the epic plan but is not currently represented in the normalized model or
computed from GitHub payloads.

## Plan

1. Add run duration to the normalized model.
   - Represent duration as a machine-readable `duration_seconds` field on `RunReport`.
   - Preserve backwards-friendly behavior by omitting the field when GitHub timestamps are missing or malformed.

2. Compute duration during GitHub normalization.
   - Parse `run_started_at` from the workflow-run payload.
   - Fall back to `created_at` when `run_started_at` is absent.
   - Compute a non-negative duration from the selected start timestamp to `updated_at`.
   - Cover normal, fallback, missing, malformed, and negative timestamp cases in unit tests.

3. Surface duration in human and machine output.
   - JSON and JSONL should carry `duration_seconds` from the shared model.
   - Human output should include a compact duration token alongside branch, run number, and relative completion time.
   - Update renderer snapshots and README examples so documented output matches real behavior.

4. Verify the epic.
   - Run the available project verification gate.
   - Run an actstat smoke command against the configured source file if credentials and network allow it.

5. Close bead `actstat-1`.
