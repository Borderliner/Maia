//! Opening a file at a line in the user's editor. The Editor option names one (or a
//! command with `{file}` and `{line}` in it); empty means `$VISUAL`, then `$EDITOR`, then
//! the first editor installed from `KNOWN`. Each editor is given the line its own way, and
//! one that runs in a terminal is started in the user's terminal.

use std::path::Path;

/// How an editor is told the line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Syntax {
    /// `code -g file:line`
    Goto,
    /// `zed file:line`
    Colon,
    /// `nvim +line file`
    Plus,
    /// `idea --line line file` (JetBrains IDEs, Kate)
    LineFlag,
}

pub struct Known {
    pub name: &'static str,
    pub label: &'static str,
    /// The program's names, as packages install it.
    pub bins: &'static [&'static str],
    pub syntax: Syntax,
    /// Runs in a terminal.
    pub terminal: bool,
}

const fn gui(name: &'static str, label: &'static str, bins: &'static [&'static str], syntax: Syntax) -> Known {
    Known { name, label, bins, syntax, terminal: false }
}

const fn tui(name: &'static str, label: &'static str, bins: &'static [&'static str], syntax: Syntax) -> Known {
    Known { name, label, bins, syntax, terminal: true }
}

/// Editors that take a line, in the order Auto looks for them: the ones with a window of
/// their own first, then those that run in a terminal.
pub const KNOWN: &[Known] = &[
    gui("code", "VS Code", &["code", "code-insiders"], Syntax::Goto),
    gui("codium", "VSCodium", &["codium", "vscodium"], Syntax::Goto),
    gui("cursor", "Cursor", &["cursor"], Syntax::Goto),
    gui("zed", "Zed", &["zed", "zeditor", "zedit"], Syntax::Colon),
    gui("subl", "Sublime Text", &["subl", "sublime_text"], Syntax::Colon),
    gui("idea", "IntelliJ IDEA", &["idea", "intellij-idea-ultimate-edition", "intellij-idea-community-edition"], Syntax::LineFlag),
    gui("clion", "CLion", &["clion"], Syntax::LineFlag),
    gui("rustrover", "RustRover", &["rustrover"], Syntax::LineFlag),
    gui("pycharm", "PyCharm", &["pycharm", "pycharm-professional", "pycharm-community"], Syntax::LineFlag),
    gui("webstorm", "WebStorm", &["webstorm"], Syntax::LineFlag),
    gui("goland", "GoLand", &["goland"], Syntax::LineFlag),
    gui("phpstorm", "PhpStorm", &["phpstorm"], Syntax::LineFlag),
    gui("rider", "Rider", &["rider"], Syntax::LineFlag),
    gui("kate", "Kate", &["kate"], Syntax::LineFlag),
    gui("gedit", "gedit", &["gedit"], Syntax::Plus),
    gui("gvim", "GVim", &["gvim"], Syntax::Plus),
    gui("emacs", "Emacs", &["emacs"], Syntax::Plus),
    tui("nvim", "Neovim", &["nvim"], Syntax::Plus),
    tui("vim", "Vim", &["vim"], Syntax::Plus),
    tui("hx", "Helix", &["hx", "helix"], Syntax::Colon),
    tui("micro", "micro", &["micro"], Syntax::Plus),
    tui("nano", "nano", &["nano"], Syntax::Plus),
    tui("kak", "Kakoune", &["kak"], Syntax::Plus),
    tui("vi", "vi", &["vi"], Syntax::Plus),
];

/// The editor a setting comes to.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    /// What to call it in messages.
    pub label: String,
    /// The program and the words the setting puts before the file.
    pub argv: Vec<String>,
    pub how: How,
    pub terminal: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum How {
    Known(Syntax),
    /// A command written with `{file}` and `{line}` in it.
    Template,
    /// An unknown program: given the file only.
    FileOnly,
}

/// The known editor a program is (by its file name, so a full path counts too).
pub fn known(program: &str) -> Option<&'static Known> {
    let base = program.rsplit('/').next().unwrap_or(program);
    KNOWN.iter().find(|k| k.name == base || k.bins.contains(&base))
}

/// The editor for `setting`. Empty: `visual`, then `editor` (the environment's), then
/// the first of `KNOWN` that `has` finds installed. A program named in the environment
/// that Maia doesn't know is taken to run in a terminal, as `$EDITOR` is meant to.
pub fn resolve(setting: &str, visual: Option<&str>, editor: Option<&str>, has: &dyn Fn(&str) -> bool) -> Option<Choice> {
    let from_words = |s: &str, env: bool| -> Option<Choice> {
        let words: Vec<String> = s.split_whitespace().map(String::from).collect();
        let first = words.first()?.clone();
        if words.iter().any(|w| w.contains("{file}")) {
            let label = first.rsplit('/').next().unwrap_or(&first).to_string();
            return Some(Choice { label, argv: words, how: How::Template, terminal: false });
        }
        Some(match known(&first) {
            Some(k) => Choice { label: k.label.into(), argv: words, how: How::Known(k.syntax), terminal: k.terminal },
            None => Choice { label: first.rsplit('/').next().unwrap_or(&first).into(), argv: words, how: How::FileOnly, terminal: env },
        })
    };
    let setting = setting.trim();
    if !setting.is_empty() {
        return from_words(setting, false);
    }
    for v in [visual, editor].into_iter().flatten() {
        if let Some(c) = from_words(v, true) {
            return Some(c);
        }
    }
    KNOWN.iter().find_map(|k| {
        let bin = k.bins.iter().find(|b| has(b))?;
        Some(Choice { label: k.label.into(), argv: vec![bin.to_string()], how: How::Known(k.syntax), terminal: k.terminal })
    })
}

/// The command line that opens `file` at `line` (the top without one).
pub fn command(c: &Choice, file: &str, line: Option<u32>) -> Vec<String> {
    let mut v = c.argv.clone();
    match (&c.how, line) {
        (How::Template, _) => {
            let l = line.unwrap_or(1).to_string();
            return v.iter().map(|w| w.replace("{file}", file).replace("{line}", &l)).collect();
        }
        (How::Known(Syntax::Goto), Some(l)) => v.extend(["-g".into(), format!("{file}:{l}")]),
        (How::Known(Syntax::Colon), Some(l)) => v.push(format!("{file}:{l}")),
        (How::Known(Syntax::Plus), Some(l)) => v.extend([format!("+{l}"), file.into()]),
        (How::Known(Syntax::LineFlag), Some(l)) => v.extend(["--line".into(), l.to_string(), file.into()]),
        _ => v.push(file.into()),
    }
    v
}

/// The environment's editors (a test sets its own).
fn env(key: &str) -> Option<String> {
    #[cfg(test)]
    return TEST_ENV.with(|e| e.borrow().iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()));
    #[cfg(not(test))]
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
thread_local! {
    /// `VISUAL` and `EDITOR` as a test wants them (the real ones are never read).
    pub static TEST_ENV: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The editor the setting comes to on this computer.
pub fn current(setting: &str) -> Option<Choice> {
    let tools = crate::drives::Tools::current()?;
    resolve(setting, env("VISUAL").as_deref(), env("EDITOR").as_deref(), &|b| tools.has(b))
}

/// What Auto picks here, for the option's hint: looked up once on a thread (it is a few
/// dozen lookups along PATH), None until then.
pub fn auto_label() -> Option<String> {
    #[cfg(test)]
    return current("").map(|c| c.label);
    #[cfg(not(test))]
    {
        use std::sync::OnceLock;
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        static FOUND: OnceLock<Option<String>> = OnceLock::new();
        static ASKED: AtomicBool = AtomicBool::new(false);
        if let Some(f) = FOUND.get() {
            return f.clone();
        }
        if !ASKED.swap(true, Relaxed) {
            std::thread::spawn(|| {
                let _ = FOUND.set(current("").map(|c| c.label));
            });
        }
        None
    }
}

/// Open `path` at `line` in the editor `setting` comes to: its label, or why not.
pub fn open_at(setting: &str, path: &Path, line: Option<u32>) -> Result<String, String> {
    let c = current(setting).ok_or("No editor found: name one in Options")?;
    let tools = crate::drives::Tools::current().ok_or("No editor found")?;
    let mut argv = command(&c, &path.to_string_lossy(), line);
    // A bare program name goes through the tools (a fake one in tests).
    if !argv[0].contains('/') {
        argv[0] = tools.path_of(&argv[0]).to_string_lossy().into_owned();
    }
    let dir = path.parent().filter(|d| d.is_dir()).unwrap_or(Path::new("/"));
    if c.terminal {
        crate::mime::run_in_terminal(dir, &argv)?;
    } else {
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]).current_dir(dir);
        crate::mime::spawn_detached(cmd)?;
    }
    Ok(c.label)
}
