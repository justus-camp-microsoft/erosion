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
- `config.rs`: frozen include/exclude scope and selection fingerprints.
- `languages/`: language routing and object-safe adapter interface. JavaScript-family,
  Python, Rust, and Gleam modules select bundled grammars and define callable,
  decision, naming, ignored-source, and grammar-validation rules. Each language
  also supplies a separate detector owning its test-file paths, syntax
  classification, initialization, and versioned policy.
  `languages/tree.rs` owns shared glob mechanics, traversal, diagnostics,
  test-region masking, nested-decision aggregation, and source-line accounting;
  it contains no language or framework test heuristics. Its private
  `TreeSitterAdapter<Rules, Policy>` uses either `AllCode` or
  `ExcludeTests<Rules::Tests>` behind `Box<dyn LanguageAdapter>`. The same
  parsing/measurement function serves both policies; only the exclusion policy
  constructs or invokes a detector. Generic policy parameters do not propagate
  through the analyzer, commands, or reports.
  The central adapter factory routes every `Language` variant explicitly.
  Family adapters accept only their supported languages and reject all others
  generically, so new languages do not require edits to unrelated adapters.
- `metrics.rs`: language-independent measurement records, complexity threshold,
  function mass, and compensated summation.
- `cache.rs`: path-independent analysis reuse by blob and grammar, with atomic
  persistent records outside the measured worktree.
- `lib.rs`: per-checkpoint aggregation, coverage accounting, partial-result
  policy, top contributing functions, and versioned report records.
- `modules.rs`: historical module discovery, module-path selection, and
  whole-module series built with the shared snapshot accumulator.
- `output.rs`: table, JSON, and single-table CSV serialization.

## Metric invariants

Language adapters do not choose repository/module selection or calculate
historical changes. `LanguageRules` requires an associated `TestDetector` with
explicit construction, policy, path-classification, and syntax-region methods;
none has a default implementation. `SyntaxRules` contains metric rules only.
Raw adapters hold no detector: they do not initialize test-path globs, obtain
test-policy metadata, or classify paths or syntax. An unfinished detector cannot
affect raw measurement. Exclusion is an explicit application policy, not a
language capability inferred from missing methods or empty results.

With exclusion enabled, `is_test_file` classifies supported source paths
separately from source analysis. Raw metrics remain the same for a given byte
sequence and grammar. Syntax-filtered views are computed only on request and
remain independent of pathname. Path decisions must never enter the blob cache.

Function mass is `CC * sqrt(SLOC)`. The erosion numerator includes only
functions with `CC > 10`; the denominator includes all measured functions.
Nested function bodies count in enclosing functions and independently.
Aggregate ratios are calculated from summed function mass, not averages of
per-file percentages. Zero mass is not measurable.

The analyzer walks syntax trees iteratively. SLOC uses LF-aligned file-level
source-line membership intersected with each callable's inclusive line span.
Comment-only, blank, and punctuation-only lines do not contribute. Python also
excludes scope-leading ordinary string expressions as docstrings, but retains
data strings. Cross-language aggregation sums mass rather than averaging percentages.

Rust includes functions and closures; decisions include loops, `if`, non-wildcard
`match` arms, `let ... else`, and short-circuit boolean operators. Gleam includes
functions and anonymous functions; decisions include non-catch-all `case` clauses,
guarded clauses, assertions, and short-circuit boolean operators. Comments do not
count as source lines.

Parser versions are pinned. The analyzer identity covers measurement source,
language routing, parser versions, metric version, and cache schema. TypeScript/TSX
and Rust come from the `justus-camp-microsoft/tree-sitter-typescript` and
`justus-camp-microsoft/tree-sitter-rust` forks through `[patch.crates-io]`
overrides pinned to full Git revisions, also recorded in `Cargo.lock`.
Cargo retrieves the dependencies at build time; the CLI never downloads
parsers at runtime. There is no local vendored copy or dependency on a sibling checkout.
Each fork crate also hashes its grammar inputs, generated parsers, scanners,
headers, bindings, and build inputs at build time. Only the resulting digests enter
the analyzer fingerprint, not an extra copy of the generated C in the executable.
Generated parsers use pinned tooling and an ABI supported by the pinned runtime;
both TypeScript dialects are regenerated together. Provenance and commands
live in each fork's `EROSION.md`. Compatibility fixes must preserve
strict diagnostics rather than rewriting source or ignoring error nodes. Changing
counting behavior must preserve cache invalidation and update the documented
metric contract/version when semantics change.

The `erosion-v3` profile adds Rust and Gleam to default coverage while preserving
JavaScript-family and Python rules. Comparisons require the same profile and
scope across checkpoints; reports from older profiles are not directly
comparable with the new mixed-language default.

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
not from historical commits. On `measure`, `history`, and `delta`, repeated
`--exclude-paths GLOB` values append to the configured exclusions without
replacing configured include or exclude patterns. They use the same
case-sensitive, repository-relative glob matching as configuration, including
`*` matching across directory separators. The combined scope is recorded in
reports and its fingerprint, and applies at every checkpoint before language
test detection or blob/cache access. Invalid globs fail explicitly before
analysis. `modules` does not accept this option; whole-module selection and
analysis are unchanged.

Inclusion is independent of whether a file is supported by an adapter;
unsupported files remain visible in coverage when not excluded by scope.

`--exclude-tests` is an explicit opt-in on `measure`, `history`, `delta`, and
`modules`. Its `language-tests-v1` policy records every adapter's version,
exact path patterns, and syntax rules in filtered reports. Paths are
case-sensitive; wildcards within a component do not cross `/`. Concrete naming,
framework, binding, and syntax decisions live in the owning language module,
not configuration, routing, aggregation, or the shared tree walker.

Normal scope or module selection happens first. Non-regular and unsupported
entries retain their normal coverage categories: an asset has no source-language
owner, and links/gitlinks are never followed. The selected language then decides
whether a supported regular source file is a test path. Matching source files,
including source helpers beneath test directories, are counted but never read,
parsed, or looked up in the blob cache. `coverage.test_excluded_entries` is a
subset of `excluded_entries`, not an additional category; ordinary configuration
exclusions are not counted again. There is no separate generated-code, bundle,
or benchmark filter.

For retained source files, the adapter also identifies test-only syntax:

- JavaScript/TypeScript/TSX resolves literal ESM/CommonJS imports for Node test,
  Jest, Vitest, Mocha, AVA, Tape, Playwright, and uvu, including aliases,
  namespaces, destructuring, common modifiers, parameterized tests and fixtures.
  Ambient suite/test globals require framework-import evidence or an unshadowed
  suite containing test callbacks in the same scope. Local helpers are removed
  only when references connect them exclusively to test regions; shared,
  exported, shadowed, reassigned, or dynamically uncertain code is retained.
- Python classifies unittest subclasses and pytest tests/fixtures using
  framework bindings and language-specific discovery conventions. Paths cover
  `test`/`tests`/`__tests__` directories, `test_*`, `*_test`, and `conftest`
  source files. In mixed files, fixtures require a bound pytest decorator;
  other pytest definitions require a bound mark and a discoverable test name.
  unittest main calls and a pure `__main__` test-runner guard are recognized.
  Bare names/imports, assignment aliases, module `pytestmark`, and ambiguous
  inheritance or dynamic bindings are not enough.
- Rust classifies test attributes and syntax whose conditional compilation
  provably requires test mode, including inline test modules. Only `tests`
  directories and `tests.rs` receive whole-file path exclusion. Known attributes
  include built-in `test`, `tokio::test`, `async_std::test`, `rstest::rstest` and
  `test_case::test_case`, with explicit import/crate aliases. Three-valued
  `cfg`/`cfg_attr` evaluation requires definite absence outside test mode and
  possible presence in test mode; unrelated predicates remain unknown.
- Gleam recognizes public zero-argument `*_test` functions and Erlang EUnit
  `*_test_` generators independently of imports. Zero-argument `gleeunit.main()`
  calls require unshadowed module/selective imports; pure runner wrappers and
  exclusively test-referenced private helpers/imports are removed. Assertions,
  parameterized/private lookalikes, dynamic runner aliases and unknown DSLs
  do not establish tests.

This is conservative static classification, not execution of a test runner,
imports, configuration, macros, or arbitrary project code. Assertions alone do
not establish a test. Unresolved cross-file/custom wrappers and dynamically
rebound APIs remain included rather than being guessed away. Language policies
in reports describe the supported cases and limits.

Raw parsing and language validation must succeed before syntax exclusion.
An error inside apparent test syntax still fails the file; `--allow-partial`
retains the existing explicit failure policy. The shared engine normalizes
bounded byte regions, preserves line endings, and traverses the original AST
without excluded subtrees. It recomputes SLOC and CC for surviving callables:
test decisions and lines cannot remain in an enclosing production function.
Raw measurements remain available unchanged. Region byte offsets refer to the
original blob, including any UTF-8 BOM.

`coverage.syntax_test_files` counts parsed files with syntax exclusions.
`test_excluded_functions` and `test_excluded_source_lines` are raw-minus-filtered
counts for those files only; path-skipped files are not parsed to count their
contents. Source-line counts are per file, not summed overlapping function
spans. Such files stay parsed, even if no measured function remains.
Test-only modules stay present and `not_measurable`; discovery and inventory do
not depend on filtering. The effective scope/grouping fingerprint includes the
policy, and the analyzer identity covers every adapter and shared analysis rule.

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

## Whole-module history

`modules` uses the same first-parent checkpoints as `history`. Modules are
directories exactly `--depth` levels below the repository root (default 1).
Their keys are the first N directory components of tracked entry paths, never
filenames. Discovery uses every sampled commit, including unsupported files and
modules deleted before the reference commit. A file becoming a directory creates
a module only when tracked descendants establish that directory at the requested
depth; a directory replaced by a file becomes absent. Shallow paths never
collapse deeper modules.

Each checkpoint's repository inventory partitions all tracked entries into those
above the requested directory depth, those in unselected modules, and those in
selected modules. Entries outside the depth are counted explicitly but not parsed,
even when their full filenames match a glob. Selecting a parent at a shallower
depth includes its loose files; repository-root files can be measured with
`measure`. Gitlinks and symlinks do not establish module directories, but remain
counted non-regular entries when beneath a selected directory.

Repeated `--glob` options select the union of matching discovered module keys.
`*` does not cross `/`; `**` can. They never filter files within selected modules.
All tracked descendants of selected module paths are inventoried; all supported
regular source files are analyzed by default, including tests and generated code.
`--exclude-tests` applies only after discovery and module-glob selection, without
changing module identities or repository inventory counts.
Unsupported files and non-regular entries remain visible in coverage. Links
and submodules are not followed. `modules` does not accept `--config` and never
loads `erosion.toml`, even when that file contains invalid or excluding rules.
Unmatched selections fail explicitly without emitting an empty result.

Discovery inspects tree inventories, not source bodies. Analysis then streams
each checkpoint once for all selected modules and reuses the existing blob cache.
Grouping parameters never enter adapters or cached analyses. Full source paths
are passed only to the language-owned test-file classifier, not blob analysis.
The shared accumulator combines function masses and coverage identically for
repository-wide and module reports.

A module's missing checkpoint has `no_files`, a null score, and zero tracked
entries. A present module with only unsupported files or no function mass is
`not_measurable`, not absent. `no_commit` remains distinct. Changes compare only
adjacent measurable checkpoints; births, deletions, and gaps never become zero
scores or changes across missing data. Renames are separate paths, not tracked
identities. Strict/partial failure policy is identical to other commands.

## Results and persistence

Each result includes full commit identity/timestamp, requested cutoff,
coverage, total and complex mass, score, and schema/analyzer/scope provenance.
JSON is the full structured contract; CSV encodes nested diagnostics and
coverage as JSON-valued columns. Human output leads with the score or delta and
compact coverage; fingerprints, raw coverage maps and full provenance remain
in JSON/CSV. Table output is presentation, not a stable machine-readable schema.
Module tables contain only measurement rows. Repository inventory counts remain
in JSON/CSV; usage explanations belong in command help, not a report footer.
The CLI emits processing/cache diagnostics on stderr only with `--verbose`;
the analyzer's report loop is quiet. Verbosity never changes serialized results.

Parsing failures are collected, not silently dropped. By default they prevent
all result output. Opted-in partial results retain failures and exclude those
files from all measured mass. Git/configuration/cache/I/O errors are never
converted into successful-looking empty or partial results. Failure diagnostics
are never gated by verbosity; partial and unmeasurable states stay visible in
the human summary.

Persistent cache records are private local analysis artifacts. They store
function names/locations, numeric measurements, parse diagnostics, and optional
test-region offsets/reasons and filtered views, not source bodies.
Content-addressed identity and atomic writes permit reuse
between invocations without a mutable repository database. Cache entries are
reconstructible and not guaranteed durable across power loss: writes do not
force a device flush for each blob. Malformed cache records require explicit
recovery; `--no-cache` is an escape hatch.

There are no migrations, durable event streams, or changes to existing
repository data. Report schema and cache schema are versioned separately.
Delta uses unfiltered report schema 1 with `mode: "delta"`; its ordered snapshots
and CSV rows reuse existing fields. Cache schema 3 separates raw analysis from
derived test-exclusion analysis. Raw records live under
`<analyzer>/raw/<language>/<blob-prefix>/<blob>.json` and never contain a
filtered view. Derived records live under
`<analyzer>/test-exclusion/<policy-fingerprint>/<language>/<blob-prefix>/<blob>.json`;
their identity also checks the language-owned policy fingerprint. Each record
has its own payload checksum and atomic publication.

A missing derived record means detection has not been computed. A stored
`without_tests: null` explicitly means detection completed without matching
regions; a missing payload field is invalid. Non-null views contain source-line
counts, function metrics, and normalized test regions. Failed parses are stored
only as raw diagnostics and never produce a derived success.

A cold exclusion run parses once and publishes raw and derived records. A raw
cache hit alone cannot satisfy an exclusion request: a missing derived view
requires reparsing the source and computing exclusion. Recomputed raw metrics
must agree with cached metrics, and existing raw records are not rewritten.
Raw runs do not read, create, or repair derived records, even if those records
exist or are corrupt. Both layers remain path-independent and reusable across
commands and checkpoints. The new analyzer namespace bypasses old cache
records without rewriting them; no migration is needed.

Unfiltered reports retain their existing JSON/CSV shapes and versions. Explicit
test exclusion uses report schema 3 for `measure`/`history`/`delta` and schema 4
for `modules`. These filtered shapes add `test_exclusion` policy provenance,
`coverage.test_excluded_entries`, `syntax_test_files`, `test_excluded_functions`,
and `test_excluded_source_lines` (including zeros at absent checkpoints). CSV
appends `test_exclusion_json` and the four coverage counters. Human output
labels the score/history heading with `tests excluded` and includes excluded
entries in the existing skipped counts, without an explanatory footer.

Unfiltered module reports use their own schema-2 `mode: "modules"` shape with grouping
parameters/fingerprint (including `kind: "directories"`),
`file_scope: "all_tracked_entries"`, the discovered module count, repository
inventory per checkpoint, and path-sorted module series containing checkpoint
snapshots. JSON retains per-module coverage and failures; CSV has one row per
module/checkpoint with the corresponding repository inventory counts. Filtered
module reports use `file_scope: "exclude_language_tests"`; selected inventory entries
still include the entries explicitly excluded from analysis as tests. Directory
grouping identity is distinct from file-leaf grouping; those module reports
must not be compared as if their scopes were identical.
The raw metric formula/counting profile remains unchanged, but filtered scores
from a different test-policy/analyzer identity are not directly comparable.
