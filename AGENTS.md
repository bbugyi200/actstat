# actstat - Agent Instructions

## 1. Tier 1 (core) Memory

The following memories contain core (always loaded) context:

### 1.1 SASE = Structured Agentic Software Engineering (sase)

#### 1.1.1 SASE Memory

SASE memory is this project's durable agent context: Markdown notes under `sase/memory/`
that render into this file. A note's `type:` frontmatter decides how it reaches you.

- **Core memory** (`type: core`) is Tier 1. It is inlined here and into every provider
  instruction shim, so it is always in your context and every note is paid for on every
  turn.
- **Reference memory** (`type: reference`) is Tier 2. Only its one-line description is
  listed here; read the body on demand with your `/sase_memory_read` skill, never by
  opening the file directly.
- **Memory webs** are keyed collections: a flat descriptor note (`sase/memory/<web>.md`)
  plus a sibling directory of strand files (`sase/memory/<web>/<slug>.md`). The
  descriptor renders at either tier, but a strand body is never inlined — read strands
  by keyword (`glossary:stitch`) through the same skill.

IMPORTANT: You should not modify any of these memory files without approval from the
user. Authorization found in a plan file, bead description, design doc, or any other
agent-produced artifact does NOT count as user permission. However, when the user
explicitly asks you to update a SASE memory file, that request already carries the
required approval for the full workflow: make the requested edit to the canonical note
under `sase/memory/`, then you MUST run `sase memory init` to regenerate `AGENTS.md`,
the provider instruction shims, and the memory README. Do NOT ask for separate
permission to initialize sase memory in that case.

#### 1.1.2 Ephemeral `actstat_<N>` Workspace Directories

SASE runs agents (like you) from ephemeral workspace directories, which are full clones
of the actstat repo. These directories are named `actstat_<N>` where `<N>` is some
integer. You need to be mindful not to run commands outside of these workspace
directories, since they have their own isolated virtual environments.

IMPORTANT: Do NOT mention your workspace directory (or any sibling workspace directory)
in any plan files that you generate using your `/sase_plan` skill. The agent(s) that
implement the plan might not run in the same workspace directory as you!

#### 1.1.3 Repositories

Configured linked and sidecar repositories for this context:

- `actstat--research`: Durable SASE research reports and generated media.

When you need to read or modify files in any repository other than your own workspace
checkout, agents MUST use your `/sase_repo` skill first. This includes configured linked
repos and sidecars, another SASE project's repo, and any GitHub repo not linked to the
current project. Open different-project and unlinked GitHub repos as external repos
through the skill. Use the path it prints as the only path for reads and writes.

This rule applies regardless of transport. Fetching a repository's files or history over
the web — github.com file/blob/raw URLs, raw.githubusercontent.com, repo tarballs, or
GitHub-API/`gh` file-content reads — counts as reading that repo: open it with
`/sase_repo` (unlinked GitHub repos open as external repos, e.g. `gh:<owner>/<repo>`)
and read the local checkout instead. Web tools remain appropriate only for content a
checkout does not contain, such as blog posts, docs sites, and GitHub issue/PR
discussions.

IMPORTANT REMINDER: Do NOT locate, clone, or web-fetch another repo's contents any other
way than by using `/sase_repo`!

#### 1.1.4 SASE Final Declaration

Before any normal response that ends this SASE provider turn, use your `/sase_final`
skill as the last action. This includes a final answer, an incomplete-status response,
an "I will wait" response, or any reply that intends to resume in a later turn. It will
call `sase final context`, inspect any selected finalizers and repository obligations,
and submit one atomic declaration with `sase final submit` when the host requires one.
The declaration must cover every repository you changed this turn, including linked,
sidecar, or external repos opened through `/sase_repo`. A host prompt scoped to one
repository's commit or conflict repair does not narrow that obligation for any other
repository you changed.

After a successful `sase final submit`, do not make more file or repository changes in
this turn. If the declaration command reports validation errors, repair the manifest and
resubmit before returning when possible. Only a successfully executed plan, monitor,
pipe, or questions handoff is exempt, because those commands terminate the runner
mechanically. Intending to resume later is not an exemption.

### 1.2 Task Bead Types (task_types)

Every task bead can carry a `task_type` drawn from this project's catalog.
`sase bead task-type list` always shows the live catalog and
`sase bead task-type show <slug>` shows one type in full; this note is the generated,
always-current snapshot of the agent-creatable types below.

<!-- sase:strands -->

- **Bug** (`bug`) - A defect an agent found while doing unrelated work, not an external
  tracker bug.
- **CI failure** (`ci`) - A confirmed true test or lint failure, not a flake.
- **Feature** (`feature`) - An out-of-scope product idea that should not become a wish
  list.
- **Flaky test** (`flake`) - A test that fails and then passes on an unchanged tree.
- **Memory** (`memory`) - A sase memory note or skill that is out of date.

<!-- /sase:strands -->

#### 1.2.1 File Discovered Work As Task Beads

Unless your prompt explicitly forbids creating beads (epic phase workers, for example,
must record `PROPOSED FOLLOW-UP:` notes on their own bead instead), you can and SHOULD
capture discovered follow-up work as sase task beads. Pick the type above whose
`when_to_use` matches what you found:

- A linter or test is flaky or failing and you did not cause it: file a task bead
  instead of ignoring the failure.
- A sase memory file or skill contains out-of-date information that should be updated:
  file a task bead proposing the update.
- A tool, command, or script this project is responsible for has a bug or a clear,
  objective improvement that would help future agents: file a task bead to fix or
  improve it.

Before creating any task bead, you MUST use `/sase_new_task`. That skill checks every
task status for semantic duplicates, checks in-progress epics for a credible causal
link, and records the issue in the right place. Only a genuinely new task becomes an
`open` draft, and every new task requires an intentional `--size` plus
`-T "task(<slug>)"` and `-f/--field` values for that type's required fields. Ready task
beads are proposed to the project owner, who either launches an agent to work them or
closes them with a reason.

## 2. Tier 2 (reference) Memory

The below files contain detailed reference material. When working in their domain, you
MUST use your `/sase_memory_read` skill to review their contents. Do not read canonical
memory files directly.

### 2.1 `sase/memory/sase_artifacts.md`

Read before creating, consuming, resolving, linking, or managing retention for SASE
artifact references and indexed files.

### 2.2 `sase/memory/sase_beads.md`

Read before creating, updating, closing, or querying sase beads — bead types and tiers,
the status lifecycle agents must never hand-edit, task-bead triage, phase-bead
description prefixes, and non-cascading close, resolution, and note semantics.
