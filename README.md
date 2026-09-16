# erosion

A read-only Rust CLI for measuring code erosion in an existing Git repository,
at one committed revision, between two commits, or at calendar checkpoints
through its history.

Erosion is the percentage of complexity-weighted function mass in functions
whose cyclomatic complexity exceeds 10:

```text
mass(function) = cyclomatic_complexity * sqrt(source_lines)
erosion = 100 * sum(mass where complexity > 10) / sum(all mass)
```

It is an exploratory structural signal, not a judgment that complex code is
unnecessary, defective, or AI-generated. Adding simple functions can lower the
score even when existing functions become more complex.

## Build and install

Requires Rust 1.88 or newer, a C toolchain for the bundled Tree-sitter grammars,
and Git 2.45 or newer (for enforced no-lazy-fetch behavior).

```console
cargo install --locked --path .
```

The binary embeds the parsers. No Tree-sitter CLI, AST-Grep, Node, or Python
installation is needed at runtime.

## Measure a revision

Run from anywhere inside the repository you want to measure, or select one
explicitly with `--repo-path`:

```console
erosion measure
erosion measure --ref main --top 20
erosion measure --ref v2.0.0 --format json
erosion measure --repo-path ~/Code/engpulse
```

The default revision is **committed `HEAD`**. Modified and untracked working-tree
files are not analyzed. `--top N` shows up to N functions with complexity above
10, ranked by mass, with path, name, line range, complexity, and source lines.
It is available in table and JSON output, not CSV.

Default output is a compact human-readable summary: score, short commit/date,
file and function counts, languages, skipped files, and parse failures. History
and delta use compact comparison tables instead of repeating per-commit metadata.
Full timestamps, hashes, scope, parser versions, masses, and coverage details
remain in `--format json` and `--format csv`.

Successful runs are quiet on stderr unless `--verbose` (or `-v`) is requested.
That flag shows processing and cache diagnostics without changing result output.
Errors and parse-failure diagnostics are always shown, including with
`--allow-partial`. When using Cargo, suppress Cargo's own build/launch messages
with `cargo run --quiet -- measure --repo-path ../engpulse`.

### Selecting a repository

`--repo-path <PATH>` is available on `measure`, `history`, and `delta`. It
defaults to `.` and accepts the worktree root or a directory inside it. An
explicit path lets you run from outside any Git repository or from a different
repository:

```console
erosion history --repo-path ../engpulse --since 3y --every 6mo
erosion delta HEAD~1 HEAD --repo-path ~/Code/engpulse
```

Relative repository paths are resolved from the invocation's working directory.
The CLI does not change that directory: explicit relative `--config` and
`--cache-dir` paths remain relative to it as well. Automatic `erosion.toml`
discovery uses the **selected repository's root**, and the cache must remain
outside the selected worktree. Invalid repository paths fail without falling
back to the invocation's repository. Omitting the flag preserves the usual
current-directory discovery.

## Compare two commits

```console
erosion delta HEAD~1 HEAD
erosion delta v1.0.0 main --format json
erosion delta abc1234 def5678 --config erosion.toml --format csv
```

Both positional revisions are required. The result is **erosion at TO minus
erosion at FROM**, in percentage points: positive means the erosion score
increased, negative means it decreased. For example, 40% to 45% is **+5 pp**,
not a relative percentage change.

Each revision is measured over its entire selected source, not just changed
files or lines. The same current scope configuration and analyzer apply to both.
Added and deleted files therefore affect the result. Commits need not share
ancestry or be in chronological order; no merge base is substituted. Comparing
a measurable commit with itself gives zero. Neither commit is checked out.
Available revisions in a shallow repository can be compared without traversing
history.

Table output shows FROM and TO, their scores and coverage, and the signed delta.
JSON uses the existing report shape with `mode: "delta"`, `reference` set to TO,
and exactly two `snapshots` in FROM/TO order. Each `cutoff_utc` is that commit's
committer timestamp; `snapshots[1].change_pp` is the delta and the first snapshot's
change is null. CSV uses the existing columns with two rows in the same order,
and the second row's `change_pp` holds the delta.

If either snapshot has no function mass, the delta is not measurable (null in
JSON, empty in CSV, and labeled `not measurable` in the human summary), not zero. Parse failures at either revision
prevent all result output unless `--allow-partial` is supplied; partial scores
and their difference exclude failed files and are explicitly labeled incomplete.
Invalid revisions and Git/cache/configuration errors always fail. `delta`
accepts the common repository, format, scope, and cache options, but not `--ref`, `--top`, or
history cadence options. `--verbose` is supported on every command.

## Measure history

```console
erosion history --since 3y --every 6mo
erosion history --ref main --since 2023-09-14 --until 2026-09-14 --every 6mo
erosion history --since 12w --every 2w --format csv > history.csv
erosion history --since 1y --format json > history.json
```

- `--since` is required: a date (`YYYY-MM-DD`) or positive integer duration.
- `--every` defaults to `6mo`. Durations accept `d`, `w`, `mo`, and `y`;
  `1m`, fractions, and compound durations such as `1y6mo` are not accepted.
- The endpoint defaults to the selected revision's **committer timestamp**,
  not the current clock. An explicit `--until` includes that whole UTC date.
  A `--since` date starts at midnight UTC.
- Calendar ticks are anchored to the endpoint, with month-end clamping. Both
  endpoints are included; the first interval can be shorter than the cadence.
  Three years at six-month intervals produces seven checkpoints.
- Only first-parent ancestors of `--ref` are considered. At each cutoff, select
  the first ancestor in chain order whose committer timestamp is at or before
  the cutoff. This also defines behavior for non-monotonic commit timestamps.
- Multiple checkpoints may select the same commit. Both the cutoff and actual
  commit timestamp/SHA are reported.
- A cutoff before any qualifying commit produces a `no_commit` row, not a
  fabricated zero or a silently shortened window.
- Shallow history is rejected for `history`, while `measure` can analyze an
  available shallow revision. Missing required objects and other
  Git errors fail explicitly; the CLI never fetches or updates branches.
- Requests are limited to 4,096 checkpoints; wider series require a wider cadence.

Bare, partial/promisor, and grafted repositories are rejected explicitly.

`measure`, `delta`, and `history` analyze all selected files independently at each
checkpoint. They do not track unchanged code, file renames, or common-file
cohorts. Changes in repository composition can affect the trend.

## Languages and scope

Supported extensions:

| Adapter | Extensions |
|---|---|
| JavaScript (including JSX) | `.js`, `.jsx`, `.mjs`, `.cjs` |
| TypeScript | `.ts`, `.mts`, `.cts` |
| TSX | `.tsx` |
| Python | `.py`, `.pyw`, `.pyi` |

By default, **all supported tracked regular files** are selected, regardless
of directory. There are no implicit test, declaration, vendor, or generated-code
exclusions, and no generated-header detection. Unsupported files, exclusions,
symlinks, and submodules are counted separately; links and submodules are not
followed. A mixed-language repository's score describes only its supported,
selected source, not the whole codebase.

An optional `erosion.toml` at the repository root defines scope:

```toml
include = ["**"]
exclude = [
  "**/node_modules/**",
  "**/dist/**",
  "**/generated/**",
  "**/*.min.js",
  "**/*.d.ts",
  "**/*.test.*",
  "**/__tests__/**",
]
```

These are examples, not universal production-code rules. Adapt them to the
repository. Globs match repository-relative paths with `/` separators; a file
must match an include and must not match any exclude. An empty include array
selects nothing. Unknown configuration keys and invalid globs are errors.

Use `--config path/to/config.toml` to select another file. Relative explicit
paths are resolved from the invocation directory. Otherwise discovery is only
at the selected Git worktree root, so running from a subdirectory or using
`--repo-path` uses the same scope.

The configuration is read **once from the current filesystem**, then applied
to every historical revision. Its effective contents and fingerprint are
included in results. No historical configuration or source code is executed.

For a Python-only series, use a config containing
`include = ["**/*.py", "**/*.pyw", "**/*.pyi"]`. Language coverage is reported
separately, but the score combines selected functions' mass across languages;
it is not the average of language percentages.

## Coverage, errors, and output

Use `--format table` (default), `json`, or `csv`. Routine processing/cache messages
go to stderr only with `--verbose`; errors and parse diagnostics always go to
stderr. Only results go to stdout. Results are buffered until the invocation
has completed successfully. CSV has one summary row per checkpoint, including
JSON-encoded coverage maps and failure details.

Parsing is strict by default: all encountered file/checkpoint parse failures
are listed and the invocation exits unsuccessfully without emitting a result
document. `--allow-partial` instead reports `partial` snapshots, excludes failed
files from the metric, and retains paths, diagnostics, and physical LOC in
JSON/CSV. Human summaries prominently label partial results and failed counts.
It does **not** relax Git, configuration, cache, or I/O errors.

| Status | Meaning |
|---|---|
| `complete` | Selected files parsed and measurable function mass exists. |
| `partial` | At least one selected file failed; score covers only parsed files. |
| `not_measurable` | No measurable function mass; score is null, not zero. |
| `no_commit` | No ancestor qualifies at this checkpoint; score is null. |

Unavailable table cells use `--`; measure/delta summaries say `not measurable`.
Null scores use empty CSV fields. A partial snapshot
may also have a null score. `change_pp` is the difference from the immediately
preceding checkpoint only when both scores exist. A complete score of zero
means measurable functions exist but none has complexity above 10.

Exit codes: `0` for successful output (including explicit nulls or opted-in
partial results), `1` for runtime or semantic validation errors (including an
invalid cadence or `--top` with CSV), and `2` for argument-parser errors such as
unknown flags, missing required arguments, or conflicting cache flags. An unborn
repository or invalid revision cannot be measured.

## Cache and reproducibility

Unchanged blobs are parsed once per language during an invocation. Persistent
per-blob analysis is stored under the platform user cache directory's `erosion`
subdirectory, outside the measured worktree:

```console
erosion history --since 3y --cache-dir /path/outside/repository/cache
erosion measure --no-cache
```

`--no-cache` disables disk reads/writes but keeps in-process reuse. Cache keys
include the blob ID, grammar selection, exact parser versions, record schema,
and an embedded analyzer-source fingerprint. Records contain metrics and parse
diagnostics, not source bodies. Functions' names and line locations are retained.
Cache files are published atomically and include payload checksums. They are
reconstructible, so writes do not force a device flush per blob and are not
guaranteed durable across a power loss. Corrupt records are explicit errors with
the affected path and recovery guidance, rather than silently trusted results.
Even a strict failed run may populate the cache with parse-failure records.

Cache hit counts are verbose diagnostics only; cold, warm, and disabled-cache result
documents are identical for the same inputs. Output schema version, metric
version, analyzer fingerprint, parser versions, effective scope, cutoff, and
full commit IDs are recorded.

## Measurement conventions

The `erosion-v2` profile supports JS/TS and Python. JS/TS counting is unchanged
from `erosion-js-v1`, but Python is now selected by default. Do not interpret a
change from an older JS-only score to a mixed-language score as code erosion:
rerun all checkpoints with the same profile and scope. Report and cache record
shapes remain at schema version 1; the new analyzer identity uses separate cache
entries rather than migrating old records.

The JavaScript-family adapter includes body-bearing declarations, expressions,
generator functions, arrows, methods, and anonymous callbacks. Complexity is
1 plus `if`, `catch`, `switch`, ternary, `do`, `for`, `for-in`/`for-of`, `while`,
and `&&`/`||` binary expressions. A switch counts once, not once per case.
`??` and optional chaining do not increment complexity.

The Python adapter includes `def`, `async def`, methods, and lambdas. Decorators
are outside a function's span; classes themselves are not functions. Stub
functions with `...` bodies are included unless excluded by scope. Complexity
starts at 1 and adds:

- One for each `if`, `elif`, `for`/`async for`, `while`, `except`/`except*`,
  conditional expression, `assert`, and binary `and`/`or`.
- One for each comprehension/generator `for` and filter.
- One for loop `else` and `try` `else`, but not ordinary `if` `else`.
- One for each non-catch-all `match` case and each guard. Wildcards and bare
  captures are catch-alls; OR-pattern alternatives do not add extra decisions.

`finally`, `with`/`async with`, `await`, `yield`, `not`, and ordinary calls do not
independently increment complexity. Ordinary string-literal expressions at the
start of a module, function, or class body are excluded as docstrings, including
raw, concatenated, and parenthesized strings. Assigned strings, later string
expressions, bytes literals, and f-strings remain source.

Nested functions contribute independently, and their bodies also contribute
to their enclosing function's complexity and size. Source lines are counted
over the full callable span, including its signature. Comments are blanked
while preserving line breaks; blank and punctuation-only (`{}[]();,:`) lines
are excluded. This is a line-based convention, not a semantic statement count.
Zero-source-line callables are omitted.

Input must be UTF-8; an initial UTF-8 BOM is stripped. Rows are LF-separated,
with CRLF supported and ASCII whitespace trimmed. Unlike the exploratory
Python script's `splitlines`, lone CR or Unicode line separators do not create
new rows: this aligns positions with Tree-sitter.

These are versioned language-specific conventions inspired by the paper's metric,
not a claim of exact equivalence to Radon, the original Python tooling, or calibrated
comparability between programming languages. This CLI calculates **erosion
only**, not the AST-Grep verbosity or duplication metrics.

## Development

```console
cargo fmt --all -- --check
cargo check-all
cargo lint
cargo test-all
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for module boundaries and invariants.
