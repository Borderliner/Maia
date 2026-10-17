//! Set As Wallpaper, each desktop its own way: picked from the desktop the session
//! names and the wallpaper programs running in it. One Maia can't drive is said so.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::drives::Tools;

/// Pictures every desktop's wallpaper setter reads.
pub fn is_wallpaper(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "png" | "webp")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way {
    /// Noctalia 5's `noctalia msg`.
    Noctalia,
    /// The Quickshell Noctalia before it, through `qs ipc`.
    NoctaliaQs,
    Gnome,
    Cinnamon,
    Mate,
    Kde,
    Xfce,
    Sway,
    Swww,
    Hyprpaper,
}

/// What the session says it is, and what runs in it.
#[derive(Clone, Debug, Default)]
pub struct Desk {
    /// `XDG_CURRENT_DESKTOP`: names separated by colons.
    pub desktop: String,
    /// Names of running programs; a Quickshell running Noctalia as "qs:noctalia-shell".
    pub running: Vec<String>,
    /// Connected outputs ("eDP-1"), for a setter that takes one at a time.
    pub outputs: Vec<String>,
}

#[cfg(test)]
thread_local! {
    /// The desktop a test pretends to be on (tests never read the session's own).
    pub static DESKTOP: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// The session's desktop names.
pub fn desktop() -> String {
    #[cfg(test)]
    return DESKTOP.with(|d| d.borrow().clone());
    #[cfg(not(test))]
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()
}

/// The running programs and connected outputs (reads /proc and /sys: off the UI thread).
pub fn look_around(desktop: String) -> Desk {
    let mut running = vec![];
    for d in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        if !d.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(d.path().join("comm")) else { continue };
        let comm = comm.trim().to_string();
        if comm == "qs" || comm == "quickshell" {
            if std::fs::read(d.path().join("cmdline")).is_ok_and(|c| c.split(|&b| b == 0).any(|a| a == b"noctalia-shell")) {
                running.push("qs:noctalia-shell".into());
            }
        }
        running.push(comm);
    }
    // /sys/class/drm/card1-eDP-1/status says "connected".
    let mut outputs = vec![];
    for d in std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten() {
        let name = d.file_name().to_string_lossy().into_owned();
        let Some((card, conn)) = name.split_once('-') else { continue };
        if card.starts_with("card") && std::fs::read_to_string(d.path().join("status")).is_ok_and(|s| s.trim() == "connected") {
            outputs.push(conn.to_string());
        }
    }
    outputs.sort();
    Desk { desktop, running, outputs }
}

/// How to set the wallpaper here, given which programs `has` finds.
pub fn pick(d: &Desk, has: impl Fn(&str) -> bool) -> Option<Way> {
    let runs = |n: &str| d.running.iter().any(|r| r == n);
    let on = |n: &str| d.desktop.split(':').any(|x| x.trim().eq_ignore_ascii_case(n));
    // A shell that draws its own wallpaper wins over the compositor it runs on.
    if runs("noctalia") && has("noctalia") {
        return Some(Way::Noctalia);
    }
    if runs("qs:noctalia-shell") && has("qs") {
        return Some(Way::NoctaliaQs);
    }
    let gs = has("gsettings");
    if on("KDE") {
        return has("plasma-apply-wallpaperimage").then_some(Way::Kde);
    }
    if on("XFCE") {
        return has("xfconf-query").then_some(Way::Xfce);
    }
    if on("X-Cinnamon") || on("Cinnamon") {
        return gs.then_some(Way::Cinnamon);
    }
    if on("MATE") {
        return gs.then_some(Way::Mate);
    }
    if on("GNOME") || on("Unity") || on("Budgie") {
        return gs.then_some(Way::Gnome);
    }
    // wlroots compositors leave the wallpaper to a program of its own.
    if runs("swww-daemon") && has("swww") {
        return Some(Way::Swww);
    }
    if on("Hyprland") && runs("hyprpaper") && has("hyprctl") {
        return Some(Way::Hyprpaper);
    }
    if on("sway") && has("swaymsg") {
        return Some(Way::Sway);
    }
    None
}

/// One program to run: its arguments, and whether the wallpaper fails with it.
pub struct Step {
    pub args: Vec<OsString>,
    pub needed: bool,
}

fn step(args: &[&dyn AsRef<std::ffi::OsStr>], needed: bool) -> Step {
    Step { args: args.iter().map(|a| a.as_ref().to_os_string()).collect(), needed }
}

/// The programs that set `path` as the wallpaper this way. XFCE's come from its
/// property list (`xfconf-query -c xfce4-desktop -l`), given as `xfce`.
pub fn steps(way: Way, path: &Path, d: &Desk, xfce: &str) -> Vec<Step> {
    let uri = crate::clipboard::path_to_uri(path);
    match way {
        // Noctalia reads the words after wallpaper-set as one path, unless the first is
        // an output: a path with a space in it goes to each output by name.
        Way::Noctalia if path.to_string_lossy().contains(char::is_whitespace) && !d.outputs.is_empty() => d.outputs.iter().map(|o| step(&[&"noctalia", &"msg", &"wallpaper-set", o, &path], true)).collect(),
        Way::Noctalia => vec![step(&[&"noctalia", &"msg", &"wallpaper-set", &path], true)],
        Way::NoctaliaQs => vec![step(&[&"qs", &"-c", &"noctalia-shell", &"ipc", &"call", &"wallpaper", &"set", &path, &"all"], true)],
        Way::Gnome => vec![
            step(&[&"gsettings", &"set", &"org.gnome.desktop.background", &"picture-uri", &uri], true),
            // GNOME 42 and later keep a second picture for the dark style.
            step(&[&"gsettings", &"set", &"org.gnome.desktop.background", &"picture-uri-dark", &uri], false),
        ],
        Way::Cinnamon => vec![step(&[&"gsettings", &"set", &"org.cinnamon.desktop.background", &"picture-uri", &uri], true)],
        Way::Mate => vec![step(&[&"gsettings", &"set", &"org.mate.background", &"picture-filename", &path], true)],
        Way::Kde => vec![step(&[&"plasma-apply-wallpaperimage", &path], true)],
        Way::Xfce => {
            let props = xfce_props(xfce);
            if props.is_empty() {
                vec![step(&[&"xfconf-query", &"-c", &"xfce4-desktop", &"-n", &"-t", &"string", &"-p", &"/backdrop/screen0/monitor0/workspace0/last-image", &"-s", &path], true)]
            } else {
                props.iter().map(|p| step(&[&"xfconf-query", &"-c", &"xfce4-desktop", &"-p", p, &"-s", &path], true)).collect()
            }
        }
        // swaymsg reads its arguments as one command line: the path goes in quotes.
        Way::Sway => vec![step(&[&"swaymsg", &format!("output * bg {} fill", sway_quote(&path.to_string_lossy()))], true)],
        Way::Swww => vec![step(&[&"swww", &"img", &path], true)],
        Way::Hyprpaper => {
            let p = path.to_string_lossy();
            // Newer hyprpaper loads on its own and refuses `preload`.
            vec![step(&[&"hyprctl", &"hyprpaper", &"preload", &p.as_ref()], false), step(&[&"hyprctl", &"hyprpaper", &"wallpaper", &format!(",{p}")], true)]
        }
    }
}

/// XFCE's picture properties: one per monitor and workspace.
pub fn xfce_props(list: &str) -> Vec<String> {
    list.lines().map(str::trim).filter(|l| l.starts_with("/backdrop/") && l.ends_with("/last-image")).map(String::from).collect()
}

fn sway_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Set `path` as the wallpaper. Blocks: run it off the UI thread.
pub fn set(tools: &Tools, d: &Desk, path: &Path) -> Result<Way, String> {
    let way = pick(d, |n| tools.has(n)).ok_or("Maia can't set the wallpaper on this desktop")?;
    let xfce = if way == Way::Xfce {
        let mut c = tools.cmd("xfconf-query");
        c.args(["-c", "xfce4-desktop", "-l"]);
        String::from_utf8_lossy(&crate::errands::output(c, Duration::from_secs(10)).unwrap_or_default()).into_owned()
    } else {
        String::new()
    };
    for s in steps(way, path, d, &xfce) {
        let Some((prog, args)) = s.args.split_first() else { continue };
        let mut c = tools.cmd(&prog.to_string_lossy());
        c.args(args);
        let r = crate::errands::output(c, Duration::from_secs(15));
        // hyprctl reports a refusal in what it prints, and exits 0.
        let r = r.and_then(|out| {
            let said = String::from_utf8_lossy(&out);
            let low = said.to_lowercase();
            if way == Way::Hyprpaper && ["error", "invalid", "fail", "couldn't"].iter().any(|w| low.contains(w)) { Err(said.trim().to_string()) } else { Ok(()) }
        });
        if let Err(e) = r {
            if s.needed {
                return Err(format!("Couldn't set the wallpaper: {e}"));
            }
        }
    }
    Ok(way)
}
