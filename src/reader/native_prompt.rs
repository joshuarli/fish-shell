use crate::common::bytes2wcstring;
use crate::env::Environment;
use crate::parser::Parser;
use crate::terminal::Outputter;
use crate::text_face::TextFace;
use fish_color::Color;
use fish_widestring::{L, WString};
use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Everything the native prompt needs, behind a single thread-local.
struct State {
    // Immutable after first prompt render:
    colors: Option<ColorKit>,
    user_host: Option<WString>,
    home: Option<String>,
    pwd_cfg: Option<(usize, usize)>, // (dir_len, full_dirs)
    // Mutable — tracks the repo we're inside:
    git: Option<(PathBuf, PathBuf)>, // (repo_root, head_file_path)
}

struct ColorKit {
    brgreen: WString,
    green: WString,
    red: WString,
    reset: WString,
}

thread_local! {
    static STATE: RefCell<State> = const {
        RefCell::new(State {
            colors: None,
            user_host: None,
            home: None,
            pwd_cfg: None,
            git: None,
        })
    };
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate the left prompt natively when `$fish_native_prompt` is set.
///
/// Layout: `user@host brgreen(pwd)reset branch (status) $ `
pub fn try_native_left_prompt(parser: &Parser) -> Option<WString> {
    let vars = parser.vars();
    vars.get(L!("fish_native_prompt"))?;

    let pwd_wstr = var_str(vars, L!("PWD"));
    let pwd = pwd_wstr.to_string();
    let status = parser.get_last_statuses().status;

    STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        let st = &mut *st; // reborrow so we can split-borrow fields

        let colors = st.colors.get_or_insert_with(|| ColorKit {
            brgreen: ansi_fg(Color::Named { idx: 10 }),
            green: ansi_fg(Color::Named { idx: 2 }),
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

        let home = st.home.get_or_insert_with(|| var_str(vars, L!("HOME")).to_string());

        let (dir_len, full_dirs) = *st.pwd_cfg.get_or_insert_with(|| {
            let dl = vars
                .get(L!("fish_prompt_pwd_dir_length"))
                .and_then(|v| v.as_string().to_string().parse().ok())
                .unwrap_or(1usize);
            let fd = vars
                .get(L!("fish_prompt_pwd_full_dirs"))
                .and_then(|v| v.as_string().to_string().parse().ok())
                .unwrap_or(1usize);
            (dl, fd)
        });

        let branch = git_branch(&mut st.git, Path::new(&pwd));
        let status_color = if status == 0 { &colors.green } else { &colors.red };

        let mut out = WString::with_capacity(128);
        out.push_utfstr(user_host);
        out.push_utfstr(&colors.brgreen);
        prompt_pwd_into(&mut out, &pwd, home, dir_len, full_dirs);
        out.push_utfstr(&colors.reset);
        if !branch.is_empty() {
            out.push(' ');
            out.push_utfstr(&branch);
        }
        out.push(' ');
        out.push_utfstr(status_color);
        out.push('(');
        push_int(&mut out, status);
        out.push(')');
        out.push_utfstr(&colors.reset);
        out.push_utfstr(L!(" $ "));
        Some(out)
    })
}

/// Whether the native prompt is active (suppresses the right prompt).
pub fn is_native_prompt(parser: &Parser) -> bool {
    parser.vars().get(L!("fish_native_prompt")).is_some()
}

// ---------------------------------------------------------------------------
// prompt_pwd — zero-alloc path shortening, writes directly into output WString
// ---------------------------------------------------------------------------

/// Shorten `$PWD` for display: tilde-contract home, then abbreviate leading
/// directory components to `dir_len` chars (0 = no shortening),
/// keeping the last `full_dirs` components full.
///
/// Writes directly into `out` — no intermediate String or WString allocation.
fn prompt_pwd_into(out: &mut WString, pwd: &str, home: &str, dir_len: usize, full_dirs: usize) {
    // Inline tilde contraction: cheaper than replace_home_directory_with_tilde
    // which does env lookups and expand_tilde internally.
    let tilde = !home.is_empty()
        && pwd.starts_with(home)
        && (pwd.len() == home.len() || pwd.as_bytes()[home.len()] == b'/');

    let remainder = if tilde { &pwd[home.len()..] } else { pwd };

    if dir_len == 0 {
        if tilde {
            out.push('~');
        }
        push_ascii(out, remainder);
        return;
    }

    // Two-pass over '/' segments: count for shorten_up_to, then emit.
    // Tilde replaces the leading empty segment (from the leading '/') with "~".
    // e.g. "/dev/fish" splits to ["", "dev", "fish"]; tilde makes it ["~", "dev", "fish"].
    let n_parts = remainder.split('/').count();
    let shorten_up_to = n_parts.saturating_sub(full_dirs.min(n_parts));

    for (i, part) in remainder.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        let display = if tilde && i == 0 { "~" } else { part };
        if i < shorten_up_to && !display.is_empty() {
            let skip = usize::from(display.as_bytes()[0] == b'.');
            let take = (skip + dir_len).min(display.len());
            push_ascii(out, &display[..take]);
        } else {
            push_ascii(out, display);
        }
    }
}

/// Push an ASCII `&str` directly into a WString (one `char` per byte).
#[inline]
fn push_ascii(out: &mut WString, s: &str) {
    out.reserve(s.len());
    for &b in s.as_bytes() {
        out.push(char::from(b));
    }
}

// ---------------------------------------------------------------------------
// Git branch (native .git/HEAD reader)
// ---------------------------------------------------------------------------

/// Return the current branch by reading `.git/HEAD`.
/// Caches the repo root and pre-joined HEAD path to avoid
/// PathBuf allocation and directory walk on each render.
fn git_branch(cache: &mut Option<(PathBuf, PathBuf)>, cwd: &Path) -> WString {
    // Fast path: still inside the cached repo.
    if let Some((root, head_path)) = cache.as_ref() {
        if cwd.starts_with(root) {
            return read_head_fast(head_path);
        }
    }

    // Slow path: walk up to find .git, then cache.
    match find_git_dir(cwd) {
        Some(git_dir) => {
            let root = git_dir.parent().unwrap_or(cwd).to_path_buf();
            let head_path = git_dir.join("HEAD");
            let branch = read_head_fast(&head_path);
            *cache = Some((root, head_path));
            branch
        }
        None => {
            *cache = None;
            WString::new()
        }
    }
}

/// Walk ancestors of `start` looking for `.git` (dir or worktree file).
/// Uses a single `symlink_metadata` syscall per candidate instead of
/// separate `is_dir()` + `is_file()` (which would be two syscalls each).
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
/// Uses a stack buffer to avoid heap allocation.
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

/// Parse `.git/HEAD` → branch name, full ref, or abbreviated SHA.
/// Uses a stack buffer (`.git/HEAD` is typically ~30–50 bytes).
fn read_head_fast(head_path: &Path) -> WString {
    let mut buf = [0u8; 256];
    let n = match std::fs::File::open(head_path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return WString::new(),
    };
    let line = match std::str::from_utf8(&buf[..n]) {
        Ok(s) => s.trim_end(),
        Err(_) => return WString::new(),
    };
    if let Some(branch) = line.strip_prefix("ref: refs/heads/") {
        wstring_from_ascii(branch)
    } else if let Some(full_ref) = line.strip_prefix("ref: ") {
        wstring_from_ascii(full_ref)
    } else {
        wstring_from_ascii(&line[..line.len().min(8)])
    }
}

/// Convert an ASCII `&str` to a `WString` without generic UTF conversion.
#[inline]
fn wstring_from_ascii(s: &str) -> WString {
    let mut w = WString::with_capacity(s.len());
    for &b in s.as_bytes() {
        w.push(char::from(b));
    }
    w
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn var_str(vars: &impl Environment, name: &'static fish_widestring::wstr) -> WString {
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

/// Write an i32 into a WString without a temporary String allocation.
fn push_int(out: &mut WString, n: i32) {
    if n < 0 {
        out.push('-');
        push_int(out, -n);
        return;
    }
    if n >= 10 {
        push_int(out, n / 10);
    }
    out.push(char::from(b'0' + (n % 10) as u8));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pwd(path: &str, home: &str, dir_len: usize, full_dirs: usize) -> String {
        let mut out = WString::new();
        prompt_pwd_into(&mut out, path, home, dir_len, full_dirs);
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
    fn push_int_formats_correctly() {
        let mut s = WString::new();
        push_int(&mut s, 0);
        assert_eq!(s, "0");

        s.clear();
        push_int(&mut s, 128);
        assert_eq!(s, "128");

        s.clear();
        push_int(&mut s, -1);
        assert_eq!(s, "-1");
    }
}
