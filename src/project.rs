//! Where a project starts: the folder that holds its repository or its manifest. Code
//! search looks from there, tasks are read from the folders on the way up to it, and
//! relative paths are written from it.

use std::path::{Path, PathBuf};

/// Files that make a folder the top of a project when there is no repository.
pub const MARKERS: &[&str] = &[
    "Cargo.toml", "package.json", "deno.json", "deno.jsonc", "pyproject.toml", "setup.py", "go.mod", "composer.json", "pom.xml",
    "build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts", "CMakeLists.txt", "meson.build", "build.zig", "mix.exs",
    "pubspec.yaml", "stack.yaml", "Gemfile", "Makefile", "makefile", "GNUmakefile", "justfile", "Justfile", "Taskfile.yml",
];

/// The repository `dir` is in: the nearest folder with a `.git` (a folder, or a file in a
/// worktree or submodule). The home folder and / never count unless `dir` is one of
/// them: a home kept in git for its dotfiles is not a project to search all of.
pub fn git_root(dir: &Path) -> Option<PathBuf> {
    let home = crate::fsx::home();
    for d in dir.ancestors() {
        if d != dir && (d == home || d.parent().is_none()) {
            return None;
        }
        if std::fs::symlink_metadata(d.join(".git")).is_ok() {
            return Some(d.to_path_buf());
        }
    }
    None
}

/// The top of the project `dir` is in: its repository, else the nearest folder with a
/// manifest, else `dir` itself.
pub fn root(dir: &Path) -> PathBuf {
    if let Some(r) = git_root(dir) {
        return r;
    }
    let home = crate::fsx::home();
    for d in dir.ancestors() {
        if d != dir && (d == home || d.parent().is_none()) {
            break;
        }
        if MARKERS.iter().any(|m| d.join(m).is_file()) {
            return d.to_path_buf();
        }
    }
    dir.to_path_buf()
}

/// `path` written from `root` ("src/main.rs"), or whole when it is not under it.
pub fn relative(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
        _ => path.to_string_lossy().into_owned(),
    }
}

/// `path:line`, the way compilers and search tools print a place in a file.
pub fn with_line(path: &str, line: u32) -> String {
    format!("{path}:{line}")
}
