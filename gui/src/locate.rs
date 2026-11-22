//! Where the `sonos-bridge` daemon binary is, for each way the app can be run.
//!
//! Two layouts matter and they are incompatible if you only look next to the
//! GUI executable:
//!
//!   * a checkout - the GUI is at `gui/target/release/`, while the daemon it
//!     must start is one workspace up, at `target/release/`;
//!   * an installed app - the GUI sits in `~/.local/bin` with no sibling
//!     binary at all, and the daemon is only findable through PATH.
//!
//! The directory list is built by a pure function so both cases are covered by
//! `apicheck selftest` without a GPU stack.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The name we are looking for.
pub const DAEMON: &str = "sonos-bridge";

/// Directories to search, in order, deduplicated.
///
/// `exe` is this executable's path, `home` the user's home, `path` the PATH
/// variable - all passed in rather than read, so the caller can test any
/// hypothetical install layout.
pub fn search_dirs(exe: &str, home: &str, path: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let add = |d: PathBuf, dirs: &mut Vec<PathBuf>| {
        if !d.as_os_str().is_empty() && !dirs.contains(&d) {
            dirs.push(d);
        }
    };

    // next to the GUI binary first: a single directory holding both binaries
    // is the simplest working install
    let exe_dir = Path::new(exe)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    add(exe_dir.clone(), &mut dirs);

    // then walk up: gui/target/release -> gui/target -> gui -> workspace root.
    // The daemon belongs to the parent workspace, so it is three levels up from
    // the GUI binary plus a target/release hop.
    let mut up = exe_dir.clone();
    for _ in 0..4 {
        match up.parent() {
            Some(p) => up = p.to_path_buf(),
            None => break,
        }
        add(up.join("target/release"), &mut dirs);
        add(up.clone(), &mut dirs);
    }

    // installed for one user, then system wide
    if !home.is_empty() {
        add(Path::new(home).join(".local/bin"), &mut dirs);
        add(Path::new(home).join(".local/share/sonos-bridge"), &mut dirs);
    }
    for d in ["/usr/local/bin", "/usr/local/sbin", "/usr/bin", "/bin"] {
        add(PathBuf::from(d), &mut dirs);
    }

    // finally whatever PATH can find
    for d in std::env::split_paths(path) {
        add(d, &mut dirs);
    }
    dirs
}

/// First `sonos-bridge` under these directories. `tried` records every path
/// considered, which is what makes a "not found" message useful.
pub fn first_executable(dirs: &[PathBuf], tried: &mut Vec<String>) -> Option<String> {
    for d in dirs {
        let cand = d.join(DAEMON);
        let s = cand.to_string_lossy().into_owned();
        if !tried.contains(&s) {
            tried.push(s.clone());
        }
        if is_exe(&cand) {
            return Some(s);
        }
    }
    None
}

fn is_exe(p: &Path) -> bool {
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// The daemon binary to launch. On failure, the list of places searched.
///
/// `SONOS_BRIDGE_BIN` overrides everything: pointing it somewhere wrong has to
/// produce an error, not a silent fallback to a different binary.
pub fn find_daemon() -> Result<String, Vec<String>> {
    let forced = std::env::var("SONOS_BRIDGE_BIN").unwrap_or_default();
    if !forced.is_empty() {
        return if is_exe(Path::new(&forced)) {
            Ok(forced)
        } else {
            Err(vec![forced])
        };
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let path = std::env::var("PATH").unwrap_or_default();
    let dirs = search_dirs(&exe, &home, &path);
    let mut tried = Vec::new();
    first_executable(&dirs, &mut tried).ok_or(tried)
}

/// Start the daemon, exactly as the panel's "Start daemon" button does.
/// Returns the message to show the user; the daemon detaches itself (setsid),
/// so it outlives whatever started it.
pub fn start_daemon() -> Result<String, String> {
    let bin = find_daemon().map_err(|tried| {
        format!(
            "no sonos-bridge binary found ({} places searched, first was {}); install it with ./install.sh or set SONOS_BRIDGE_BIN",
            tried.len(),
            tried.first().cloned().unwrap_or_else(|| "?".into())
        )
    })?;
    let out = std::process::Command::new(&bin)
        .arg("start")
        .output()
        .map_err(|e| format!("could not run {bin}: {e}"))?;
    if out.status.success() {
        let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if said.is_empty() {
            format!("daemon started ({bin})")
        } else {
            said
        })
    } else {
        Err(format!(
            "{bin} could not start the daemon: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Offline checks for the layout rules above.
pub fn selftest() -> i32 {
    let mut fails = 0usize;
    let mut total = 0usize;
    let mut t = |name: &str, ok: bool| {
        total += 1;
        if !ok {
            fails += 1;
            println!("  FAIL  {}", name);
        } else {
            println!("  ok    {}", name);
        }
    };

    // layout 1: checkout. the daemon is one workspace above the gui crate
    let exe = "/home/u/projects/sonos-bridge-rs/gui/target/release/sonos-bridge-gui";
    let dirs = search_dirs(exe, "/home/u", "/usr/bin:/bin");
    t(
        "checkout: finds the workspace target/release",
        dirs.contains(&PathBuf::from(
            "/home/u/projects/sonos-bridge-rs/target/release",
        )),
    );
    t(
        "checkout: looks next to itself first",
        dirs.first()
            == Some(&PathBuf::from(
                "/home/u/projects/sonos-bridge-rs/gui/target/release",
            )),
    );
    t(
        "checkout: falls back to ~/.local/bin",
        dirs.contains(&PathBuf::from("/home/u/.local/bin")),
    );
    t(
        "checkout: PATH entries are searched too",
        dirs.contains(&PathBuf::from("/usr/bin")),
    );

    // layout 2: installed. no sibling binary, PATH is the only lead
    let dirs2 = search_dirs("/home/u/.local/bin/sonos-bridge-gui", "/home/u", "/usr/bin");
    t(
        "installed: gui dir is searched (daemon installed beside it)",
        dirs2.contains(&PathBuf::from("/home/u/.local/bin")),
    );
    t(
        "installed: system bin dirs present",
        dirs2.contains(&PathBuf::from("/usr/bin")),
    );

    // no duplicates, and no empty entry from a stray ":" in PATH
    let dirs3 = search_dirs(exe, "/home/u", "/usr/bin::");
    let mut uniq = dirs3.clone();
    uniq.sort();
    uniq.dedup();
    t("no duplicate directories", uniq.len() == dirs3.len());
    t(
        "empty PATH entry does not add '.'",
        !dirs3.iter().any(|d| d.as_os_str() == ""),
    );

    // resolution: a real executable is picked, a non-executable is skipped
    let dir = std::env::temp_dir().join(format!("sb-locate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let fake = dir.join(DAEMON);
    std::fs::write(&fake, b"#!/bin/sh\nexit 0\n").ok();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).ok();

    let mut tried = Vec::new();
    let found = first_executable(&[dir.clone()], &mut tried);
    t("executable found", found.as_deref() == fake.to_str());
    t(
        "the search path is reported",
        tried.iter().any(|s| s == fake.to_str().unwrap_or("")),
    );

    t("0755 file counts as executable", is_exe(&fake));

    // same name, not runnable: a stale build artefact must not be mistaken
    // for an installed daemon
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o644)).ok();
    t("a non-executable file is not a hit", !is_exe(&fake));
    t(
        "a directory holding only that file is skipped",
        first_executable(&[dir.clone()], &mut Vec::new()).is_none(),
    );
    t(
        "missing directory yields no path but a full report",
        first_executable(&[PathBuf::from("/nonexistent-sb")], &mut Vec::new()).is_none(),
    );

    std::fs::remove_dir_all(&dir).ok();

    println!();
    if fails == 0 {
        println!("locate: all {} checks passed", total);
        0
    } else {
        println!("locate: {}/{} checks FAILED", fails, total);
        1
    }
}
