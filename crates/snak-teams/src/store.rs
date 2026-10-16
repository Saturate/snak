// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where the Teams refresh token is kept between runs.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::error::{CoreError, Result};

#[cfg(not(test))]
const KEYCHAIN_SERVICE: &str = "dev.snak.app";
#[cfg(not(test))]
const KEYCHAIN_USER: &str = "refresh_token";
const OP_ITEM: &str = "Snak Teams login";
// Entra rotates the refresh token on every access-token request (hourly per
// scope). Superseded tokens stay valid, so 1Password, which may ask for Touch
// ID on each write, only gets a fresh one a few times a day.
const OP_WRITE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Storage {
    #[default]
    Keychain,
    /// A 0600 file. No prompts, but any process running as the user can read it.
    File,
    /// An item in 1Password, read and written through the `op` CLI.
    OnePassword { account: String },
}

struct State {
    storage: Storage,
    last_op_write: Option<Instant>,
}

/// Held for every read, write, move and clear. Sign-out clears on a
/// background thread (1Password may wait on Touch ID), and nothing may read,
/// move or save the login until that has finished.
static STORE_OPS: Mutex<()> = Mutex::new(());

fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    STORE_OPS.lock().unwrap_or_else(|e| e.into_inner())
}

// Process-wide because token rotation saves from deep inside the client.
static STATE: Mutex<State> = Mutex::new(State {
    storage: Storage::Keychain,
    last_op_write: None,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Switches storage, moving an existing login across. Call before restoring a session.
/// The switch only takes effect once the login is safely in the new place,
/// so a failed write leaves it readable where it was.
pub async fn set_storage(storage: Storage) {
    let switched = tokio::task::spawn_blocking(move || {
        let _ops = exclusive();
        let previous = state().storage.clone();
        if previous == storage {
            return Ok(());
        }
        if stored_login_signed_out() {
            // Its delete may only look in the new place, so remove it here.
            delete(&previous);
        } else if let Some(token) = read(&previous) {
            write(&storage, &token)?;
            // A copy left behind would survive a later sign-out, which only
            // looks where the setting points.
            delete(&previous);
            state().last_op_write = Some(Instant::now());
        }
        state().storage = storage;
        Ok::<(), CoreError>(())
    })
    .await;
    match switched {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "login not moved; keeping the old storage"),
        Err(e) => tracing::warn!(error = %e, "login move task failed"),
    }
}

/// Points at a storage without moving anything, for tools that only read.
pub fn use_storage_without_moving(storage: Storage) {
    state().storage = storage;
}

/// A refresh token and the sign-out epoch it was read or obtained under.
/// Saves made on its behalf are dropped once that epoch is over.
pub struct StoredLogin {
    pub(crate) token: String,
    pub(crate) epoch: u64,
}

pub async fn load() -> Option<StoredLogin> {
    // A sign-out whose delete hasn't run yet: the stored login is on its way
    // out and must not be read back in by a quick "Try again".
    if stored_login_signed_out() {
        return None;
    }
    tokio::task::spawn_blocking(move || {
        let _ops = exclusive();
        let epoch = epoch();
        if stored_login_signed_out() {
            return None;
        }
        let storage = state().storage.clone();
        let token = read(&storage)?;
        // Sign-out doesn't wait for the lock, so it may have happened while
        // the read sat on a prompt.
        (self::epoch() == epoch).then_some(StoredLogin { token, epoch })
    })
    .await
    .ok()
    .flatten()
}

use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

/// Bumped the moment a sign-out happens. Every save carries the epoch its
/// session started under and is dropped once that no longer matches: an old
/// session still refreshing, or a login begun before the sign-out, must not
/// write its token back.
static EPOCH: AtomicU64 = AtomicU64::new(0);
/// The newest sign-out whose delete has finished (or was skipped).
static CLEARED: AtomicU64 = AtomicU64::new(0);
/// The epoch of the last token written, so a delayed delete can tell that a
/// login begun after its sign-out has already been saved.
static WRITTEN: AtomicU64 = AtomicU64::new(0);

pub(crate) fn epoch() -> u64 {
    EPOCH.load(SeqCst)
}

fn clear_pending() -> bool {
    CLEARED.load(SeqCst) != EPOCH.load(SeqCst)
}

/// A sign-out's delete hasn't run yet, and no login has been saved since the
/// latest sign-out: whatever is stored belongs to a signed-out session.
fn stored_login_signed_out() -> bool {
    clear_pending() && WRITTEN.load(SeqCst) < EPOCH.load(SeqCst)
}

/// `fresh` marks a new login, which skips the 1Password write throttle.
/// Nothing is written if a sign-out happened since `epoch`.
pub(crate) async fn save(token: String, fresh: bool, epoch: u64) -> Result<()> {
    let written = tokio::task::spawn_blocking(move || {
        let _ops = exclusive();
        // Checked under the lock, so it can't race the delete.
        if epoch != self::epoch() {
            tracing::debug!("dropping a token save from a signed-out session");
            return Ok(false);
        }
        // Also under the lock: a move may have finished while this waited.
        let storage = {
            let state = state();
            let throttled = matches!(state.storage, Storage::OnePassword { .. })
                && !fresh
                && state
                    .last_op_write
                    .is_some_and(|at| at.elapsed() < OP_WRITE_INTERVAL);
            if throttled {
                return Ok(false);
            }
            state.storage.clone()
        };
        write(&storage, &token)?;
        WRITTEN.fetch_max(epoch, SeqCst);
        Ok(true)
    })
    .await
    .map_err(|e| CoreError::Keychain(e.to_string()))?;
    if matches!(written, Ok(true)) {
        state().last_op_write = Some(Instant::now());
    }
    written.map(|_| ())
}

/// Signs out: takes effect at once, then deletes the stored login on a
/// background thread, since 1Password may wait on Touch ID. If a login begun
/// after the sign-out is saved before that delete gets to run, the delete is
/// skipped rather than removing it.
pub fn sign_out() {
    let issued = EPOCH.fetch_add(1, SeqCst) + 1;
    let spawned = std::thread::Builder::new()
        .name("snak-sign-out".into())
        .spawn(move || clear(issued));
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "no thread for the sign-out cleanup; running it here");
        clear(issued);
    }
}

fn clear(issued: u64) {
    // On drop, so a panic in the delete doesn't leave every later load
    // thinking a delete is still pending.
    struct MarkCleared(u64);
    impl Drop for MarkCleared {
        fn drop(&mut self) {
            CLEARED.fetch_max(self.0, SeqCst);
        }
    }
    let _ops = exclusive();
    let _cleared = MarkCleared(issued);
    // Bound first: a temporary would keep STATE locked through the `op`
    // calls, which can sit on a Touch ID prompt.
    let current = state().storage.clone();
    // A login saved since this sign-out went to the current storage (saves
    // and moves both happen under the lock), so only that place keeps it.
    let keep = (WRITTEN.load(SeqCst) >= issued).then_some(&current);
    if keep.is_some() {
        tracing::info!("a new login arrived before the sign-out cleanup; keeping it");
    }
    let mut places = vec![Storage::Keychain, Storage::File];
    if matches!(current, Storage::OnePassword { .. }) {
        places.push(current.clone());
    }
    for place in places.iter().filter(|place| Some(*place) != keep) {
        delete(place);
    }
}

fn read(storage: &Storage) -> Option<String> {
    match storage {
        Storage::Keychain => keychain_entry().ok()?.get_password().ok(),
        Storage::File => {
            let token = std::fs::read_to_string(token_file()?).ok()?;
            let token = token.trim();
            (!token.is_empty()).then(|| token.to_string())
        }
        Storage::OnePassword { account } => {
            let id = op_item_ids(account).into_iter().next()?;
            let out = op(
                account,
                &[
                    "item",
                    "get",
                    &id,
                    "--fields",
                    "label=credential",
                    "--reveal",
                ],
                None,
            )
            .ok()?;
            let token = out.trim();
            (!token.is_empty()).then(|| token.to_string())
        }
    }
}

fn write(storage: &Storage, token: &str) -> Result<()> {
    match storage {
        Storage::Keychain => keychain_entry()?
            .set_password(token)
            .map_err(|e| CoreError::Keychain(e.to_string())),
        Storage::File => write_file(token),
        Storage::OnePassword { account } => {
            // Piped as a template so the token never appears in argv, where
            // any process can read it from the process list.
            let template = json!({
                "title": OP_ITEM,
                "category": "API_CREDENTIAL",
                "tags": ["snak"],
                "fields": [{
                    "id": "credential",
                    "label": "credential",
                    "type": "CONCEALED",
                    "value": token,
                }],
            })
            .to_string();
            // By id: a title lookup fails outright once two items share the
            // title, which an earlier version of this code could leave behind.
            match op_item_ids(account).into_iter().next() {
                Some(id) => op(
                    account,
                    &["item", "edit", &id, "--template", "/dev/stdin"],
                    Some(&template),
                ),
                None => op(
                    account,
                    &["item", "create", "--template", "/dev/stdin"],
                    Some(&template),
                ),
            }
            .map(|_| ())
        }
    }
}

/// Ids of our items, most recently updated first.
fn op_item_ids(account: &str) -> Vec<String> {
    let out = match op(
        account,
        &["item", "list", "--tags", "snak", "--format", "json"],
        None,
    ) {
        Ok(out) => out,
        Err(e) => {
            // Usually 1Password locked or the prompt dismissed. On sign-out
            // this means the item stays, so say so rather than fail silently.
            tracing::warn!(error = %e, "could not list Snak's 1Password items");
            return Vec::new();
        }
    };
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(&out) else {
        tracing::warn!("unexpected output from op item list");
        return Vec::new();
    };
    let mut ours: Vec<(&str, &str)> = items
        .iter()
        .filter(|item| item["title"] == OP_ITEM)
        .filter_map(|item| {
            Some((
                item["id"].as_str()?,
                item["updated_at"].as_str().unwrap_or(""),
            ))
        })
        .collect();
    // RFC 3339 timestamps sort correctly as strings.
    ours.sort_by(|a, b| b.1.cmp(a.1));
    ours.into_iter().map(|(id, _)| id.to_string()).collect()
}

fn delete(storage: &Storage) {
    match storage {
        Storage::Keychain => {
            if let Ok(entry) = keychain_entry() {
                let _ = entry.delete_credential();
            }
        }
        Storage::File => {
            if let Some(path) = token_file() {
                let _ = std::fs::remove_file(path);
            }
        }
        // Archived rather than deleted, so it can still be recovered from
        // 1Password's archive.
        Storage::OnePassword { account } => {
            for id in op_item_ids(account) {
                let _ = op(account, &["item", "delete", &id, "--archive"], None);
            }
        }
    }
}

#[cfg(not(test))]
fn keychain_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
        .map_err(|e| CoreError::Keychain(e.to_string()))
}

// Tests sign out, which deletes everywhere; they must never reach the real login.
#[cfg(test)]
fn keychain_entry() -> Result<keyring::Entry> {
    Err(CoreError::Keychain("no Keychain in tests".into()))
}

#[cfg(not(test))]
fn token_file() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join("Library/Application Support/dev.snak.app/teams-refresh-token"))
}

#[cfg(test)]
fn token_file() -> Option<PathBuf> {
    Some(std::env::temp_dir().join(format!("snak-store-test-{}", std::process::id())))
}

fn write_file(token: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let io_error = |e: std::io::Error| CoreError::Keychain(format!("token file: {e}"));
    let path = token_file().ok_or_else(|| CoreError::Keychain("no home directory".into()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io_error)?;
    }
    let tmp = path.with_extension("tmp");
    // A leftover from a crash keeps its old mode through `create`, so start fresh.
    let _ = std::fs::remove_file(&tmp);
    // Created owner-only from the start, so the token is never briefly world-readable.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(io_error)?;
    file.write_all(token.as_bytes()).map_err(io_error)?;
    std::fs::rename(tmp, path).map_err(io_error)
}

fn op(account: &str, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut child = Command::new("op")
        .args(args)
        .args(["--account", account])
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::Keychain(format!("1Password CLI: {e}")))?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes())
            .map_err(|e| CoreError::Keychain(format!("1Password CLI: {e}")))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| CoreError::Keychain(format!("1Password CLI: {e}")))?;
    if !out.status.success() {
        let error = String::from_utf8_lossy(&out.stderr);
        return Err(CoreError::Keychain(format!(
            "1Password CLI: {}",
            error.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
// Holding the store lock across an await is how these tests stand in for a
// delete stuck behind a prompt; the serial guard spans each whole test.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;

    // The store is process-wide, so these run one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        wait_cleared();
        use_storage_without_moving(Storage::File);
        let _ = std::fs::remove_file(token_file().unwrap());
        serial
    }

    fn wait_cleared() {
        for _ in 0..500 {
            if !clear_pending() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("sign-out cleanup never finished");
    }

    fn stored() -> Option<String> {
        read(&Storage::File)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_from_before_sign_out_cannot_save() {
        let _serial = setup();
        let old = epoch();
        sign_out();
        wait_cleared();
        save("ROTATED".into(), false, old).await.unwrap();
        // A device login the user started before signing out.
        save("LATE-LOGIN".into(), true, old).await.unwrap();
        assert_eq!(stored(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_returns_nothing_while_the_delete_waits() {
        let _serial = setup();
        save("OLD".into(), true, epoch()).await.unwrap();
        let held = exclusive();
        sign_out();
        assert!(load().await.is_none());
        drop(held);
        wait_cleared();
        assert_eq!(stored(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_login_after_sign_out_survives_the_delayed_delete() {
        let _serial = setup();
        save("OLD".into(), true, epoch()).await.unwrap();
        // The delete is held up, say behind a Touch ID prompt.
        let held = exclusive();
        sign_out();
        let saving = tokio::spawn(save("NEW".into(), true, epoch()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(held);
        saving.await.unwrap().unwrap();
        wait_cleared();
        assert_eq!(stored().as_deref(), Some("NEW"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restore_read_during_sign_out_is_discarded() {
        let _serial = setup();
        let file = token_file().unwrap();
        // A FIFO makes the read block with the lock held, like a 1Password prompt.
        assert!(
            Command::new("mkfifo")
                .arg(&file)
                .status()
                .unwrap()
                .success()
        );
        let loading = tokio::spawn(load());
        // A non-blocking open of a FIFO only succeeds once a reader has it
        // open, so this waits for the read to be under way with the lock held.
        let mut writer = None;
        for _ in 0..500 {
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(fifo) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&file)
            {
                writer = Some(fifo);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut writer = writer.expect("load never opened the token file");
        sign_out();
        writer.write_all(b"OLD").unwrap();
        drop(writer);
        assert!(loading.await.unwrap().is_none());
        wait_cleared();
        assert_eq!(stored(), None);
    }
}
