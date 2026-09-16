# Architecture

`erosion` is one Rust package with a library and a thin command-line binary.
It measures committed Git objects. It has no provider writes, database,
server, plugin loader, AST-Grep dependency, or project-code execution.

## Boundaries

- `main.rs`: CLI argument validation, command orchestration, exit behavior.
- `git.rs`: repository discovery, safe revision resolution, first-parent
  traversal, tree inventory, framed blob reading. Git subprocess access stays
  here and is restricted to read-only plumbing.
- `history.rs`: UTC duration/calendar arithmetic and checkpoint-to-commit
  selection. Repository birth is represented by absent commits, not zero scores.
- `config.rs`: one invocation's frozen include/exclude scope and fingerprint.
- `languages/`: language routing and adapter interface. JavaScript-family and
  Python adapters select bundled grammars and define callable, decision, naming,
  ignored-source, and grammar-validation rules. `languages/tree.rs` owns their shared traversal,
  diagnostics, nested-decision aggregation, and source-line accounting.
- `metrics.rs`: language-independent measurement records, complexity threshold,
  function mass, and compensated summation.
- `cache.rs`: path-independent analysis reuse by blob and grammar, with atomic
  persistent records outside the measured worktree.
- `lib.rs`: per-checkpoint aggregation, coverage accounting, partial-result
  policy, top contributing functions, and versioned report records.
- `output.rs`: table, JSON, and single-table CSV serialization.

## Metric invariants

Language adapters do not choose repository scope or calculate historical
changes. They return the same analysis for a given byte sequence and grammar,
regardless of pathname. Scope decisions must never enter the blob cache.

Function mass is `CC * sqrt(SLOC)`. The erosion numerator includes only
functions with `CC > 10`; the denominator includes all measured functions.
Nested function bodies count in enclosing functions and independently.
Aggregate ratios are calculated from summed function mass, not averages of
per-file percentages. Zero mass is not measurable.

The analyzer walks syntax trees iteratively. SLOC uses LF-aligned file-level
source-line membership intersected with each callable's inclusive line span.
Comment-only, blank, and punctuation-only lines do not contribute. Python also
excludes scope-leading ordinary string expressions as docstrings, but retains
data strings. Language-specific rules are documented in README.md; cross-language
aggregation sums mass rather than averaging percentages.

Parser versions are pinned. The analyzer identity covers measurement source,
language routing, parser versions, metric version, and cache schema. Changing
counting behavior must preserve cache invalidation and update the documented
metric contract/version when semantics change.

The `erosion-v2` profile adds Python to default coverage while preserving JS/TS
rules. Comparisons require the same profile and scope across checkpoints; an old
JS-only report is not directly comparable with a new mixed-language default.

## Repository and temporal invariants

The working tree, index, refs, and source object cache are not mutated.
No checkout, stash, fetch, hooks, filters, or textconv are used to analyze
source. Blobs are read one framed request at a time, not queued into an
unbounded request pipe. Symlinks and submodules are never followed.

Repository discovery starts at `--repo-path`, defaulting to the invocation's
working directory. All Git access targets the discovered root without changing
the process's working directory. Relative repository, explicit configuration,
and explicit cache paths are invocation-relative. Cache containment is checked
against the selected worktree, not the invocation directory.

Configuration is loaded once from the explicit file or selected worktree root,
not from historical commits. The same effective scope applies at every
checkpoint. Inclusion is independent of whether a file is supported by an
adapter; unsupported files remain visible in coverage.

First-parent history is traversed before selecting calendar checkpoints.
Selection uses chain order with a timestamp predicate, not timestamp sorting,
because Git committer times need not be monotonic. Calendar offsets are
anchored to the endpoint to avoid repeated month-end clamping drift. Exact
start/end cutoffs are included and repeated selected SHAs remain visible.

Shallow or unavailable history is not silently substituted with a shorter
series. A fully traversed history may legitimately have no eligible ancestor
for a cutoff; that checkpoint has an explicit `no_commit` status.

`delta FROM TO` resolves both commits before analysis, then reuses the snapshot
report pipeline with exactly two checkpoints in argument order. It does not
traverse history, sort timestamps, select a merge base, or restrict scope to
changed files. Its reference is TO and the second snapshot's `change_pp` is
TO minus FROM. Both sides share the frozen scope and blob cache; an absent
score on either side makes the difference unmeasurable. Parse-failure diagnostics
include commit identity so failures remain distinguishable even at equal timestamps.

## Results and persistence

Each result includes full commit identity/timestamp, requested cutoff,
coverage, total and complex mass, score, and schema/analyzer/scope provenance.
JSON is the full structured contract; CSV encodes nested diagnostics and
coverage as JSON-valued columns. Human output leads with the score or delta and
compact coverage; fingerprints, raw coverage maps and full provenance remain
in JSON/CSV. Table output is presentation, not a stable machine-readable schema.
The CLI emits processing/cache diagnostics on stderr only with `--verbose`;
the analyzer's report loop is quiet. Verbosity never changes serialized results.

Parsing failures are collected, not silently dropped. By default they prevent
all result output. Opted-in partial results retain failures and exclude those
files from all measured mass. Git/configuration/cache/I/O errors are never
converted into successful-looking empty or partial results. Failure diagnostics
are never gated by verbosity; partial and unmeasurable states stay visible in
the human summary.

Persistent cache records are private local analysis artifacts. They store
function names/locations, numeric measurements, and parse diagnostics, not
source bodies. Content-addressed identity and atomic writes permit reuse
between invocations without a mutable repository database. Cache entries are
reconstructible and not guaranteed durable across power loss: writes do not
force a device flush for each blob. Malformed cache records require explicit
recovery; `--no-cache` is an escape hatch.

There are no migrations, durable event streams, or changes to existing
repository data. Report schema and cache schema are versioned separately.
Delta uses report schema 1 with the additional `mode` value `delta`; its ordered
snapshots and CSV rows reuse existing fields. Metric identity and cache schema
are unchanged.
