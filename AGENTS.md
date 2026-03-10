# fish-shell fork: native-prompt branch

Personal fork of [fish-shell](https://github.com/fish-shell/fish-shell) focused on startup speed, binary size, and eliminating prompt latency.

## Modifications

### Native prompt (`src/reader/native_prompt.rs`)

When `$fish_native_prompt` is set, the prompt renders entirely in Rust — no fish script evaluation, no subprocesses. Layout: `user@host pwd branch $`.

- **Zero-alloc hot path**: colors, user@host, home dir, and git repo root are cached in thread-local state. PWD shortening and `.git/HEAD` reading use stack buffers.
- **Git branch**: walks up to `.git/` once per directory change, then reads HEAD from the cached path. Supports worktrees and submodules via `gitdir:` indirection.
- **PWD shortening**: tilde-contracts home, abbreviates leading components to `$fish_prompt_pwd_dir_length` chars, keeps last `$fish_prompt_pwd_full_dirs` components full.
- **denv integration**: shows a red `*` when `$__DENV_DIRTY` is set.
- **Exit status**: PWD is green on success, red on failure.

### Selective completion embedding (`build.rs`, `embedded_completions.txt`)

A build.rs generator reads `embedded_completions.txt` (one command name per line) and only embeds those completions plus functions and the default theme. Saves ~5 MB. Delete the file to restore the upstream embed-everything behavior.

### Optimized release profile (`Cargo.toml`, `Makefile`)

- Release profile: `opt-level = 3`, fat LTO, single codegen unit, `panic = "abort"`, stripped.
- `make build-release`: nightly `build-std` build for minimal binary size.
- `make install`: installs to `/usr/local/bin/fish`, adds to `/etc/shells`, sets as login shell.

### Vendored shellexpand (`vendor/shellexpand/`)

shellexpand 3.1.1 calls `var_name.as_str()` which collides with the inherent `str::as_str()` in nightly Rust. Patched via `[patch.crates-io]` to use fully-qualified trait syntax.

## Coding style

- Use comments sparingly. Don't explain what the code is doing, rather explain why.
