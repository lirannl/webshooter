//! Whether the graphical session is locked, and a way to wait for it to unlock.
//!
//! Portal dialogs are approved by injecting Enter into the compositor (see
//! [`crate::pipewire::portal_auth`]), which only works if something on screen
//! can take the keystroke. A locked session has exactly that problem: the
//! screen shows the lock screen, no dialog ever takes keyboard focus, and the
//! keystrokes go nowhere. The approval then cannot succeed no matter how long
//! it is waited for, so a locked session is not a slow approval — it is a
//! refused one.
//!
//! Consequently a locked session is answered, not waited on: the client is told
//! the session is locked, and the capture is parked until logind reports an
//! unlock, at which point approval resumes and the parked capture proceeds.
//! Manual approval is never offered: a person watching this server is not
//! necessarily the person at the locked machine, and a prompt that can only be
//! answered on the machine's own screen is not an answer path.
//!
//! logind is the source of truth (`LockedHint` on the seat-bearing session),
//! and the same discovery [`crate::compositor_discovery`] already does for
//! `XDG_RUNTIME_DIR` finds that session.

use std::time::Duration;

use anyhow::{Context, Result};

/// How often the lock state is re-read while waiting for an unlock.
///
/// Short enough that a capture resumes promptly after the user comes back, long
/// enough that a parked capture is not spawning `loginctl` in a tight loop. The
/// portal dialogs queued behind the wait are pressed for 30s each, so this is
/// not what bounds how quickly a resumed capture actually starts.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Whether the session can take a keystroke for portal approval.
///
/// `Unknown` is the answer whenever logind cannot be consulted — no
/// `loginctl`, no seat-bearing session, a timeout. It deliberately resolves to
/// "try anyway" rather than "refuse": a session we cannot ask about is not
/// evidence of a locked one, and refusing every capture on an unusual system
/// would be a much worse failure than attempting an approval that might work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    /// The session is unlocked; keystrokes reach a dialog.
    Unlocked,
    /// The session is locked; keystrokes go nowhere and no dialog can be
    /// approved until it unlocks.
    Locked,
    /// logind could not be consulted.
    Unknown,
}

impl LockState {
    /// Whether a portal dialog could be approved in this state.
    ///
    /// Only [`LockState::Locked`] is a refusal. `Unknown` presses on, because
    /// the alternative is refusing captures outright on any system where
    /// loginctl is missing or slow.
    pub fn can_approve(self) -> bool {
        !matches!(self, LockState::Locked)
    }
}

/// The session to ask logind about, resolved once per process.
///
/// Logged rather than propagated: a missing session is not fatal to anything
/// here, it only means every answer is `Unknown`, and the caller already treats
/// `Unknown` as "carry on".
fn session_id() -> Option<String> {
    match crate::compositor_discovery::seat_session_for_uid(unsafe { libc::getuid() }) {
        Ok(Some(id)) => Some(id),
        Ok(None) => {
            log::debug!("session lock: no seat-bearing logind session; assuming unlocked");
            None
        }
        Err(err) => {
            log::debug!("session lock: could not find a logind session: {err:#}");
            None
        }
    }
}

/// Ask logind whether `session_id` is locked.
fn locked_hint(session_id: &str) -> Result<LockState> {
    let output = std::process::Command::new("loginctl")
        .args(["show-session", session_id, "-p", "LockedHint"])
        .output()
        .context("failed to run `loginctl show-session`")?;
    if !output.status.success() {
        anyhow::bail!(
            "`loginctl show-session {session_id}` exited with {}",
            output.status
        );
    }
    Ok(parse_locked_hint(&String::from_utf8_lossy(&output.stdout)))
}

/// Interpret `loginctl`'s `LockedHint` output.
///
/// Split out from the process spawn so the shapes that actually matter are
/// pinned: the two answers, a missing property, and the empty output a failing
/// or redirected `loginctl` can produce. Anything unrecognised is `Unknown`,
/// which carries on — the same reasoning as [`LockState::Unknown`] itself.
fn parse_locked_hint(output: &str) -> LockState {
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "LockedHint" {
            continue;
        }
        return match value.trim() {
            "yes" | "true" => LockState::Locked,
            "no" | "false" => LockState::Unlocked,
            // logind uses "na" for a session that has no lock screen at all.
            "na" | "n/a" => LockState::Unlocked,
            "" => LockState::Unknown,
            other => {
                log::debug!("session lock: unrecognised LockedHint={other:?}");
                LockState::Unknown
            }
        };
    }
    log::debug!("session lock: no LockedHint in `loginctl` output");
    LockState::Unknown
}

/// Read the lock state of the graphical session right now.
pub fn state() -> LockState {
    let Some(session_id) = session_id() else {
        return LockState::Unknown;
    };
    match locked_hint(&session_id) {
        Ok(state) => state,
        Err(err) => {
            log::debug!("session lock: could not read the lock state: {err:#}");
            LockState::Unknown
        }
    }
}

/// Wait until the session is unlocked, returning immediately if it already is.
///
/// Returns [`LockState::Unknown`] if logind cannot be consulted: there is no
/// signal to wait on, so waiting would hang a capture for good. That is the
/// right trade — the caller then attempts approval, which is exactly what it
/// would have done without this call.
pub async fn wait_until_unlocked(cancel: &tokio_util::sync::CancellationToken) -> LockState {
    let mut reported = None;
    loop {
        let state = state();
        if reported != Some(state) {
            log::info!("session lock: {state:?}");
            reported = Some(state);
        }
        if state.can_approve() {
            return state;
        }
        tokio::select! {
            _ = cancel.cancelled() => return LockState::Locked,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer that matters: locked means keystrokes go nowhere, so a dialog
    /// can never be approved and the capture is refused rather than waited on.
    #[test]
    fn a_locked_session_cannot_approve() {
        assert!(!LockState::Locked.can_approve());
    }

    /// Unlocked, and — deliberately — *unknown*, both carry on. Refusing on an
    /// unreadable lock state would break every capture on a system where
    /// loginctl is missing or slow, which is a far worse failure than an
    /// approval attempt that might work.
    #[test]
    fn only_a_known_lock_refuses() {
        assert!(LockState::Unlocked.can_approve());
        assert!(LockState::Unknown.can_approve());
    }

    #[test]
    fn locked_hint_answers_are_read() {
        assert_eq!(parse_locked_hint("LockedHint=yes\n"), LockState::Locked);
        assert_eq!(parse_locked_hint("LockedHint=no\n"), LockState::Unlocked);
    }

    /// A session with no lock screen has nothing to unlock and must not be
    /// treated as locked: logind reports `na` for those.
    #[test]
    fn a_session_without_a_lock_screen_is_not_locked() {
        assert_eq!(parse_locked_hint("LockedHint=na\n"), LockState::Unlocked);
    }

    /// Output we cannot interpret is `Unknown`, never `Locked`. Guessing "locked"
    /// here would refuse captures on any logind that changes its spelling — the
    /// failure mode this whole type is built to avoid.
    #[test]
    fn unreadable_output_is_unknown_not_locked() {
        for output in [
            "",
            "some other property=1\n",
            "LockedHint=maybe\n",
            "LockedHint=\n",
        ] {
            assert_eq!(
                parse_locked_hint(output),
                LockState::Unknown,
                "{output:?} must not be read as locked"
            );
        }
    }

    /// The property is read from a subprocess, so spacing around the separator
    /// must not matter, and a property whose *name* merely contains the key
    /// must not be mistaken for it.
    #[test]
    fn parsing_is_about_the_key_not_its_spelling() {
        assert_eq!(
            parse_locked_hint("  LockedHint = yes  \n"),
            LockState::Locked
        );
        assert_eq!(parse_locked_hint("NotLockedHint=yes\n"), LockState::Unknown);
    }

    /// The session this process belongs to has to be findable at all, or every
    /// answer is `Unknown` and the whole gate is inert.
    ///
    /// This is the check for the bug it was written for: asking logind for a uid
    /// property it does not have (spelled `User` on current systemd) is not an
    /// error, it is an *omission*, so discovery silently returned `None` on
    /// every modern machine — indistinguishable from "no graphical session", and
    /// quietly disabling the gate everywhere.
    ///
    /// `Ok(None)` is tolerated rather than failed, because a machine with no
    /// graphical session is not this module's bug and panicking there would
    /// make the suite unrunnable on headless CI. What is asserted is that
    /// discovery does not fail outright and does not hand back an empty id.
    #[test]
    fn this_machine_has_a_findable_seat_session() {
        match crate::compositor_discovery::seat_session_for_uid(unsafe { libc::getuid() }) {
            Ok(Some(id)) => assert!(!id.is_empty(), "a session id must not be empty"),
            Ok(None) => log::debug!("no seat-bearing session here; state() will be Unknown"),
            Err(err) => panic!("session discovery failed outright: {err:#}"),
        }
    }

    /// The lock state of the machine running the test must be readable, and the
    /// two states must agree about approvability — otherwise the refusal would
    /// not follow the lock and the gate would be decorative.
    #[test]
    fn the_state_of_this_machine_is_readable() {
        let state = state();
        assert_eq!(state.can_approve(), state != LockState::Locked);
    }
}
