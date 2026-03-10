use crate::common::bytes2wcstring;
use crate::env::Environment;
use crate::parser::Parser;
use crate::terminal::Outputter;
use crate::text_face::TextFace;
use fish_color::Color;
use fish_widestring::{L, WString, wstr};
use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Everything the native prompt needs, behind a single thread-local.
struct State {
    // Immutable after first prompt render:
    colors: Option<ColorKit>,
    user_host: Option<WString>,
    home: Option<WString>,           // wide string avoids a conversion roundtrip
    pwd_cfg: Option<(usize, usize)>, // (dir_len, full_dirs)
    // Mutable — tracks the repo we're inside:
    git: Option<(PathBuf, PathBuf)>, // (repo_root, head_file_path)
    // PWD cache: re-converting WString→String on every render allocates.
    // Keep the last-seen PWD (for change detection) and its UTF-8 form
    // (for Path operations), refreshed only on directory change.
    pwd_last: WString,
    pwd_str: String,
}

struct ColorKit {
    brgreen: WString,
    red: WString,
    reset: WString,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        colors: None,
        user_host: None,
        home: None,
        pwd_cfg: None,
        git: None,
        pwd_last: WString::new(),
        pwd_str: String::new(),
    });
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate the left prompt natively when `$fish_native_prompt` is set.
/// Writes directly into `out`, reusing its capacity across renders.
/// Returns `true` if the native prompt was written; `false` to fall back.
///
/// Layout: `user@host [color]pwd[*][reset][ branch][ status] $ `
pub fn try_native_left_prompt(parser: &Parser, out: &mut WString) -> bool {
    let vars = parser.vars();
    if vars.get(L!("fish_native_prompt")).is_none() {
        return false;
    }

    // Borrow PWD from the env Arc — no new WString allocation.
    let pwd_var = match vars.get(L!("PWD")) {
        Some(v) => v,
        None => return false,
    };
    let pwd: &wstr = pwd_var
        .as_list()
        .first()
        .map(|s| s.as_utfstr())
        .unwrap_or(L!(""));

    let status = parser.get_last_statuses().status;

    // Check __DENV_DIRTY via a borrowed list — no WString allocation.
    let dirty_var = vars.get(L!("__DENV_DIRTY"));
    let denv_dirty = dirty_var
        .as_ref()
        .and_then(|v| v.as_list().first())
        .map(|s| s.as_utfstr() == L!("1"))
        .unwrap_or(false);

    out.clear();

    STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        let st = &mut *st; // reborrow for field-level split borrows

        let colors = st.colors.get_or_insert_with(|| ColorKit {
            brgreen: ansi_fg(Color::Named { idx: 10 }),
            red: ansi_fg(Color::Named { idx: 1 }),
            reset: ansi_reset(),
        });

        let user_host = st.user_host.get_or_insert_with(|| {
            let mut s = var_str(vars, L!("USER"));
            s.push('@');
            s.push_utfstr(&var_str(vars, L!("hostname")));
            s.push(' ');
            s
        });

        // Borrow immutable refs to the color escape sequences up front so
        // that later `&mut st.git` (a different field) doesn't conflict.
        let brgreen: &WString = &colors.brgreen;
        let red: &WString = &colors.red;
        let reset: &WString = &colors.reset;

        let home: &wstr = st
            .home
            .get_or_insert_with(|| {
                vars.get(L!("HOME"))
                    .and_then(|v| v.as_list().first().cloned())
                    .unwrap_or_default()
            })
            .as_utfstr();

        let (dir_len, full_dirs) = *st.pwd_cfg.get_or_insert_with(|| {
            let dl = vars
                .get(L!("fish_prompt_pwd_dir_length"))
                .and_then(|v| {
                    parse_usize_from_wstr(
                        v.as_list().first().map(|s| s.as_utfstr()).unwrap_or(L!("")),
                    )
                })
                .unwrap_or(1);
            let fd = vars
                .get(L!("fish_prompt_pwd_full_dirs"))
                .and_then(|v| {
                    parse_usize_from_wstr(
                        v.as_list().first().map(|s| s.as_utfstr()).unwrap_or(L!("")),
                    )
                })
                .unwrap_or(1);
            (dl, fd)
        });

        // Refresh the UTF-8 pwd cache only when the directory changes,
        // reusing the String allocation on the hot path.
        if st.pwd_last.as_utfstr() != pwd {
            st.pwd_last.clear();
            st.pwd_last.push_utfstr(pwd);
            st.pwd_str.clear();
            for &c in pwd.as_char_slice() {
                st.pwd_str.push(c);
            }
        }

        let pwd_color = if status == 0 { brgreen } else { red };
        out.push_utfstr(user_host);
        out.push_utfstr(pwd_color);
        prompt_pwd_into(out, pwd, home, dir_len, full_dirs);
        out.push_utfstr(reset);

        if denv_dirty {
            out.push_utfstr(red);
            out.push_utfstr(L!(" *"));
            out.push_utfstr(reset);
        }

        // git_branch_into writes " branch" (with leading space) if in a repo.
        git_branch_into(&mut st.git, Path::new(&st.pwd_str), out);
        out.push_utfstr(L!(" $ "));
    });

    true
}

// ---------------------------------------------------------------------------
// prompt_pwd — zero-alloc path shortening, writes directly into output WString
// ---------------------------------------------------------------------------

/// Shorten `$PWD` for display: tilde-contract home, then abbreviate leading
/// directory components to `dir_len` chars (0 = no shortening),
/// keeping the last `full_dirs` components full.
///
/// Writes directly into `out` — no intermediate allocation.
fn prompt_pwd_into(out: &mut WString, pwd: &wstr, home: &wstr, dir_len: usize, full_dirs: usize) {
    let pwd_chars = pwd.as_char_slice();
    let home_chars = home.as_char_slice();

    let tilde = !home_chars.is_empty()
        && pwd_chars.starts_with(home_chars)
        && (pwd_chars.len() == home_chars.len() || pwd_chars[home_chars.len()] == '/');

    let remainder: &[char] = if tilde {
        &pwd_chars[home_chars.len()..]
    } else {
        pwd_chars
    };

    if dir_len == 0 {
        if tilde {
            out.push('~');
        }
        for &c in remainder {
            out.push(c);
        }
        return;
    }

    // Two-pass over '/' segments: count for shorten_up_to, then emit.
    let n_parts = remainder.split(|&c| c == '/').count();
    let shorten_up_to = n_parts.saturating_sub(full_dirs.min(n_parts));

    for (i, part) in remainder.split(|&c| c == '/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        // The first segment is empty when tilde is true (leading '/');
        // replace it with '~'.
        if tilde && i == 0 {
            out.push('~');
        } else if i < shorten_up_to && !part.is_empty() {
            let skip = usize::from(part[0] == '.');
            let take = (skip + dir_len).min(part.len());
            for &c in &part[..take] {
                out.push(c);
            }
        } else {
            for &c in part {
                out.push(c);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Git branch (native .git/HEAD reader)
// ---------------------------------------------------------------------------

/// Write " branch" into `out` (with leading space) if `cwd` is inside a git repo.
/// Returns true if a branch was written.
/// Caches the repo root and HEAD path to avoid a directory walk on each render.
fn git_branch_into(
    cache: &mut Option<(PathBuf, PathBuf)>,
    cwd: &Path,
    out: &mut WString,
) -> bool {
    // Fast path: still inside the cached repo.
    if let Some((root, head_path)) = cache.as_ref() {
        if cwd.starts_with(root) {
            return read_head_into(head_path, out);
        }
    }

    // Slow path: walk up to find .git, then cache.
    match find_git_dir(cwd) {
        Some(git_dir) => {
            let root = git_dir.parent().unwrap_or(cwd).to_path_buf();
            let head_path = git_dir.join("HEAD");
            let written = read_head_into(&head_path, out);
            *cache = Some((root, head_path));
            written
        }
        None => {
            *cache = None;
            false
        }
    }
}

/// Walk ancestors of `start` looking for `.git` (dir or worktree file).
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let dot_git = dir.join(".git");
        if let Ok(meta) = std::fs::symlink_metadata(&dot_git) {
            if meta.is_dir() {
                return Some(dot_git);
            }
            if meta.is_file() {
                return resolve_gitdir_file(&dot_git);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Follow the `gitdir:` indirection in a worktree/submodule `.git` file.
fn resolve_gitdir_file(path: &Path) -> Option<PathBuf> {
    let mut buf = [0u8; 512];
    let n = std::fs::File::open(path).ok()?.read(&mut buf).ok()?;
    let raw = std::str::from_utf8(&buf[..n]).ok()?.trim();
    let target = Path::new(raw.strip_prefix("gitdir: ")?);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        path.parent()?.join(target)
    })
}

/// Read `.git/HEAD` and write " branch" (with leading space) into `out`.
/// Uses a stack buffer — no heap allocation.
fn read_head_into(head_path: &Path, out: &mut WString) -> bool {
    let mut buf = [0u8; 256];
    let n = match std::fs::File::open(head_path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let line = match std::str::from_utf8(&buf[..n]) {
        Ok(s) => s.trim_end(),
        Err(_) => return false,
    };
    let branch = if let Some(b) = line.strip_prefix("ref: refs/heads/") {
        b
    } else if let Some(b) = line.strip_prefix("ref: ") {
        b
    } else {
        &line[..line.len().min(8)]
    };
    if branch.is_empty() {
        return false;
    }
    out.push(' ');
    push_ascii(out, branch);
    true
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn var_str(vars: &impl Environment, name: &'static wstr) -> WString {
    vars.get(name).map(|v| v.as_string()).unwrap_or_default()
}

fn ansi_fg(color: Color) -> WString {
    let mut o = Outputter::new_buffering();
    o.set_text_face(TextFace::new(color, Color::None, Color::None, Default::default()));
    bytes2wcstring(o.contents())
}

fn ansi_reset() -> WString {
    let mut o = Outputter::new_buffering();
    o.reset_text_face();
    bytes2wcstring(o.contents())
}

/// Push ASCII bytes directly into a WString (one char per byte).
#[inline]
fn push_ascii(out: &mut WString, s: &str) {
    out.reserve(s.len());
    for &b in s.as_bytes() {
        out.push(char::from(b));
    }
}

/// Parse a `usize` from a `&wstr` without allocating a String.
fn parse_usize_from_wstr(s: &wstr) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let mut result = 0usize;
    for &c in s.as_char_slice() {
        let digit = (c as u32).checked_sub('0' as u32)?;
        if digit > 9 {
            return None;
        }
        result = result.checked_mul(10)?.checked_add(digit as usize)?;
    }
    Some(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pwd(path: &str, home: &str, dir_len: usize, full_dirs: usize) -> String {
        let path_w = WString::from(path);
        let home_w = WString::from(home);
        let mut out = WString::new();
        prompt_pwd_into(
            &mut out,
            path_w.as_utfstr(),
            home_w.as_utfstr(),
            dir_len,
            full_dirs,
        );
        out.to_string()
    }

    #[test]
    fn pwd_shortens_middle_components() {
        assert_eq!(pwd("/home/u/dev/fish-shell", "/home/u", 1, 1), "~/d/fish-shell");
    }

    #[test]
    fn pwd_no_shorten_when_zero() {
        assert_eq!(pwd("/home/u/dev/fish-shell", "/home/u", 0, 1), "~/dev/fish-shell");
    }

    #[test]
    fn pwd_root() {
        assert_eq!(pwd("/", "/home/u", 1, 1), "/");
    }

    #[test]
    fn pwd_preserves_leading_dot() {
        assert_eq!(pwd("/home/u/.config/fish", "/home/u", 1, 1), "~/.c/fish");
    }

    #[test]
    fn pwd_home_exactly() {
        assert_eq!(pwd("/home/u", "/home/u", 1, 1), "~");
    }

    #[test]
    fn pwd_no_tilde_when_outside_home() {
        assert_eq!(pwd("/var/log/syslog", "/home/u", 1, 1), "/v/l/syslog");
    }

    #[test]
    fn pwd_no_false_tilde_on_prefix_match() {
        // /home/user2 should NOT match home=/home/user
        assert_eq!(pwd("/home/user2/foo", "/home/user", 1, 1), "/h/u/foo");
    }

    #[test]
    fn parse_usize_works() {
        assert_eq!(parse_usize_from_wstr(WString::from("0").as_utfstr()), Some(0));
        assert_eq!(parse_usize_from_wstr(WString::from("42").as_utfstr()), Some(42));
        assert_eq!(parse_usize_from_wstr(WString::from("").as_utfstr()), None);
        assert_eq!(parse_usize_from_wstr(WString::from("x").as_utfstr()), None);
    }
}
