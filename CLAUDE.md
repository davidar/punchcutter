# punchcutter development notes

A GPU text engine: OpenType shaping and Slug glyph coverage in Slang compute
shaders, with a CPU side that only parses fonts into bytes and finds them
through fontconfig. README.md has the overview, the build and the two
harnesses. The modules live in `slang/modules/`, inside the `punchcutter-slang`
crate that compiles them in process.

**References** (living, in `docs/`): arena.md the byte-level contract between
the crates and the Slang modules; roadmap.md what is not built yet.
CONTRIBUTING.md has the rules. `docs/check-links.sh` checks every `docs/….md`
pointer.

## Ground rules

- **Public voice.** Everything written here is for a reader who has never met
  us: no personal names, machine names or dates as narrative in comments or
  documents, no history, no private labels. Cite documents by file and section
  name, never by number. Before a push: `docs/check-links.sh` clean.
- **The engine knows nothing about its consumer.** No module or crate refers
  to what draws the text.
- **GPU first.** The CPU keeps I/O; shaping, coverage and every table walk are
  the shaders'. No CPU shaper, not even a reference one (harfrust is the
  oracle, in an example).
- **Slang, not WGSL**, for every shader. `slang/modules/shape_abi.slang` is generated
  (`cargo run -p punchcutter-shape --bin gen-slang`); never edit it by hand.
- **The GPU on this machine**: the harnesses pick an integrated or software
  adapter and refuse a discrete NVIDIA one; keep it that way.
- Rust is formatted with rustfmt's defaults: `cargo fmt --all` before a
  commit; CI checks `cargo fmt --all -- --check`.
- Never kill a process by name; kill tracked PIDs only.

## Dependency hygiene

- **Never run bare `cargo update` or `cargo add`.** Anything that can change
  `Cargo.lock` goes through `cargo cooldown update` / `cargo cooldown add
  <crate>` (policy in `cooldown.toml`). Run it as
  `TMPDIR=$HOME/.cache/cooldown-tmp cargo cooldown check`: `/tmp` is a small
  RAM-backed tmpfs. Scratch builds of any size go under `~/.cache`, never
  `/tmp`.
- `Cargo.lock` is committed and is the source of truth; build with `--locked`.

## Host constraints

- The host is an immutable image without the unversioned `.so` symlinks the
  linker wants. `cargo check` is fine; `cargo build`, `cargo test` and
  `cargo run` need `LIBRARY_PATH=/var/home/linuxbrew/.linuxbrew/lib`.
- Slang needs no install: `punchcutter-slang` carries it (the `shader-slang`
  fork downloads the pinned release once, into `~/.cache/shader-slang`).
  `spirv-val` is used when present.

## Git

- Stage by explicit path (`git status --short`, then `git add <file> …`);
  `git add -A` / `--all` / `.` are denied on purpose, so that nothing reaches
  a commit by a wildcard.
