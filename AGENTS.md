# What We've Changed

## Native Prompt (`src/reader/native_prompt.rs`)

The default fish prompt (`fish_prompt`) is implemented in fish script: it calls
`fish_git_prompt` which spawns a `git` subprocess on every prompt render to
determine the branch, dirty state, etc. On large repos this can take 50-200ms
per keypress.

We added a native (Rust) prompt that bypasses all of that. When
`$fish_native_prompt` is set, `try_native_left_prompt` is called instead of
evaluating the fish-script prompt function. It produces a
`user@host pwd branch (status) $ ` prompt entirely in Rust with:

- **Zero subprocess spawns.** Git branch is read by opening `.git/HEAD`
  directly with a stack buffer (no heap allocation for typical HEAD files).
- **Cached repo root.** After the first prompt render finds a `.git` directory
  by walking ancestors, the repo root and HEAD file path are cached in a
  thread-local. Subsequent renders just re-read HEAD (one `open` + `read`
  syscall) without re-walking the directory tree.
- **Worktree/submodule support.** If `.git` is a file (worktree indirection),
  `resolve_gitdir_file` follows the `gitdir:` pointer using a stack buffer.
- **Zero-alloc path shortening.** `prompt_pwd_into` writes directly into the
  output WString without intermediate String allocations. It respects
  `$fish_prompt_pwd_dir_length` and `$fish_prompt_pwd_full_dirs`.
- **No ANSI escape recomputation.** Color sequences are computed once and
  cached in the thread-local `ColorKit`.

The result is a prompt that renders in ~5μs instead of ~100ms.

## Path Jump (`^J`) — `src/reader/path_jump.rs`

A new interactive feature bound to `Ctrl-J`. It opens a pager showing
recently-used file paths from history, ranked by frecency (frequency × recency
weight). The user can fuzzy-filter by typing, then act on the selected path:

- **Enter** confirms the selection, then a second **Enter** inserts the path at
  the cursor position.
- **`e`** opens the path in `$EDITOR`.
- **`c`** runs `cd` to the path's parent directory.
- **`l`** runs `ls` on the path.
- **Backspace** returns to filtering from the confirmed state.
- **Escape** closes the pager.

### Performance: Build Once, Filter Synchronously

The original implementation called `History::item_at_index()` up to 10,000
times per keystroke. Each call acquired the history mutex and, for on-disk
items, decoded YAML. This meant every keystroke triggered: mutex lock → 10K
iterations → YAML decode per old item → frecency scoring → fuzzy filtering →
mutex unlock.

We split this into two phases:

1. **`build_index`** (expensive, once per `^J` press): Runs on a background
   thread. Calls `History::collect_path_entries()`, which iterates all history
   items under a **single mutex acquisition**, extracting deduplicated
   `(path, frequency, last_used)` tuples. Then applies frecency scoring and
   sorts by score descending into a `PathIndex`.

2. **`filter_index`** (cheap, per keystroke): Runs **synchronously** on the
   main thread. Iterates the pre-built `PathIndex` (typically a few hundred
   deduplicated paths, not 10K history items), applies fuzzy matching, and
   returns completions. No mutex, no I/O, no debouncer delay.

The `PathIndex` is cached on `ReaderData` for the duration of the `^J` session
and freed when the pager closes.

#### Key files

| File | Role |
|------|------|
| `src/reader/path_jump.rs` | `PathIndex`, `build_index`, `filter_index`, `format_path_for_insertion`, frecency scoring |
| `src/history/history.rs` | `collect_path_entries()` — batch path extraction under single lock |
| `src/reader/reader.rs` | `path_jump_index` cache, `rl::PathJump` handler, `fill_path_jump_pager`, `execute_path_jump_action`, `set_path_jump_completions` |
| `src/reader/iothreads.rs` | `path_jump` debouncer (used only for the initial `build_index` call) |
| `src/input_common.rs` | `ReadlineCmd::PathJump` variant |
| `src/input.rs` | `path-jump` binding name registration |
| `src/pager.rs` | `help_lines`, `search_field_no_underline` support |
| `share/functions/__fish_shared_key_bindings.fish` | `ctrl-j` rebound from `execute` to `path-jump` |

## Other Changes

- **Justfile**: Added `install-dev` target (debug build), fixed indentation.
- **Embedded completions**: Reduced binary size by ~5MB by only embedding a
  select few completion files instead of all of them.
