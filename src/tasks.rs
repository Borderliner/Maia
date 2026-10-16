//! A project's tasks, read from its manifests: package.json scripts (run with the package
//! manager its lockfile names), Cargo's build, check, test and runs, Makefile targets,
//! justfile recipes, pyproject scripts, deno and composer scripts, Taskfile tasks and Go's
//! commands. A task runs in the user's terminal, in the folder its manifest is in.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub name: String,
    pub argv: Vec<String>,
    /// What it runs or what its comment says, shown dim beside it.
    pub detail: String,
}

/// The tasks one manifest gives.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub dir: PathBuf,
    /// The manifest, and the tool that runs it when that isn't obvious.
    pub source: String,
    pub tasks: Vec<Task>,
}

fn task(name: &str, argv: &[&str], detail: impl Into<String>) -> Task {
    Task { name: name.into(), argv: argv.iter().map(|s| s.to_string()).collect(), detail: detail.into() }
}

fn read(dir: &Path, name: &str) -> Option<String> {
    let p = dir.join(name);
    // Manifests are small; anything huge is not one.
    if std::fs::metadata(&p).ok()?.len() > 4 << 20 {
        return None;
    }
    std::fs::read_to_string(p).ok()
}

/// Every group from `start` up to the top of its project, nearest first.
pub fn detect(start: &Path) -> Vec<Group> {
    let top = crate::project::root(start);
    let mut out = vec![];
    for d in start.ancestors() {
        out.extend(detect_dir(d));
        if d == top || !d.starts_with(&top) {
            break;
        }
    }
    out
}

/// The groups the manifests in `dir` give.
pub fn detect_dir(dir: &Path) -> Vec<Group> {
    let mut out = vec![];
    let mut push = |source: String, tasks: Vec<Task>| {
        if !tasks.is_empty() {
            out.push(Group { dir: dir.to_path_buf(), source, tasks });
        }
    };
    if let Some(t) = read(dir, "package.json") {
        let pm = package_manager(dir, &t);
        push(format!("package.json · {pm}"), package_scripts(&t, pm));
    }
    for name in ["deno.json", "deno.jsonc"] {
        if let Some(t) = read(dir, name) {
            push(name.into(), deno_tasks(&t));
            break;
        }
    }
    if let Some(t) = read(dir, "Cargo.toml") {
        push("Cargo.toml".into(), cargo_tasks(dir, &t));
    }
    for name in ["justfile", "Justfile", ".justfile"] {
        if let Some(t) = read(dir, name) {
            push(name.into(), just_recipes(&t));
            break;
        }
    }
    for name in ["GNUmakefile", "makefile", "Makefile"] {
        if let Some(t) = read(dir, name) {
            push(name.into(), make_targets(&t));
            break;
        }
    }
    if let Some(t) = read(dir, "pyproject.toml") {
        for (source, tasks) in pyproject_scripts(dir, &t) {
            push(source, tasks);
        }
    }
    for name in ["Taskfile.yml", "Taskfile.yaml", "taskfile.yml"] {
        if let Some(t) = read(dir, name) {
            push(name.into(), taskfile_tasks(&t));
            break;
        }
    }
    if let Some(t) = read(dir, "composer.json") {
        push("composer.json".into(), json_scripts(&t, "scripts", &["composer", "run-script"]));
    }
    if dir.join("go.mod").is_file() {
        let mut v = vec![task("build", &["go", "build", "./..."], "go build ./..."), task("test", &["go", "test", "./..."], "go test ./..."), task("vet", &["go", "vet", "./..."], "go vet ./...")];
        if dir.join("main.go").is_file() {
            v.insert(1, task("run", &["go", "run", "."], "go run ."));
        }
        push("go.mod".into(), v);
    }
    out
}

/// The package manager a JavaScript project uses: the one `packageManager` names, else
/// the one whose lockfile is there, else npm.
pub fn package_manager(dir: &Path, package_json: &str) -> &'static str {
    let named = serde_json::from_str::<serde_json::Value>(package_json).ok().and_then(|v| v.get("packageManager")?.as_str().map(String::from)).unwrap_or_default();
    for pm in ["pnpm", "yarn", "bun", "npm"] {
        if named.starts_with(&format!("{pm}@")) || named == pm {
            return pm;
        }
    }
    for (lock, pm) in [("pnpm-lock.yaml", "pnpm"), ("yarn.lock", "yarn"), ("bun.lockb", "bun"), ("bun.lock", "bun"), ("package-lock.json", "npm")] {
        if dir.join(lock).is_file() {
            return pm;
        }
    }
    "npm"
}

pub fn package_scripts(text: &str, pm: &str) -> Vec<Task> {
    json_scripts(text, "scripts", &[pm, "run"])
}

/// The string (or command) values under `key`, each run as `prefix name`. Keys keep the
/// manifest's order.
fn json_scripts(text: &str, key: &str, prefix: &[&str]) -> Vec<Task> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return vec![] };
    let Some(m) = v.get(key).and_then(|s| s.as_object()) else { return vec![] };
    let names = object_keys(text, key);
    let mut out: Vec<Task> = vec![];
    let mut add = |name: &str, val: &serde_json::Value| {
        let detail = match val {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" && "),
            serde_json::Value::Object(o) => o.get("command").and_then(|c| c.as_str()).unwrap_or_default().to_string(),
            _ => String::new(),
        };
        let mut argv: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
        argv.push(name.to_string());
        out.push(Task { name: name.into(), argv, detail });
    };
    for n in &names {
        if let Some(val) = m.get(n) {
            add(n, val);
        }
    }
    // serde_json's map is sorted: whatever the order scan missed comes after.
    for (n, val) in m {
        if !names.contains(n) {
            add(n, val);
        }
    }
    out
}

/// The keys of the object under top-level `key`, in the order they are written.
fn object_keys(text: &str, key: &str) -> Vec<String> {
    let b = text.as_bytes();
    let (mut depth, mut i, mut inside) = (0usize, 0usize, false);
    let (mut top, mut keys) = (String::new(), vec![]);
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i + 1;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                let s = String::from_utf8_lossy(&b[start..i.min(b.len())]).into_owned();
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if b.get(j) == Some(&b':') {
                    if depth == 1 {
                        top = s;
                    } else if depth == 2 && inside {
                        keys.push(s);
                    }
                }
            }
            b'{' | b'[' => {
                depth += 1;
                inside |= depth == 2 && b[i] == b'{' && top == key;
            }
            b'}' | b']' => {
                if depth == 2 && inside {
                    return keys;
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        i += 1;
    }
    keys
}

pub fn deno_tasks(text: &str) -> Vec<Task> {
    json_scripts(&strip_json_comments(text), "tasks", &["deno", "task"])
}

/// JSON with `//` and `/* */` comments taken out (outside strings).
fn strip_json_comments(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                out.push(b'"');
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        out.push(b[i]);
                        i += 1;
                    }
                    out.push(b[i]);
                    i += 1;
                }
                if i < b.len() {
                    out.push(b'"');
                }
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `key = value` lines of a TOML file with the table they are in (`[[bin]]` tables are
/// numbered: "bin#0", "bin#1"). Enough TOML for manifests: values spread over several
/// lines are passed over whole.
pub fn toml_entries(text: &str) -> Vec<(String, String, String)> {
    let mut out = vec![];
    let mut table = String::new();
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    let mut lines = text.lines();
    while let Some(raw) = lines.next() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix("[[").and_then(|l| l.split("]]").next()) {
            let name = name.trim().to_string();
            let n = counts.entry(name.clone()).or_insert(0);
            table = format!("{name}#{n}");
            *n += 1;
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.split(']').next()) {
            table = name.trim().to_string();
            continue;
        }
        let Some((k, v)) = split_key(line) else { continue };
        let mut value = v.trim().to_string();
        // A value that opens a multi-line string or array goes on until it closes.
        if value.starts_with("\"\"\"") || value.starts_with("'''") {
            let q = &value[..3].to_string();
            if value.len() < 6 || !value[3..].contains(q.as_str()) {
                for more in lines.by_ref() {
                    value.push('\n');
                    value.push_str(more);
                    if more.contains(q.as_str()) {
                        break;
                    }
                }
            }
        } else {
            let mut depth = bracket_depth(&value);
            while depth > 0 {
                let Some(more) = lines.next() else { break };
                depth += bracket_depth(more);
                value.push(' ');
                value.push_str(more.trim());
            }
        }
        out.push((table.clone(), k, value));
    }
    out
}

/// A TOML line's key (unquoted) and the value after `=`.
fn split_key(line: &str) -> Option<(String, &str)> {
    if let Some(rest) = line.strip_prefix('"').or_else(|| line.strip_prefix('\'')) {
        let q = if line.starts_with('"') { '"' } else { '\'' };
        let end = rest.find(q)?;
        let after = rest[end + 1..].trim_start().strip_prefix('=')?;
        return Some((rest[..end].to_string(), after));
    }
    let (k, v) = line.split_once('=')?;
    let k = k.trim();
    if k.is_empty() || !k.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return None;
    }
    Some((k.to_string(), v))
}

/// How many more brackets `s` opens than it closes, outside strings.
fn bracket_depth(s: &str) -> i32 {
    let (mut d, mut q) = (0, None::<char>);
    for c in s.chars() {
        match (q, c) {
            (None, '"' | '\'') => q = Some(c),
            (Some(x), _) if x == c => q = None,
            (None, '[' | '{') => d += 1,
            (None, ']' | '}') => d -= 1,
            (None, '#') => break,
            _ => {}
        }
    }
    d
}

/// A TOML string value without its quotes.
pub fn toml_str(v: &str) -> Option<String> {
    let v = v.trim();
    let v = v.split(" #").next().unwrap_or(v).trim();
    for q in ['"', '\''] {
        if let Some(s) = v.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return Some(s.to_string());
        }
    }
    None
}

/// Rust programs in `dir/sub`: `name.rs` files and `name/main.rs` folders, by name.
fn rust_files(dir: &Path, sub: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir.join(sub))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let p = e.path();
                    match name.strip_suffix(".rs") {
                        Some(stem) if p.is_file() => Some(stem.to_string()),
                        _ if p.join("main.rs").is_file() => Some(name),
                        _ => None,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn cargo_tasks(dir: &Path, text: &str) -> Vec<Task> {
    let entries = toml_entries(text);
    let has_table = |t: &str| entries.iter().any(|e| e.0 == t || e.0.starts_with(&format!("{t}."))) || text.lines().any(|l| l.trim() == format!("[{t}]"));
    let package = has_table("package");
    if !package && !has_table("workspace") {
        return vec![];
    }
    let mut v = vec![task("build", &["cargo", "build"], "cargo build"), task("check", &["cargo", "check"], "cargo check"), task("test", &["cargo", "test"], "cargo test")];
    if !package {
        return v;
    }
    let pkg = entries.iter().find(|e| e.0 == "package" && e.1 == "name").and_then(|e| toml_str(&e.2)).unwrap_or_default();
    let named = |kind: &str| -> Vec<String> { entries.iter().filter(|e| e.0.starts_with(&format!("{kind}#")) && e.1 == "name").filter_map(|e| toml_str(&e.2)).collect() };
    let mut bins = named("bin");
    if dir.join("src/main.rs").is_file() && !bins.contains(&pkg) && !pkg.is_empty() {
        bins.insert(0, pkg.clone());
    }
    for b in rust_files(dir, "src/bin") {
        if !bins.contains(&b) {
            bins.push(b);
        }
    }
    match bins.as_slice() {
        [] => {}
        [_] => v.insert(1, task("run", &["cargo", "run"], "cargo run")),
        _ => {
            for (k, b) in bins.iter().enumerate() {
                v.insert(1 + k, Task { name: format!("run {b}"), argv: vec!["cargo".into(), "run".into(), "--bin".into(), b.clone()], detail: format!("cargo run --bin {b}") });
            }
        }
    }
    let mut examples = named("example");
    for e in rust_files(dir, "examples") {
        if !examples.contains(&e) {
            examples.push(e);
        }
    }
    for e in examples {
        v.push(Task { name: format!("example {e}"), argv: vec!["cargo".into(), "run".into(), "--example".into(), e.clone()], detail: format!("cargo run --example {e}") });
    }
    v
}

/// A Makefile's targets, in order: not special ones (.PHONY), pattern rules or
/// variables; the comment just above or after a target is its detail.
pub fn make_targets(text: &str) -> Vec<Task> {
    let mut out: Vec<Task> = vec![];
    let mut comment = String::new();
    for line in text.lines() {
        if line.starts_with('\t') || line.starts_with(' ') {
            comment.clear();
            continue;
        }
        if let Some(c) = line.strip_prefix('#') {
            comment = c.trim_start_matches('#').trim().to_string();
            continue;
        }
        let Some(colon) = line.find(':') else {
            comment.clear();
            continue;
        };
        let (head, rest) = (&line[..colon], &line[colon + 1..]);
        // `a := b`, `a ::= b`, `a = b:c`: variables, not rules.
        if rest.starts_with('=') || rest.starts_with(":=") || head.contains('=') || head.trim_start().starts_with("export ") {
            comment.clear();
            continue;
        }
        let after = rest.split("##").nth(1).map(|s| s.trim().to_string());
        for t in head.split_whitespace() {
            if t.starts_with('.') || t.contains('%') || t.contains('$') || t.contains('(') || out.iter().any(|o| o.name == t) {
                continue;
            }
            let detail = after.clone().unwrap_or_else(|| comment.clone());
            out.push(Task { name: t.into(), argv: vec!["make".into(), t.into()], detail });
        }
        comment.clear();
    }
    out
}

/// A justfile's public recipes, in order, with the comment above each (as `just --list`
/// shows it) or else the parameters it takes.
pub fn just_recipes(text: &str) -> Vec<Task> {
    let mut out: Vec<Task> = vec![];
    let (mut comment, mut private) = (String::new(), false);
    for line in text.lines() {
        if line.trim().is_empty() {
            comment.clear();
            private = false;
            continue;
        }
        if line.starts_with([' ', '\t']) {
            continue;
        }
        if let Some(c) = line.strip_prefix('#') {
            if !c.starts_with('!') {
                comment = c.trim().to_string();
            }
            continue;
        }
        if line.starts_with('[') {
            private |= line.contains("private");
            continue;
        }
        let l = line.strip_prefix('@').unwrap_or(line);
        let name: String = l.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
        let rest = &l[name.len()..];
        let keyword = matches!(name.as_str(), "set" | "alias" | "export" | "import" | "mod" | "unexport") && rest.starts_with(' ');
        let (doc, hidden) = (std::mem::take(&mut comment), std::mem::replace(&mut private, false));
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit() || c == '-') || keyword || rest.trim_start().starts_with(":=") {
            continue;
        }
        // The parameters run up to the first colon outside quotes that isn't `:=`.
        let (mut quote, mut colon) = (None::<char>, None);
        for (i, c) in rest.char_indices() {
            match (quote, c) {
                (None, '"' | '\'') => quote = Some(c),
                (Some(q), _) if q == c => quote = None,
                (None, ':') if !rest[i + 1..].starts_with('=') => {
                    colon = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let Some(colon) = colon else { continue };
        if name.starts_with('_') || hidden || out.iter().any(|o| o.name == name) {
            continue;
        }
        let params = rest[..colon].split_whitespace().collect::<Vec<_>>().join(" ");
        let detail = if doc.is_empty() { params } else { doc };
        out.push(Task { name: name.clone(), argv: vec!["just".into(), name], detail });
    }
    out
}

/// pyproject's script tables, each with the tool that runs it.
pub fn pyproject_scripts(dir: &Path, text: &str) -> Vec<(String, Vec<Task>)> {
    let entries = toml_entries(text);
    let runner = if dir.join("uv.lock").is_file() {
        Some("uv")
    } else if dir.join("poetry.lock").is_file() {
        Some("poetry")
    } else if dir.join("pdm.lock").is_file() {
        Some("pdm")
    } else {
        None
    };
    let mut out = vec![];
    for (table, tool, prefix) in [
        ("project.scripts", runner.unwrap_or(""), runner.map(|r| vec![r, "run"]).unwrap_or_default()),
        ("tool.poetry.scripts", "poetry", vec!["poetry", "run"]),
        ("tool.pdm.scripts", "pdm", vec!["pdm", "run"]),
        ("tool.rye.scripts", "rye", vec!["rye", "run"]),
        ("tool.hatch.envs.default.scripts", "hatch", vec!["hatch", "run"]),
        ("tool.poe.tasks", "poe", vec!["poe"]),
    ] {
        let tasks: Vec<Task> = entries
            .iter()
            .filter(|e| e.0 == table)
            .map(|(_, k, v)| {
                let mut argv: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
                argv.push(k.clone());
                let detail = toml_str(v).unwrap_or_else(|| v.trim().to_string());
                Task { name: k.clone(), argv, detail }
            })
            .collect();
        if !tasks.is_empty() {
            out.push((if tool.is_empty() { "pyproject.toml".to_string() } else { format!("pyproject.toml · {tool}") }, tasks));
        }
    }
    out
}

/// go-task's Taskfile: the keys under `tasks:`, with their `desc`.
pub fn taskfile_tasks(text: &str) -> Vec<Task> {
    let mut out: Vec<Task> = vec![];
    let mut inside = false;
    let mut indent = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let lead = line.len() - line.trim_start().len();
        if lead == 0 {
            inside = line.trim_end() == "tasks:";
            continue;
        }
        if !inside {
            continue;
        }
        let ind = *indent.get_or_insert(lead);
        let body = line.trim();
        if lead == ind {
            if let Some(name) = body.strip_suffix(':').or_else(|| body.split_once(": ").map(|x| x.0)) {
                let name = name.trim_matches(['"', '\'']);
                out.push(Task { name: name.into(), argv: vec!["task".into(), name.into()], detail: String::new() });
            }
        } else if lead > ind {
            if let (Some(d), Some(t)) = (body.strip_prefix("desc:"), out.last_mut()) {
                if t.detail.is_empty() {
                    t.detail = d.trim().trim_matches(['"', '\'']).to_string();
                }
            }
        }
    }
    out
}
