//! Build output and installed dependencies under a folder: Cargo's `target`, `node_modules`,
//! Python environments and caches, `dist` and `build` beside a manifest that makes them,
//! framework and tool caches. Found on a thread without opening anything twice (a folder's
//! markers come from the same listing as the folder), sized by the folder-size walk, and
//! moved to the trash by the usual job when chosen.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub path: PathBuf,
    pub kind: &'static str,
    /// When the folder last changed (a build or an install touches it).
    pub modified: Option<SystemTime>,
    /// None until it has been added up.
    pub size: Option<u64>,
}

/// A folder named `name` whose siblings are `siblings`: what kind of build output it is,
/// if any. Only names that a tool makes, and `dist`, `build` and the like only beside the
/// manifest of a tool that writes them there.
pub fn kind_by_name(name: &str, siblings: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    let js = || siblings("package.json");
    let py = || siblings("pyproject.toml") || siblings("setup.py") || siblings("setup.cfg");
    let gradle = || ["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"].iter().any(|m| siblings(m));
    Some(match name {
        "node_modules" => "Node packages",
        "__pycache__" => "Python bytecode",
        ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".hypothesis" => "Python tool cache",
        ".tox" | ".nox" => "Python test environments",
        "target" if siblings("Cargo.toml") => "Rust build",
        "target" if siblings("pom.xml") => "Maven build",
        ".gradle" if gradle() => "Gradle cache",
        "build" if gradle() => "Gradle build",
        "build" | "dist" if js() || py() => "Build output",
        "build" if siblings("CMakeLists.txt") || siblings("meson.build") => "Build output",
        "build" | ".dart_tool" if siblings("pubspec.yaml") => "Dart build",
        ".next" if js() => "Next.js build",
        ".nuxt" | ".output" if js() => "Nuxt build",
        ".svelte-kit" if js() => "SvelteKit build",
        ".parcel-cache" if js() => "Parcel cache",
        ".turbo" if js() => "Turborepo cache",
        ".angular" if js() => "Angular cache",
        "zig-cache" | ".zig-cache" | "zig-out" if siblings("build.zig") => "Zig build",
        "_build" | "deps" if siblings("mix.exs") => "Elixir build",
        ".stack-work" if siblings("stack.yaml") => "Haskell build",
        "dist-newstyle" if siblings("cabal.project") => "Haskell build",
        _ => return None,
    })
}

/// What a folder is by what it holds: a Python environment (pyvenv.cfg) or a cache that
/// says so (CACHEDIR.TAG, from the cache directory standard).
pub fn kind_by_contents(names: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    if names("pyvenv.cfg") {
        Some("Python environment")
    } else if names("CACHEDIR.TAG") {
        Some("Cache")
    } else {
        None
    }
}

/// Hidden folders walked into anyway: where Python environments usually are.
const ENV_NAMES: &[&str] = &[".venv", "venv", ".env", "env", "virtualenv"];

pub enum Msg {
    Found(Found),
    Size(PathBuf, u64),
    /// The walk and the sizes are all done.
    Done,
}

/// A scan in progress; dropping it stops it.
pub struct Scan {
    pub rx: mpsc::Receiver<Msg>,
    stop: Arc<AtomicBool>,
    /// Bumped to stop the size walks (they check it as they go).
    generation: Arc<AtomicU64>,
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        self.generation.fetch_add(1, Relaxed);
    }
}

impl Scan {
    pub fn start(root: PathBuf, ctx: &egui::Context) -> Scan {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let (st, generation2, ctx) = (stop.clone(), generation.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("buildclean".into()).spawn(move || {
            let (size_tx, size_rx) = mpsc::channel::<PathBuf>();
            let size_rx = Mutex::new(size_rx);
            let g = generation2.load(Relaxed);
            std::thread::scope(|s| {
                for _ in 0..2 {
                    let (tx, ctx, size_rx, generation2) = (tx.clone(), ctx.clone(), &size_rx, &generation2);
                    s.spawn(move || {
                        loop {
                            let Some(p) = size_rx.lock().ok().and_then(|r| r.recv().ok()) else { return };
                            if generation2.load(Relaxed) != g {
                                continue;
                            }
                            let size = crate::stats::compute(&p, generation2, g).size;
                            if generation2.load(Relaxed) == g {
                                let _ = tx.send(Msg::Size(p, size));
                                ctx.request_repaint();
                            }
                        }
                    });
                }
                walk(&root, &st, &mut |f: Found| {
                    let _ = size_tx.send(f.path.clone());
                    let _ = tx.send(Msg::Found(f));
                    ctx.request_repaint();
                });
                drop(size_tx);
            });
            let _ = tx.send(Msg::Done);
            ctx.request_repaint();
        });
        Scan { rx, stop, generation }
    }
}

/// Every build folder under `root`, handed to `found` as it turns up. Not entered: the
/// folders found, `.git`, other hidden folders (but for Python environments), links and
/// other file systems.
pub fn walk(root: &Path, stop: &AtomicBool, found: &mut dyn FnMut(Found)) {
    use std::os::unix::fs::MetadataExt;
    let Ok(dev) = std::fs::metadata(root).map(|m| m.dev()) else { return };
    if crate::fsx::is_virtual_fs(root) {
        return;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if stop.load(Relaxed) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let kids: Vec<(String, bool, PathBuf)> = rd.flatten().filter_map(|e| {
            let ft = e.file_type().ok()?;
            (!ft.is_symlink()).then(|| (e.file_name().to_string_lossy().into_owned(), ft.is_dir(), e.path()))
        }).collect();
        let has = |n: &str| kids.iter().any(|k| k.0 == n);
        if d != root {
            if let Some(kind) = kind_by_contents(&has) {
                let modified = std::fs::metadata(&d).and_then(|m| m.modified()).ok();
                found(Found { path: d, kind, modified, size: None });
                continue;
            }
        }
        let mut dirs: Vec<PathBuf> = vec![];
        for (name, is_dir, path) in &kids {
            if !is_dir || name == ".git" {
                continue;
            }
            let Ok(m) = std::fs::symlink_metadata(path) else { continue };
            if m.dev() != dev {
                continue;
            }
            if let Some(kind) = kind_by_name(name, &has) {
                found(Found { path: path.clone(), kind, modified: m.modified().ok(), size: None });
            } else if !name.starts_with('.') || ENV_NAMES.contains(&name.as_str()) {
                dirs.push(path.clone());
            }
        }
        dirs.sort();
        stack.extend(dirs.into_iter().rev());
    }
}
