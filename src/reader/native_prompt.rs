use crate::common::bytes2wcstring;
use crate::env::Environment;
use crate::expand::replace_home_directory_with_tilde;
use crate::parser::Parser;
use crate::terminal::Outputter;
use crate::text_face::TextFace;
use fish_color::Color;
use fish_widestring::{L, WString};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// Everything the native prompt needs, behind a single thread-local.
struct State {
    // Immutable after first prompt render:
    colors: Option<ColorKit>,
    user_host: Option<WString>,
    // Mutable — tracks the repo we're inside:
    git: Option<(PathBuf, PathBuf)>, // (repo_root, git_dir)
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
    let pwd_bytes = pwd_wstr.to_string(); // single WString→String for Path use
    let pwd = prompt_pwd(pwd_wstr, vars as &dyn Environment);
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

        let branch = git_branch(&mut st.git, Path::new(&pwd_bytes));
        let status_color = if status == 0 { &colors.green } else { &colors.red };

        let mut out = WString::new();
        out.push_utfstr(user_host);
        out.push_utfstr(&colors.brgreen);
        out.push_utfstr(&pwd);
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
// prompt_pwd
// ---------------------------------------------------------------------------

/// Shorten `$PWD` for display: tilde-contract home, then abbreviate leading
/// directory components to `$fish_prompt_pwd_dir_length` chars (default 1),
/// keeping the last `$fish_prompt_pwd_full_dirs` components full (default 1).
fn prompt_pwd(pwd_raw: WString, vars: &dyn Environment) -> WString {
    let path = replace_home_directory_with_tilde(pwd_raw, vars).to_string();

    let dir_len: usize = vars
        .get(L!("fish_prompt_pwd_dir_length"))
        .and_then(|v| v.as_string().to_string().parse().ok())
        .unwrap_or(1);

    if dir_len == 0 {
        return WString::from(path);
    }

    let full_dirs: usize = vars
        .get(L!("fish_prompt_pwd_full_dirs"))
        .and_then(|v| v.as_string().to_string().parse().ok())
        .unwrap_or(1);

    let parts: Vec<&str> = path.split('/').collect();
    let shorten_up_to = parts.len().saturating_sub(full_dirs.min(parts.len()));

    let mut out = String::with_capacity(path.len());
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        if i < shorten_up_to && !part.is_empty() {
            let skip = usize::from(part.starts_with('.'));
            out.push_str(&part[..(skip + dir_len).min(part.len())]);
        } else {
            out.push_str(part);
        }
    }
    WString::from(out)
}

// ---------------------------------------------------------------------------
// Git branch (native .git/HEAD reader)
// ---------------------------------------------------------------------------

/// Return the current branch by reading `.git/HEAD`.
/// Reuses the cached git dir when PWD is still under the same repo root.
fn git_branch(cache: &mut Option<(PathBuf, PathBuf)>, cwd: &Path) -> WString {
    // Fast path: still inside the cached repo.
    if let Some((root, git_dir)) = cache.as_ref() {
        if cwd.starts_with(root) {
            return read_head(git_dir);
        }
    }

    // Slow path: walk up to find .git, then cache.
    match find_git_dir(cwd) {
        Some(git_dir) => {
            let root = git_dir.parent().unwrap_or(cwd).to_path_buf();
            let branch = read_head(&git_dir);
            *cache = Some((root, git_dir));
            branch
        }
        None => {
            *cache = None;
            WString::new()
        }
    }
}

/// Walk ancestors of `start` looking for `.git` (dir or worktree file).
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            return resolve_gitdir_file(&dot_git);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Follow the `gitdir:` indirection in a worktree/submodule `.git` file.
fn resolve_gitdir_file(path: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(path).ok()?;
    let target = Path::new(raw.trim().strip_prefix("gitdir: ")?);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        path.parent()?.join(target)
    })
}

/// Parse `.git/HEAD` → branch name, full ref, or abbreviated SHA.
fn read_head(git_dir: &Path) -> WString {
    let Ok(contents) = std::fs::read_to_string(git_dir.join("HEAD")) else {
        return WString::new();
    };
    let line = contents.trim_end();
    if let Some(branch) = line.strip_prefix("ref: refs/heads/") {
        WString::from(branch)
    } else if let Some(full_ref) = line.strip_prefix("ref: ") {
        WString::from(full_ref)
    } else {
        WString::from(&line[..line.len().min(8)])
    }
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
    use crate::tests::prelude::TestEnvironment;

    fn env(entries: &[(&str, &str)]) -> TestEnvironment {
        let mut e = TestEnvironment::new();
        for &(k, v) in entries {
            e.vars.insert(WString::from(k), WString::from(v));
        }
        e
    }

    #[test]
    fn pwd_shortens_middle_components() {
        let v = env(&[("HOME", "/home/u"), ("PWD", "/home/u/dev/fish-shell")]);
        assert_eq!(prompt_pwd(WString::from("/home/u/dev/fish-shell"), &v), "~/d/fish-shell");
    }

    #[test]
    fn pwd_no_shorten_when_zero() {
        let v = env(&[
            ("HOME", "/home/u"),
            ("PWD", "/home/u/dev/fish-shell"),
            ("fish_prompt_pwd_dir_length", "0"),
        ]);
        assert_eq!(prompt_pwd(WString::from("/home/u/dev/fish-shell"), &v), "~/dev/fish-shell");
    }

    #[test]
    fn pwd_root() {
        let v = env(&[("HOME", "/home/u"), ("PWD", "/")]);
        assert_eq!(prompt_pwd(WString::from("/"), &v), "/");
    }

    #[test]
    fn pwd_preserves_leading_dot() {
        let v = env(&[("HOME", "/home/u"), ("PWD", "/home/u/.config/fish")]);
        assert_eq!(prompt_pwd(WString::from("/home/u/.config/fish"), &v), "~/.c/fish");
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
