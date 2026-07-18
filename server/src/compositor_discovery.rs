use std::{env, os::unix::fs::FileTypeExt, path::Path};

use anyhow::{Context, Result};

use crate::logging::log;

/// Return the id of the first logind session belonging to `uid` that has a
/// seat assigned (i.e. a graphical session). Used to discover session-derived
/// environment like `XDG_RUNTIME_DIR` and `XDG_CURRENT_DESKTOP` when they are
/// missing from the process environment (e.g. when launched over SSH).
pub(super) fn seat_session_for_uid(uid: u32) -> Result<Option<String>> {
    let output = std::process::Command::new("loginctl")
        .args(["list-sessions", "--no-legend", "--no-pager"])
        .output()
        .context("failed to run `loginctl list-sessions` to discover the graphical session")?;
    if !output.status.success() {
        anyhow::bail!("`loginctl list-sessions` exited with failure");
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(session_id) = line.split_whitespace().next() else {
            continue;
        };
        let props = match std::process::Command::new("loginctl")
            .args(["show-session", session_id, "-p", "UID", "-p", "Seat"])
            .output()
        {
            Ok(p) if p.status.success() => p,
            _ => continue,
        };

        let mut session_uid = None;
        let mut seat = String::new();
        for prop in String::from_utf8_lossy(&props.stdout).lines() {
            if let Some(v) = prop.strip_prefix("UID=") {
                session_uid = v.parse::<u32>().ok();
            } else if let Some(v) = prop.strip_prefix("Seat=") {
                seat = v.to_string();
            }
        }

        if session_uid == Some(uid) && !seat.is_empty() {
            return Ok(Some(session_id.to_string()));
        }
    }

    Ok(None)
}

/// Ensure `XDG_RUNTIME_DIR` is set. If it is missing from the environment we
/// discover it from logind: the runtime dir is conventionally `/run/user/<uid>`
/// for the graphical session. We only hard-fail when logind is present and
/// reports *no* seat-bearing session (i.e. nothing to capture). If logind
/// itself is unavailable we fall back to the conventional path so non-systemd /
/// elogind-less setups still launch when the directory exists.
pub(super) fn ensure_xdg_runtime_dir() -> Result<()> {
    if env::var_os("XDG_RUNTIME_DIR").is_some() {
        return Ok(());
    }
    let uid = unsafe { libc::getuid() };
    let dir = format!("/run/user/{uid}");

    match seat_session_for_uid(uid) {
        Ok(Some(session_id)) => {
            // Safe: single-threaded startup, set before any other code reads it.
            unsafe {
                env::set_var("XDG_RUNTIME_DIR", &dir);
            }
            log(format!(
                "Discovered XDG_RUNTIME_DIR={dir} from session {session_id}"
            ));
            Ok(())
        }
        Ok(None) => anyhow::bail!(
            "No seat-bearing session found for uid {uid}; cannot determine XDG_RUNTIME_DIR. \
             A graphical session is required to capture a display."
        ),
        Err(_) => {
            // logind unavailable: fall back to the convention if the dir exists.
            if Path::new(&dir).is_dir() {
                // Safe: single-threaded startup.
                unsafe {
                    env::set_var("XDG_RUNTIME_DIR", &dir);
                }
                log(format!(
                    "Set XDG_RUNTIME_DIR={dir} by convention (no logind available)"
                ));
                Ok(())
            } else {
                anyhow::bail!(
                    "Could not determine XDG_RUNTIME_DIR: no logind session and {dir} does not exist. \
                     Set XDG_RUNTIME_DIR manually or start a graphical session."
                )
            }
        }
    }
}

/// Ensure `XDG_CURRENT_DESKTOP` is set, which selects the capture path
/// (`is_kwin` -> krfb virtual monitor). Resolution order when unset:
/// 1. logind session `Desktop` property (authoritative when a DM set it),
/// 2. the running compositor process (reliable even when launched manually,
///    where logind's `Desktop` is often empty — common on Arch),
/// 3. the `DESKTOP_SESSION` env var as a last resort.
///
/// Leaving it unset is handled by the caller (exit on Linux).
pub(super) fn ensure_xdg_current_desktop() -> Result<()> {
    if env::var_os("XDG_CURRENT_DESKTOP").is_some() {
        return Ok(());
    }

    // 1. logind session `Desktop` property.
    let uid = unsafe { libc::getuid() };
    if let Some(session_id) = seat_session_for_uid(uid)?
        && let Ok(output) = std::process::Command::new("loginctl")
            .args(["show-session", &session_id, "-p", "Desktop"])
            .output()
        && output.status.success()
        && let Some(desktop) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("Desktop="))
    {
        let desktop = desktop.trim();
        if !desktop.is_empty() {
            // Safe: single-threaded startup.
            unsafe {
                env::set_var("XDG_CURRENT_DESKTOP", desktop);
            }
            log(format!(
                "Discovered XDG_CURRENT_DESKTOP={desktop} from session {session_id}"
            ));
            return Ok(());
        }
    }

    // 2. Infer from the running compositor.
    if let Some(desktop) = desktop_from_compositor() {
        // Safe: single-threaded startup.
        unsafe {
            env::set_var("XDG_CURRENT_DESKTOP", &desktop);
        }
        log(format!(
            "Inferred XDG_CURRENT_DESKTOP={desktop} from running compositor"
        ));
        return Ok(());
    }

    // 3. Session env var as a last resort.
    if let Some(desktop) = env::var_os("DESKTOP_SESSION")
        && !desktop.is_empty()
    {
        let desktop = desktop.to_string_lossy().into_owned();
        // Safe: single-threaded startup.
        unsafe {
            env::set_var("XDG_CURRENT_DESKTOP", &desktop);
        }
        log(format!(
            "Using DESKTOP_SESSION={desktop} for XDG_CURRENT_DESKTOP"
        ));
        return Ok(());
    }

    Ok(())
}

/// Best-effort mapping from a running Wayland/X11 compositor to the value
/// expected in `XDG_CURRENT_DESKTOP`. Needed because logind's session `Desktop`
/// is frequently empty when the compositor is launched manually (e.g. from a
/// TTY), which is typical on Arch.
fn desktop_from_compositor() -> Option<String> {
    const TABLE: &[(&str, &str)] = &[
        ("kwin_wayland", "KDE"),
        ("kwin_x11", "KDE"),
        ("gnome-shell", "GNOME"),
        ("sway", "sway"),
        ("Hyprland", "Hyprland"),
        ("weston", "weston"),
        ("river", "river"),
        ("wayfire", "wayfire"),
        ("labwc", "labwc"),
        ("cosmic-comp", "COSMIC"),
    ];
    for (proc, desktop) in TABLE {
        if compositor_running(proc) {
            return Some(desktop.to_string());
        }
    }
    None
}

/// Whether a process with the given comm name is running. Tries `pgrep` first,
/// then falls back to scanning `/proc` (so it works without procps installed).
fn compositor_running(name: &str) -> bool {
    if let Ok(out) = std::process::Command::new("pgrep").args(["-x", name]).output()
        && out.status.success()
        && !out.stdout.is_empty()
    {
        return true;
    }
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm"))
                && comm.trim() == name
            {
                return true;
            }
        }
    }
    false
}

/// Ensure `WAYLAND_DISPLAY` is set. When missing we scan `XDG_RUNTIME_DIR` for
/// Wayland sockets (`wayland-N`), which is where compositors publish them, and
/// pick the lowest-numbered one. Non-fatal if none is found.
pub(super) fn ensure_wayland_display() -> Result<()> {
    if env::var_os("WAYLAND_DISPLAY").is_some() {
        return Ok(());
    }
    let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") else {
        return Ok(());
    };
    let dir = Path::new(&runtime_dir);

    let mut candidates: Vec<(u32, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name.strip_prefix("wayland-")
                && let Ok(ft) = entry.file_type()
                && ft.is_socket()
                && let Ok(n) = rest.parse::<u32>()
            {
                candidates.push((n, name));
            }
        }
    }
    candidates.sort();

    match candidates.into_iter().next() {
        Some((_, display)) => {
            // Safe: single-threaded startup.
            unsafe {
                env::set_var("WAYLAND_DISPLAY", &display);
            }
            log(format!(
                "Discovered WAYLAND_DISPLAY={display} in {runtime_dir:?}"
            ));
        }
        None => {
            // No socket found: fall back to the conventional default.
            // Safe: single-threaded startup.
            unsafe {
                env::set_var("WAYLAND_DISPLAY", "wayland-0");
            }
            log("No Wayland socket found; defaulting WAYLAND_DISPLAY=wayland-0");
        }
    }
    Ok(())
}
