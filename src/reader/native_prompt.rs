use crate::common::bytes2wcstring;
use crate::env::Environment;
use crate::expand::replace_home_directory_with_tilde;
use crate::parser::Parser;
use crate::terminal::Outputter;
use crate::text_face::{TextFace, parse_text_face};
use fish_color::Color;
use fish_widestring::{L, WString};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// Cached values that don't change during a session.
struct PromptCache {
    /// `$USER@$hostname ` prefix — computed once.
    user_host_prefix: Option<WString>,
    /// ANSI escapes — computed once.
    color_on: WString,
    color_off: WString,
    color_green: WString,
    color_red: WString,
    /// Cached git dir for a given PWD prefix, so we skip the walk when PWD
    /// is under the same repo.
    cached_git_dir: Option<PathBuf>,
    cached_git_repo_root: Option<PathBuf>,
}

thread_local! {
    static CACHE: RefCell<PromptCache> = RefCell::new(PromptCache {
        user_host_prefix: None,
        color_on: WString::new(),
        color_off: WString::new(),
        color_green: WString::new(),
        color_red: WString::new(),
        cached_git_dir: None,
        cached_git_repo_root: None,
    });
}

/// If `$fish_native_prompt` is set, generate the left prompt natively in Rust.
/// Returns `None` if the native prompt is not enabled.
///
/// The native prompt replicates:
/// ```fish
/// printf '%s@%s %s%s%s %s $ ' \
///     $USER $hostname \
///     (set_color $fish_color_cwd) (prompt_pwd) (set_color normal) \
///     "$(/usr/bin/git rev-parse --abbrev-ref HEAD 2>/dev/null)"
/// ```
pub fn try_native_left_prompt(parser: &Parser) -> Option<WString> {
    let vars = parser.vars();
    vars.get(L!("fish_native_prompt"))?;

    let pwd_raw = vars
        .get(L!("PWD"))
        .map(|v| v.as_string())
        .unwrap_or_default();

    let pwd = native_prompt_pwd(vars as &dyn Environment);

    CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();

        // Cache $USER@$hostname — these never change during a session.
        if cache.user_host_prefix.is_none() {
            let user = vars
                .get(L!("USER"))
                .map(|v| v.as_string())
                .unwrap_or_default();
            let hostname = vars
                .get(L!("hostname"))
                .map(|v| v.as_string())
                .unwrap_or_default();
            let mut p = WString::new();
            p.push_utfstr(&user);
            p.push('@');
            p.push_utfstr(&hostname);
            p.push(' ');
            cache.user_host_prefix = Some(p);
        }

        // Cache color escapes — computed once.
        if cache.color_off.is_empty() {
            cache.color_on = generate_color_escape(&[WString::from("brgreen")]);
            cache.color_off = generate_reset_escape();
            cache.color_green = generate_color_escape(&[WString::from("green")]);
            cache.color_red = generate_color_escape(&[WString::from("red")]);
        }

        // Git branch — use cached git dir if PWD is still under the same repo root.
        let pwd_string = pwd_raw.to_string();
        let pwd_path = Path::new(&pwd_string);
        let git_branch = get_git_branch_cached(&mut cache, pwd_path);

        // Exit status: green 0 or red N.
        let statuses = parser.get_last_statuses();
        let status = statuses.status;

        let mut result = WString::new();
        result.push_utfstr(cache.user_host_prefix.as_ref().unwrap());
        result.push_utfstr(&cache.color_on);
        result.push_utfstr(&pwd);
        result.push_utfstr(&cache.color_off);
        result.push(' ');
        result.push_utfstr(&git_branch);
        result.push(' ');
        if status == 0 {
            result.push_utfstr(&cache.color_green);
        } else {
            result.push_utfstr(&cache.color_red);
        }
        result.push('(');
        result.push_utfstr(&WString::from(status.to_string()));
        result.push(')');
        result.push_utfstr(&cache.color_off);
        result.push_utfstr(L!(" $ "));

        Some(result)
    })
}

/// Returns true if the native prompt is enabled.
pub fn is_native_prompt(parser: &Parser) -> bool {
    parser.vars().get(L!("fish_native_prompt")).is_some()
}

/// Native implementation of `prompt_pwd`.
/// Replaces home with `~`, then shortens directory components to
/// `$fish_prompt_pwd_dir_length` characters (default 1), keeping the last
/// `$fish_prompt_pwd_full_dirs` components at full length (default 1).
fn native_prompt_pwd(vars: &dyn Environment) -> WString {
    let pwd = vars
        .get(L!("PWD"))
        .map(|v| v.as_string())
        .unwrap_or_default();

    let path = replace_home_directory_with_tilde(pwd, vars);

    let dir_length: usize = vars
        .get(L!("fish_prompt_pwd_dir_length"))
        .and_then(|v| v.as_string().to_string().parse().ok())
        .unwrap_or(1);

    let full_dirs: usize = vars
        .get(L!("fish_prompt_pwd_full_dirs"))
        .and_then(|v| v.as_string().to_string().parse().ok())
        .unwrap_or(1);

    if dir_length == 0 {
        return path;
    }

    let path_str: Vec<char> = path.to_string().chars().collect();
    let path_string: String = path_str.iter().collect();
    let components: Vec<&str> = path_string.split('/').collect();
    let total = components.len();

    if total == 0 {
        return path;
    }

    // Number of components to keep at full length from the end
    let full_count = full_dirs.min(total);
    let shorten_count = total - full_count;

    let mut result_parts: Vec<String> = Vec::with_capacity(total);
    for (i, comp) in components.iter().enumerate() {
        if i < shorten_count && !comp.is_empty() {
            // Shorten this component: keep leading dot + dir_length chars
            let chars: Vec<char> = comp.chars().collect();
            let start = if chars.first() == Some(&'.') { 1 } else { 0 };
            let keep = (start + dir_length).min(chars.len());
            result_parts.push(chars[..keep].iter().collect());
        } else {
            result_parts.push(comp.to_string());
        }
    }

    WString::from(result_parts.join("/"))
}

/// Generate ANSI escape sequence from color arguments (same format as `set_color` args).
/// Handles list variables like `["green", "--bold"]` or `["008000", "--theme=none"]`.
fn generate_color_escape(args: &[WString]) -> WString {
    if args.is_empty() {
        return WString::new();
    }
    let face = parse_text_face(args);
    let mut outp = Outputter::new_buffering();
    outp.set_text_face(TextFace::new(
        face.fg.unwrap_or(Color::None),
        face.bg.unwrap_or(Color::None),
        face.underline_color.unwrap_or(Color::None),
        face.style.unwrap_or_default(),
    ));
    bytes2wcstring(outp.contents())
}

/// Generate ANSI reset escape sequence (equivalent to `set_color normal`).
fn generate_reset_escape() -> WString {
    let mut outp = Outputter::new_buffering();
    outp.reset_text_face();
    bytes2wcstring(outp.contents())
}

/// Get the current git branch, using cached git dir when PWD is under the same repo.
/// Falls back to a full directory walk when PWD leaves the cached repo root.
fn get_git_branch_cached(cache: &mut PromptCache, cwd: &Path) -> WString {
    // Fast path: if PWD is still under the cached repo root, just re-read HEAD.
    if let (Some(repo_root), Some(git_dir)) = (&cache.cached_git_repo_root, &cache.cached_git_dir)
    {
        if cwd.starts_with(repo_root) {
            return read_git_head(git_dir);
        }
    }

    // Slow path: walk up to find .git, then cache the result.
    match find_git_dir(cwd) {
        Some(git_dir) => {
            // The repo root is the parent of the .git dir (or the dir containing the .git file).
            let repo_root = git_dir.parent().unwrap_or(cwd).to_path_buf();
            let branch = read_git_head(&git_dir);
            cache.cached_git_dir = Some(git_dir);
            cache.cached_git_repo_root = Some(repo_root);
            branch
        }
        None => {
            cache.cached_git_dir = None;
            cache.cached_git_repo_root = None;
            WString::new()
        }
    }
}

/// Walk up from `start` looking for a `.git` entry (directory or file).
/// Returns the resolved git directory path (following worktree indirection).
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            // Worktree or submodule: `.git` is a file containing `gitdir: <path>`
            return resolve_gitdir_file(&dot_git);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Read a `.git` file (worktree/submodule) and resolve the `gitdir:` path.
fn resolve_gitdir_file(dot_git_file: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(dot_git_file).ok()?;
    let line = contents.trim();
    let path_str = line.strip_prefix("gitdir: ")?;
    let path = Path::new(path_str);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        // Relative to the directory containing the `.git` file
        let parent = dot_git_file.parent()?;
        Some(parent.join(path))
    }
}

/// Read HEAD from a git directory and return the branch name.
/// Returns empty string if not in a git repo or on detached HEAD.
fn read_git_head(git_dir: &Path) -> WString {
    let head_path = git_dir.join("HEAD");
    let contents = match std::fs::read_to_string(&head_path) {
        Ok(c) => c,
        Err(_) => return WString::new(),
    };
    let line = contents.trim_end_matches('\n');
    if let Some(ref_path) = line.strip_prefix("ref: refs/heads/") {
        WString::from(ref_path)
    } else if let Some(ref_path) = line.strip_prefix("ref: ") {
        // Unusual ref (not a local branch), show full ref
        WString::from(ref_path)
    } else {
        // Detached HEAD — show abbreviated SHA (first 8 chars)
        let abbrev = if line.len() > 8 { &line[..8] } else { line };
        WString::from(abbrev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::prelude::TestEnvironment;

    fn make_env(entries: &[(&str, &str)]) -> TestEnvironment {
        let mut env = TestEnvironment::new();
        for &(k, v) in entries {
            env.vars.insert(WString::from(k), WString::from(v));
        }
        env
    }

    #[test]
    fn test_native_prompt_pwd_home() {
        let vars = make_env(&[
            ("HOME", "/home/testuser"),
            ("PWD", "/home/testuser/dev/fish-shell"),
        ]);
        let result = native_prompt_pwd(&vars);
        assert_eq!(result, WString::from("~/d/fish-shell"));
    }

    #[test]
    fn test_native_prompt_pwd_no_shorten() {
        let vars = make_env(&[
            ("HOME", "/home/testuser"),
            ("PWD", "/home/testuser/dev/fish-shell"),
            ("fish_prompt_pwd_dir_length", "0"),
        ]);
        let result = native_prompt_pwd(&vars);
        assert_eq!(result, WString::from("~/dev/fish-shell"));
    }

    #[test]
    fn test_native_prompt_pwd_root() {
        let vars = make_env(&[("HOME", "/home/testuser"), ("PWD", "/")]);
        let result = native_prompt_pwd(&vars);
        assert_eq!(result, WString::from("/"));
    }

    #[test]
    fn test_native_prompt_pwd_dotfile() {
        let vars = make_env(&[
            ("HOME", "/home/testuser"),
            ("PWD", "/home/testuser/.config/fish"),
        ]);
        let result = native_prompt_pwd(&vars);
        assert_eq!(result, WString::from("~/.c/fish"));
    }
}
