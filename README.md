# scip-shell

Generate [SCIP](https://github.com/sourcegraph/scip) code-intelligence indexes
for shell scripts. Parsing is done with
[`brush-parser`](https://crates.io/crates/brush-parser), the POSIX/bash parser
from the [brush](https://github.com/reubeno/brush) shell, so we work from a real
AST with source spans rather than a regex approximation. Shell is dynamically
typed and dynamically scoped, so a fair amount of this is necessarily a static
approximation -- see the limitations below.

## What it indexes

- **Function definitions and calls.** Each `name() { ... }` becomes a global SCIP
  symbol. A command word that names a function is recorded as a reference, so "go
  to definition" and "find references" work across a file, including calls that
  precede the definition. A function takes precedence over a `$PATH` binary of
  the same name.
- **Variable assignments and references.** Assignments (`X=...`, `X+=...`,
  array elements) define a symbol; `$X` and `${X}` mentions reference it.
  Variables are emitted as SCIP *local* symbols because shell variables are
  dynamically scoped and have no stable cross-file identity. A `local` (or
  `declare` / `typeset`) declaration inside a function gets its own scope, so a
  function-local `name` does not collide with a global one; plain references
  resolve to the global. Positional and special parameters (`$1`, `$?`, `$@`,
  ...) are skipped.
- **Binaries on `$PATH`.** A command word that resolves to an executable on
  `$PATH` (e.g. `grep`, `ls`) becomes a global symbol in a synthetic `system`
  package, keyed by name. The name is not resolved to an absolute path, so the
  same command cross-references regardless of where it lives on a given host.
- **Filesystem paths.** Absolute paths (`/usr/share/dict/words`), used as a
  command, an argument, an assignment value (`CONF=/etc/app.conf`), a redirection
  target, or a `#!` interpreter, become global symbols in a synthetic
  `filesystem` package keyed by the path string.
- **Sourced files.** The file argument of `source` / `.` becomes a global symbol
  in a synthetic `source` package, the one construct that crosses file
  boundaries in shell. Only literal paths are linked; `source "$lib"` is not.
- **Expansions.** Variable reads, command substitutions (`$(...)` and
  `` `...` ``) and arithmetic (`$((...))`) are scanned out of each word.
  Command substitutions are re-parsed so the binaries, variables and paths
  inside them are indexed too; command and process substitutions are treated as
  subshells, so a `local` or function defined inside one does not leak outward.
  Arithmetic
  records both bare names (`$((count + 1))`) and `$`-prefixed reads
  (`$(($base + 1))`). Parameter expansions report the parameter, including the
  `${#name}` length and `${!name}` indirection forms, and references nested in a
  default or subscript (`${x:-$y}`, `${arr[$i]}`). The standalone arithmetic
  command (`(( count += 1 ))`) has its reads indexed as well. Here-document
  bodies are scanned for references when the here-end is unquoted.

Function symbols carry the comment block immediately above the definition as
their documentation. Every occurrence is tagged with a SCIP `SyntaxKind`
(function, local, builtin, string literal) for semantic highlighting.
References inside single quotes are treated as literal text, and `\$` is
honoured as an escape.

## Usage

```sh
# Index a directory tree into index.scip. Files with a shell extension
# (.sh, .bash, .ksh, .zsh) and extensionless files with a shell shebang
# are picked up.
scip-shell --project-root . src/

# Index specific files to a named output
scip-shell script.sh lib.bash -o out.scip
```

Run `scip-shell --help` for the full set of options (project root, output path,
package name and version recorded in symbols).

## Limitations

- Variable scoping handles `local` declarations but not full data-flow: a plain
  assignment to a name inside a function is treated as touching whichever
  variable is visible at that point (a `local` if one has been declared,
  otherwise the global), without modelling shell's runtime dynamic scope. This
  is good enough for the common cases but will get fairly aggressive aliasing
  wrong.
- Only absolute paths are recognised at the moment; relative paths (`./build.sh`)
  are left unindexed.
- Binary resolution depends on the `$PATH` of the process running the indexer, so
  an index is only as reproducible as that `$PATH`.
- The C-style arithmetic `for` loop (`for ((i=0; i<n; i++))`) is not indexed: its
  three sub-expressions share a single source span, so there is no reasonable way
  to locate them individually.
