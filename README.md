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
The idea for this CLI came from [this article](https://earendil.com/posts/measuring-code-sloppiness/)
## Build and install

Requires Rust 1.88 or newer, a C toolchain for the bundled Tree-sitter grammars,
and Git 2.45 or newer (for enforced no-lazy-fetch behavior).

```console
cargo install --locked --path .
```

The binary embeds the parsers. No Tree-sitter CLI, AST-Grep, Node, or Python
installation is needed at runtime.

## Supported languages

- JavaScript: `.js`, `.jsx`, `.mjs`, `.cjs`
- TypeScript: `.ts`, `.mts`, `.cts`, `.tsx`
- Python: `.py`, `.pyw`, `.pyi`
- Rust: `.rs`
- Gleam: `.gleam`

## Usage
```
Usage: erosion <COMMAND>

Commands:
  measure  Measure committed HEAD or a selected revision
  history  Measure first-parent history at UTC calendar checkpoints
  modules  Measure whole-module history by selecting discovered module paths
  delta    Compare two committed snapshots: erosion at TO minus erosion at FROM
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```
