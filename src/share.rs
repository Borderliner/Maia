//! Handing a project's files to someone: their text as Markdown code blocks for a chat,
//! an issue or a review, links to them on the repository's website, and lines in its
//! .gitignore. Everything here reads or writes the disk or runs git, so the app calls
//! it from a thread.

use std::path::{Path, PathBuf};

/// A file's text is cut after this much; a chat or an issue takes no more.
pub const FILE_CAP: usize = 100 * 1024;
/// All the blocks together stop growing past this.
pub const TOTAL_CAP: usize = 1024 * 1024;
/// A folder gives at most this many files.
const MAX_FILES: usize = 300;

/// The fence's language for a file, as Markdown renderers name them.
pub fn fence_lang(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "makefile" | "gnumakefile" => return "makefile",
        "dockerfile" | "containerfile" => return "dockerfile",
        "justfile" => return "just",
        "cmakelists.txt" => return "cmake",
        _ => {}
    }
    let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "rs" => "rust",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "py" | "pyi" | "pyw" => "python",
        "rb" => "ruby",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "lua" => "lua",
        "zig" => "zig",
        "dart" => "dart",
        "ex" | "exs" => "elixir",
        "hs" => "haskell",
        "scala" => "scala",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "vue" => "vue",
        "svelte" => "svelte",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" | "svg" => "xml",
        "md" | "markdown" => "markdown",
        "ini" | "cfg" | "conf" => "ini",
        "diff" | "patch" => "diff",
        "nix" => "nix",
        "proto" => "protobuf",
        "graphql" | "gql" => "graphql",
        "tf" => "hcl",
        _ => "",
    }
}

/// What a copy as code blocks came to.
#[derive(Debug, Default, PartialEq)]
pub struct Blocks {
    pub text: String,
    /// Files in the text.
    pub files: usize,
    /// Files in it cut short.
    pub cut: usize,
    /// Left out: (path from the top, why).
    pub skipped: Vec<(String, &'static str)>,
}

impl Blocks {
    /// The toast after copying.
    pub fn note(&self) -> String {
        let n = |k: usize| format!("{k} file{}", if k == 1 { "" } else { "s" });
        let mut s = format!("Copied {} as code blocks", n(self.files));
        if self.cut > 0 {
            s += &format!(", {} cut at {} KB", n(self.cut), FILE_CAP / 1024);
        }
        match self.skipped.as_slice() {
            [] => {}
            [(p, why)] => s += &format!("; left out {p} ({why})"),
            many => s += &format!("; left out {}", n(many.len())),
        }
        s
    }
}

/// The text of `paths` (folders give the files in them that git does not ignore) as
/// Markdown code blocks, each under its path from `root`.
pub fn code_blocks(paths: &[PathBuf], root: &Path) -> Blocks {
    let mut out = Blocks::default();
    let mut files = vec![];
    for p in paths {
        if p.is_dir() {
            files_in(p, &mut files);
        } else {
            files.push(p.clone());
        }
    }
    for f in files {
        let rel = crate::project::relative(&f, root);
        if out.text.len() >= TOTAL_CAP {
            out.skipped.push((rel, "over 1 MB in all"));
            continue;
        }
        let Ok(bytes) = read_head(&f, FILE_CAP + 1) else {
            out.skipped.push((rel, "can't be read"));
            continue;
        };
        let size = std::fs::metadata(&f).map_or(bytes.len() as u64, |m| m.len());
        if bytes.contains(&0) {
            out.skipped.push((rel, "binary"));
            continue;
        }
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        let cut = text.len() > FILE_CAP;
        if cut {
            // Cut at a line's end, never in the middle of a character.
            let mut at = FILE_CAP;
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            let at = text[..at].rfind('\n').map_or(at, |i| i + 1);
            text.truncate(at);
            out.cut += 1;
        }
        if !out.text.is_empty() {
            out.text.push('\n');
        }
        out.text += &rel;
        if cut {
            out.text += &format!(" (the first {} KB of {})", FILE_CAP / 1024, crate::fsx::fmt_size(size));
        }
        out.text.push('\n');
        // A fence longer than any run of backticks inside, so the text cannot close it.
        let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(longest.max(2) + 1);
        out.text += &format!("{fence}{}\n{text}", fence_lang(&f.file_name().unwrap_or_default().to_string_lossy()));
        if !text.is_empty() && !text.ends_with('\n') {
            out.text.push('\n');
        }
        out.text += &fence;
        out.text.push('\n');
        out.files += 1;
    }
    out
}

fn read_head(p: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut v = vec![];
    std::fs::File::open(p)?.take(n as u64).read_to_end(&mut v)?;
    Ok(v)
}

/// The files under `dir` in name order, without hidden ones or what git ignores.
fn files_in(dir: &Path, out: &mut Vec<PathBuf>) {
    use crate::codesearch::{Ignore, ignores_above, parse_gitignore};
    let repo = crate::project::git_root(dir);
    let mut stack = vec![(dir.to_path_buf(), ignores_above(dir, repo.as_deref()))];
    let mut found = vec![];
    while let Some((d, ign)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let kids: Vec<(String, std::fs::FileType, PathBuf)> = rd.flatten().filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.file_type().ok()?, e.path()))).collect();
        let ign = match kids.iter().find(|k| k.0 == ".gitignore" && k.1.is_file()).and_then(|k| std::fs::read_to_string(&k.2).ok()) {
            Some(t) if repo.is_some() => Some(Ignore::new(ign, &d, parse_gitignore(&t))),
            _ => ign,
        };
        for (name, ft, path) in kids {
            if name.starts_with('.') || ft.is_symlink() || ign.as_ref().is_some_and(|i| i.ignored(&path, ft.is_dir())) {
                continue;
            }
            if ft.is_dir() {
                stack.push((path, ign.clone()));
            } else if ft.is_file() {
                found.push(path);
            }
        }
        if found.len() > MAX_FILES * 4 {
            break;
        }
    }
    let mut order: Vec<u32> = (0..found.len() as u32).collect();
    crate::tab::merge_sort(&mut order, &|a: &u32, b: &u32| found[*a as usize].cmp(&found[*b as usize]));
    out.extend(order.into_iter().take(MAX_FILES).map(|i| found[i as usize].clone()));
}

// ------------------------------------------------------------------ web links

/// The kinds of site a repository can be on, by how they write a link to a file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Forge {
    GitHub,
    GitLab,
    /// Gitea and Forgejo, Codeberg among them.
    Gitea,
    Bitbucket,
    SourceHut,
}

/// The website of a remote's URL: "https://github.com/owner/repo" and its kind.
pub fn web_base(url: &str) -> Result<(String, Forge), String> {
    let url = url.trim();
    let folder = || "The remote is a folder, not a website".to_string();
    // scheme://[user@]host[:port]/path, or scp-like [user@]host:path.
    let (scheme, authority, path) = match url.split_once("://") {
        Some((s, r)) => {
            let s = s.to_lowercase();
            if s == "file" {
                return Err(folder());
            }
            let (a, p) = r.split_once('/').unwrap_or((r, ""));
            (s, a, p)
        }
        None => match url.split_once(':') {
            Some((a, p)) if !a.is_empty() && !a.contains('/') => ("ssh".to_string(), a, p),
            _ => return Err(folder()),
        },
    };
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = hostport.split_once(':').unwrap_or((hostport, ""));
    let mut host = host.to_lowercase();
    // A port is kept for a web address; for ssh the website is on the usual one.
    let port = if scheme == "ssh" || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) { String::new() } else { format!(":{port}") };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return Err("The remote has no repository path".into());
    }
    // ssh config aliases for a second account: "github.com-work".
    for known in ["github.com", "gitlab.com", "codeberg.org", "bitbucket.org"] {
        if host.starts_with(known) && host[known.len()..].starts_with(['-', '_']) {
            host = known.to_string();
        }
    }
    let forge = if host.contains("github") {
        Forge::GitHub
    } else if host.contains("gitlab") {
        Forge::GitLab
    } else if host == "codeberg.org" || host.contains("gitea") || host.contains("forgejo") {
        Forge::Gitea
    } else if host.contains("bitbucket") {
        Forge::Bitbucket
    } else if host == "git.sr.ht" {
        Forge::SourceHut
    } else {
        return Err(format!("No web page is known for {host}"));
    };
    let web = if scheme == "http" { "http" } else { "https" };
    Ok((format!("{web}://{host}{port}/{path}"), forge))
}

/// A link to `rel` (from the repository's top, "" for the top itself) at commit `sha`.
pub fn file_url(base: &str, forge: Forge, sha: &str, rel: &str, is_dir: bool, line: Option<u32>) -> String {
    let rel = escape_path(rel);
    let tail = if rel.is_empty() { String::new() } else { format!("/{rel}") };
    let mut s = match (forge, is_dir || rel.is_empty()) {
        (Forge::GitHub, false) => format!("{base}/blob/{sha}{tail}"),
        (Forge::GitHub, true) => format!("{base}/tree/{sha}{tail}"),
        (Forge::GitLab, false) => format!("{base}/-/blob/{sha}{tail}"),
        (Forge::GitLab, true) => format!("{base}/-/tree/{sha}{tail}"),
        (Forge::Gitea, _) => format!("{base}/src/commit/{sha}{tail}"),
        (Forge::Bitbucket, _) => format!("{base}/src/{sha}{tail}"),
        (Forge::SourceHut, _) => format!("{base}/tree/{sha}/item{tail}"),
    };
    if let Some(l) = line.filter(|_| !is_dir) {
        s += &match forge {
            Forge::Bitbucket => format!("#lines-{l}"),
            _ => format!("#L{l}"),
        };
    }
    s
}

/// A path for a URL: everything but letters, digits, `-._~` and `/` percent-encoded.
fn escape_path(p: &str) -> String {
    let mut s = String::with_capacity(p.len());
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            s.push(b as char);
        } else {
            s += &format!("%{b:02X}");
        }
    }
    s
}

fn git_out(root: &Path, args: &[&str]) -> Option<String> {
    let o = crate::gitlog::git(root).args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Links to `paths` on the repository's website at the checked-out commit, one a line;
/// `line` adds a line to the link of the file it belongs to. Ok((links, a caution)).
pub fn web_links(paths: &[(PathBuf, bool)], line: Option<(PathBuf, u32)>) -> Result<(String, Option<String>), String> {
    let first = paths.first().ok_or("Nothing to link to")?;
    let at = if first.1 { first.0.as_path() } else { first.0.parent().unwrap_or(&first.0) };
    let root = crate::project::git_root(at).ok_or("Not in a git repository")?;
    let sha = git_out(&root, &["rev-parse", "HEAD"]).ok_or("The repository has no commits yet")?;
    // The remote the branch pushes to, else origin, else the only one.
    let remote = git_out(&root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"])
        .and_then(|u| u.split_once('/').map(|(r, _)| r.to_string()))
        .or_else(|| {
            let all = git_out(&root, &["remote"])?;
            let names: Vec<&str> = all.lines().collect();
            names.iter().find(|n| **n == "origin").or(names.first()).map(|n| n.to_string())
        })
        .ok_or("The repository has no remote")?;
    let url = git_out(&root, &["remote", "get-url", &remote]).ok_or("The remote has no URL")?;
    let (base, forge) = web_base(&url)?;
    let mut links = vec![];
    let mut new = vec![];
    for (p, is_dir) in paths {
        let rel = crate::project::relative(p, &root);
        let rel = if p == &root { String::new() } else { rel };
        let l = line.as_ref().filter(|(lp, _)| lp == p).map(|x| x.1);
        links.push(file_url(&base, forge, &sha, &rel, *is_dir, l));
        if !*is_dir && git_out(&root, &["ls-files", "--error-unmatch", "--", &rel]).is_none() {
            new.push(rel);
        }
    }
    let pushed = git_out(&root, &["branch", "-r", "--contains", "HEAD"]).is_some_and(|s| !s.is_empty());
    let caution = match (new.as_slice(), pushed) {
        ([one], _) => Some(format!("{one} isn't committed, so its link won't open yet")),
        ([_, ..], _) => Some(format!("{} files aren't committed, so their links won't open yet", new.len())),
        ([], false) => Some("this commit isn't pushed, so the link won't open until it is".to_string()),
        ([], true) => None,
    };
    Ok((links.join("\n"), caution))
}

// ------------------------------------------------------------------ .gitignore

/// The line that ignores just `rel` ("/build/", "/notes.txt"), its special characters
/// escaped so git reads them as themselves.
pub fn ignore_line(rel: &str, is_dir: bool) -> String {
    let mut s = String::from("/");
    for c in rel.chars() {
        if matches!(c, '*' | '?' | '[' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    if s.ends_with(' ') {
        let t = s.trim_end_matches(' ').len();
        s = format!("{}{}", &s[..t], "\\ ".repeat(s.len() - t));
    }
    if is_dir {
        s.push('/');
    }
    s
}

/// The line that ignores every file with `ext` ("*.log").
pub fn ignore_ext_line(ext: &str) -> String {
    let mut s = String::from("*.");
    for c in ext.chars() {
        if matches!(c, '*' | '?' | '[' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// Add `lines` to the .gitignore at the repository's top, leaving out ones it already
/// has. Returns how many were added and the files git still tracks among `paths`.
pub fn add_ignores(root: &Path, lines: &[String], paths: &[PathBuf]) -> Result<(usize, Vec<String>), String> {
    let file = root.join(".gitignore");
    let old = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("Couldn't read .gitignore: {e}")),
    };
    let have: std::collections::HashSet<&str> = old.lines().map(str::trim_end).collect();
    let mut add: Vec<&str> = vec![];
    for l in lines {
        if !have.contains(l.as_str()) && !add.contains(&l.as_str()) {
            add.push(l);
        }
    }
    if !add.is_empty() {
        let mut new = old.clone();
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        for l in &add {
            new += l;
            new.push('\n');
        }
        std::fs::write(&file, new).map_err(|e| format!("Couldn't write .gitignore: {e}"))?;
    }
    // Ignoring changes nothing for a file git already tracks.
    let tracked = paths
        .iter()
        .map(|p| crate::project::relative(p, root))
        .filter(|rel| git_out(root, &["ls-files", "--", rel]).is_some_and(|s| !s.is_empty()))
        .collect();
    Ok((add.len(), tracked))
}

/// The toast after adding to .gitignore.
pub fn ignore_note(lines: &[String], added: usize, tracked: &[String]) -> String {
    let what = if lines.len() == 1 { lines[0].clone() } else { format!("{} lines", lines.len()) };
    let mut s = if added == 0 { format!(".gitignore already has {what}") } else { format!("Added {what} to .gitignore") };
    match tracked {
        [] => {}
        [one] => s += &format!("; git still tracks {one} until it's removed with git rm --cached"),
        many => s += &format!("; git still tracks {} of them until they're removed with git rm --cached", many.len()),
    }
    s
}

/// The extension every one of `items` (path, is a folder) has, when they are all files
/// with the same one.
pub fn shared_ext<'a>(mut items: impl Iterator<Item = (&'a Path, bool)>) -> Option<String> {
    let ext = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).and_then(|n| n.rsplit_once('.').filter(|(stem, e)| !stem.is_empty() && !e.is_empty()).map(|(_, e)| e.to_string()));
    let (p, dir) = items.next()?;
    let first = ext(p).filter(|_| !dir)?;
    items.all(|(p, dir)| !dir && ext(p).as_deref() == Some(&first)).then_some(first)
}
