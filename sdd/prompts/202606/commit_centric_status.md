---
plan: sdd/tales/202606/commit_centric_status.md
---

#fork:actstat-1 The `actstat list` command should only show output for repos that actually have GitHub Actions runs. Also we should be showing the most recent commit that has a completely finished GitHub Actions workflow run but not a single job if that workflow run failed. If multiple jobs failed, we need to make sure to show all jobs. Also, just to make sure we are on the same page, a workflow that didn't run because some of its jobs failed (so it aborted quickly) should be considered a failed workflow (we should show the jobs that failed). Can you help me fix these issues? Think this through thoroughly and create a plan using your `/sase_plan` skill. Submit your plan with the
`sase plan propose` command (as the skill instructs) before making any file changes.
 

%xprompts_enabled:false
### Questions and Answers

#### Q1: Branch scope

> When choosing 'the most recent commit' per repo, what scope should actstat consider? In your live config, sase-org/sase's newest commit is on an automation branch (release-please--branches--master) running only a 'PR Title' check, while the real master commits (some with CI failures) are slightly older. Scope determines whether you see real CI status or automation noise.

- [x] **Default branch only** — Only commits on the repo's default branch (e.g. master/main). Hides release-please/PR automation noise. RECOMMENDED.
- [ ] **push events only** — Any branch, but only push-triggered runs (drops pull_request automation like 'PR Title').
- [ ] **Any branch, any event** — Most literal reading of 'most recent commit' — but will surface automation-branch commits.

#### Q2: Commit aggregation

> A single commit usually triggers several workflow runs (e.g. sase-org/sase commit 985af86 ran CI=failure AND Deploy Docs=success). Should actstat treat the commit as ONE unit?

- [x] **Aggregate: fail if any run failed** — Commit is RED if any of its workflow runs failed; show every failed run's failed jobs. Commit is GREEN only if all runs passed. RECOMMENDED.
- [ ] **Single CI run only** — Pick one workflow run per commit (the most recent) and ignore the rest — closer to today's behavior.

#### Q3: -n meaning

> What should the -n/--limit flag count after this change? Today it means 'N most recent completed workflow runs per repo'.

- [x] **N most recent commits** — Each shown as one commit summary (green one-liner, or red with failed jobs). RECOMMENDED — matches the new commit-centric model.
- [ ] **Keep N = workflow runs** — Leave -n meaning individual runs; only change selection/grouping logic.

#### Q4: Empty-repo suppression

> For Issue 1 ('only show repos that actually have runs'): which repos should be omitted, and in which formats?

- [x] **Omit empty everywhere; keep errors** — Drop repos with no qualifying runs from human, JSON, and JSONL. Still show repos that errored (403/404/etc.). RECOMMENDED.
- [ ] **Omit empty in human only** — Suppress the 'no completed runs' line in human output, but keep empty repos in JSON/JSONL for machine consumers.
- [ ] **Omit empty AND error repos** — Show only repos with real runs; hide both 'no runs' and error rows entirely.

%xprompts_enabled:true