/// Virtual keyboard that auto-accepts XDG Desktop Portal dialogs
/// by simulating Enter via `inputtino`.
///
/// Create a single `PortalAuthKb` at the start of the capture session,
/// then call `accept_dialog` for each portal dialog.  The keyboard is
/// destroyed when it goes out of scope.
use anyhow::{Result, anyhow};
use std::{
    future::Future,
    ops::Deref,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::{Duration, sleep, timeout};

use crate::config::CONFIG_DIR;
use crate::keyboard::{self, Keyboard};

/// The message a refused approval carries, and the one sent to the client.
///
/// Shared by the server that refuses and the client that has to explain it, so
/// the two cannot drift into "server says one thing, client shows another".
pub const SESSION_LOCKED: &str = "the session is locked — unlock it to start sharing your screen";

static PORTAL_TOKEN_FILE: LazyLock<PathBuf> =
    LazyLock::new(|| CONFIG_DIR.get().unwrap().join("portal_token"));

/// Per-capture restore-token state.  Each concurrent capture owns its own
/// `Arc<Mutex<Option<String>>>` so two clients can't clobber each other's
/// portal session token (the previous implementation shared a single global
/// file, so a second client would overwrite the first's token mid-session).
pub type PortalToken = Arc<Mutex<Option<String>>>;

/// Seed a fresh per-capture token state from the on-disk token written by
/// `setup_pipewire` at startup, so the very first `select_devices` dialog is
/// still skipped.  Each capture then maintains its own copy in memory.
pub async fn load_persisted_portal_token() -> PortalToken {
    PortalToken::new(Mutex::new(get_persisted_portal_token().await))
}

pub fn get_portal_token(state: &PortalToken) -> Option<String> {
    state.lock().unwrap().clone()
}

pub fn set_portal_token(state: &PortalToken, token: String) {
    *state.lock().unwrap() = Some(token);
}

async fn get_persisted_portal_token() -> Option<String> {
    let file = File::open(PORTAL_TOKEN_FILE.deref()).await.ok()?;
    let mut string = String::default();
    let _ = BufReader::new(file).read_to_string(&mut string).await;
    Some(string)
}

/// Read the startup auth token from disk (without wrapping it in capture
/// state).  Used by `setup_pipewire` to seed its own `select_devices` call.
pub async fn read_persisted_portal_token() -> Option<String> {
    get_persisted_portal_token().await
}

/// Persist the startup auth token to disk so subsequent launches (and the
/// initial seed of every capture) can skip the first `select_devices` dialog.
pub async fn persist_portal_token(token: String) {
    if let Ok(mut file) = File::create(PORTAL_TOKEN_FILE.deref()).await {
        let _ = file.write_all(token.as_bytes()).await;
    }
}

// ---------------------------------------------------------------------------
// Auto-accept helper
// ---------------------------------------------------------------------------

/// How long a dialog may sit unapproved before it is given up on.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a dialog is given to appear before Enter is pressed blind.
///
/// A ceiling, never a delay the common path pays: this is raced against the
/// portal call, and a call with a valid restore token shows no dialog at all, so
/// it wins the race and returns without a keystroke ever being pressed. Only the
/// rare call that does show a dialog waits this out — which is also why it can be
/// generous, since nothing is waiting on it but a person who has not been asked
/// yet.
const DIALOG_GRACE: Duration = Duration::from_millis(300);

/// How long to expect an approval to take after Enter has been pressed once.
///
/// Short on purpose: Enter has just gone to the dialog, so an answer that has not
/// arrived by now is not coming from that keystroke. What arrives after this
/// point is a person, and that is the fallback below rather than a reason to keep
/// pressing.
const POST_PRESS_GRACE: Duration = Duration::from_millis(50);

/// Wait for a person to approve the dialog, bounded.
///
/// The fallback, never the path: the Enter injection above is what normally
/// closes these dialogs, and a human is only asked once that has not worked —
/// either because there is no keyboard to inject with, or because the injected
/// keystrokes did not reach the dialog.
///
/// Bounded because a capture nobody is watching must not hang forever, and
/// reporting rather than exiting because a server that kills itself here leaves
/// every connected client with a dropped session and no explanation. The
/// caller decides what to do with the failure.
async fn wait_for_manual<T, F: Future<Output = Result<T>>>(
    portal_fut: std::pin::Pin<&mut F>,
    why: &str,
) -> Result<T> {
    println!("[portal_auth] waiting up to {APPROVAL_TIMEOUT:?} for manual approval ({why})");
    match timeout(APPROVAL_TIMEOUT, portal_fut).await {
        Ok(result) => {
            println!("[portal_auth] manually approved");
            result
        }
        Err(_) => {
            eprintln!("[portal_auth] no approval after {APPROVAL_TIMEOUT:?} ({why})");
            Err(
                anyhow!("portal dialog not approved within {APPROVAL_TIMEOUT:?}")
                    .context("portal dialog not approved"),
            )
        }
    }
}

/// Run `portal_fut`, approving the dialog by pressing Enter **once**.
///
/// The sequence, in order:
///
/// 1. the portal call starts, raced against a short grace for a dialog to
///    appear;
/// 2. a call that shows no dialog — a valid restore token — completes inside
///    that race and no keystroke is ever pressed;
/// 3. otherwise Enter is pressed **once**, to the dialog that is now up;
/// 4. [`POST_PRESS_GRACE`] is allowed for the answer;
/// 5. failing that, a person gets [`APPROVAL_TIMEOUT`] to click it.
///
/// Once, rather than repeatedly, because an injected key goes wherever focus is:
/// pressing again would land in whatever the person switched to in the meantime.
/// The press is not timed to the dialog taking focus — Wayland will not tell a
/// client that its own window lost keyboard focus, and the one protocol that can
/// (`wlr-foreign-toplevel`) is a wlroots extension that non-wlr compositors need
/// not implement. So this waits a bounded moment and presses once, and the
/// fallback is a person.
///
/// A locked session is refused at step 3 rather than pressed through: keystrokes
/// cannot reach a dialog behind the lock screen, and the one thing they *would*
/// reach is a password prompt. See [`crate::session_lock`].
pub async fn accept_dialog<T>(
    kb: &mut Option<Keyboard>,
    portal_fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::pin!(portal_fut);
    println!("[portal_auth] portal call started");

    // Step 1 and 2: the call, raced against the dialog appearing. `biased` with
    // the portal branch first, so a call that needs no dialog always wins and
    // the common case costs a roundtrip rather than a grace period.
    tokio::select! {
        biased;
        result = portal_fut.as_mut() => {
            println!("[portal_auth] no dialog was shown");
            return result;
        }
        _ = sleep(DIALOG_GRACE) => {},
    }

    // A dialog should now be up, so an approval is genuinely required, and only
    // now is the lock worth reading: it cannot be approved from behind a lock
    // screen, and the keystrokes spent discovering that would land on the lock
    // screen's password prompt — the one input that must never be synthesised.
    //
    // Reading it before the race instead would refuse calls that show no dialog
    // at all, which is most of them: those succeed on a locked session.
    match crate::session_lock::state() {
        crate::session_lock::LockState::Locked => {
            return Err(anyhow!(SESSION_LOCKED).context("portal dialog not approved"));
        }
        crate::session_lock::LockState::Unknown => {
            log::debug!("portal_auth: could not read the lock state; pressing anyway");
        }
        crate::session_lock::LockState::Unlocked => {}
    }

    let Some(kb) = kb.as_mut() else {
        // Nothing to inject with, so this is the fallback from the start rather
        // than after a failed attempt at the primary path.
        return wait_for_manual(portal_fut.as_mut(), "no keyboard").await;
    };

    // Step 3: once. Held briefly before release so it registers as a press
    // rather than a zero-length one.
    println!("[portal_auth] pressing Enter once");
    kb.press_key(keyboard::ENTER);
    sleep(POST_PRESS_GRACE).await;
    kb.release_key(keyboard::ENTER);

    // Step 4: the answer, if it is coming from that keystroke, is already here.
    tokio::select! {
        biased;
        result = portal_fut.as_mut() => {
            println!("[portal_auth] approved by Enter");
            return result;
        }
        _ = sleep(POST_PRESS_GRACE) => {},
    }

    // Step 5: the keystroke did not do it. The portal request is still pending —
    // `select!` gives the future back rather than cancelling it — so this is the
    // same dialog still on screen, now waited on by a person.
    wait_for_manual(portal_fut.as_mut(), "Enter did not approve it").await
}
