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
    /// Unset until the first `set_storage` of the run adopts the setting.
    storage: Option<Storage>,
    last_op_write: Option<Instant>,
    /// Places a sign-out or a move couldn't clear (1Password locked, `op`
    /// gone, a prompt dismissed), so they may hold a login that must not be
    /// read back. The next sign-out tries them again, and a write there
    /// replaces what they hold.
    left_behind: Vec<Storage>,
}

impl State {
    fn current(&self) -> Storage {
        self.storage.clone().unwrap_or_default()
    }
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
    storage: None,
    last_op_write: None,
    left_behind: Vec::new(),
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Switches storage, moving an existing login across. The first call of a
/// run only adopts the setting: the login is wherever the setting already
/// said, and the default it would move from may hold nothing of ours.
///
/// On an error the old storage stays in use: a sign-out only looks in
/// 1Password while the setting points there, so switching away from a copy
/// that couldn't be read or removed would strand it.
pub async fn set_storage(storage: Storage) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let _ops = exclusive();
        // Bound first: a temporary in the `let ... else` would keep STATE
        // locked into the else block.
        let previous = state().storage.clone();
        let Some(previous) = previous else {
            state().storage = Some(storage);
            return Ok(());
        };
        if previous == storage {
            return Ok(());
        }
        if signed_out(&previous) {
            // Nothing to move, so the switch goes ahead even if the old copy
            // can't be removed (a broken 1Password would otherwise be a dead
            // end); it's remembered for the next sign-out instead.
            match delete(&previous) {
                Ok(()) => state().left_behind.retain(|place| *place != previous),
                Err(e) => {
                    tracing::warn!(error = %e, "switching away from a login that couldn't be removed");
                    remember_left_behind(&previous);
                }
            }
            clear_if_left_behind(&storage);
        } else if let Some(token) = read(&previous)? {
            write(&storage, &token)?;
            if let Err(e) = delete(&previous) {
                // Back to one copy, where the setting still points.
                match delete(&storage) {
                    Ok(()) => state().left_behind.retain(|place| *place != storage),
                    Err(undo) => {
                        tracing::warn!(error = %undo, "login left in both storages");
                        remember_left_behind(&storage);
                    }
                }
                return Err(e);
            }
            let mut state = state();
            state.last_op_write = Some(Instant::now());
            state.left_behind.retain(|place| *place != storage);
        } else {
            clear_if_left_behind(&storage);
        }
        state().storage = Some(storage);
        Ok(())
    })
    .await
    .map_err(|e| CoreError::TaskFailed(e.to_string()))?
}

/// Points at a storage without moving anything, for tools that only read.
pub fn use_storage_without_moving(storage: Storage) {
    state().storage = Some(storage);
}

/// A refresh token and the sign-out epoch it was read or obtained under.
/// Saves made on its behalf are dropped once that epoch is over.
pub struct StoredLogin {
    pub(crate) token: String,
    pub(crate) epoch: u64,
}

/// `Ok(None)` when nothing usable is stored.
pub async fn load() -> Result<Option<StoredLogin>> {
    // Checked before waiting on the lock too, which a pending delete may hold
    // through a Touch ID prompt.
    let storage = state().current();
    if signed_out(&storage) {
        return Ok(None);
    }
    tokio::task::spawn_blocking(move || {
        let _ops = exclusive();
        let epoch = epoch();
        let storage = state().current();
        // A sign-out whose delete hasn't run (or failed): the stored login is
        // on its way out and must not be read back in by a quick "Try again".
        if signed_out(&storage) {
            return Ok(None);
        }
        let token = read(&storage)?;
        // Sign-out doesn't wait for the lock, so it may have happened while
        // the read sat on a prompt.
        Ok(token
            .filter(|_| self::epoch() == epoch)
            .map(|token| StoredLogin { token, epoch }))
    })
    .await
    .map_err(|e| CoreError::TaskFailed(e.to_string()))?
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

/// What `storage` holds belongs to a signed-out session: a sign-out's delete
/// hasn't run yet and nothing was saved since, or it couldn't clear there.
fn signed_out(storage: &Storage) -> bool {
    state().left_behind.contains(storage)
        || (clear_pending() && WRITTEN.load(SeqCst) < EPOCH.load(SeqCst))
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
            let throttled = matches!(state.storage, Some(Storage::OnePassword { .. }))
                && !fresh
                && state
                    .last_op_write
                    .is_some_and(|at| at.elapsed() < OP_WRITE_INTERVAL);
            if throttled {
                return Ok(false);
            }
            state.current()
        };
        write(&storage, &token)?;
        WRITTEN.fetch_max(epoch, SeqCst);
        state().left_behind.retain(|place| *place != storage);
        Ok(true)
    })
    .await
    .map_err(|e| CoreError::TaskFailed(e.to_string()))?;
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
    let current = state().current();
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
    let left_behind = state().left_behind.clone();
    for place in left_behind {
        if !places.contains(&place) {
            places.push(place);
        }
    }
    for place in places.iter().filter(|place| Some(*place) != keep) {
        match delete(place) {
            Ok(()) => state().left_behind.retain(|left| left != place),
            Err(e) => {
                tracing::warn!(error = %e, "sign-out left a login behind");
                remember_left_behind(place);
            }
        }
    }
}

/// Switching to a place a sign-out couldn't clear, with nothing to carry
/// over: clear it now, since the list doesn't survive a restart and the
/// next launch would read it.
fn clear_if_left_behind(place: &Storage) {
    let left = state().left_behind.contains(place);
    if !left {
        return;
    }
    match delete(place) {
        Ok(()) => state().left_behind.retain(|left| left != place),
        Err(e) => {
            tracing::warn!(error = %e, "switched to a place still holding a signed-out login")
        }
    }
}

fn remember_left_behind(place: &Storage) {
    let mut state = state();
    if !state.left_behind.contains(place) {
        state.left_behind.push(place.clone());
    }
}

/// `Ok(None)` only when nothing is stored. An error means the store couldn't
/// be asked, say 1Password locked or a prompt dismissed.
fn read(storage: &Storage) -> Result<Option<String>> {
    let token = match storage {
        Storage::Keychain => keychain_get()?,
        Storage::File => match std::fs::read_to_string(token_path()?) {
            Ok(token) => Some(token),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(CoreError::Keychain(format!("token file: {e}"))),
        },
        Storage::OnePassword { account } => {
            let Some(id) = op_item_ids(account)?.into_iter().next() else {
                return Ok(None);
            };
            Some(op(
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
            )?)
        }
    };
    Ok(token
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty()))
}

fn write(storage: &Storage, token: &str) -> Result<()> {
    match storage {
        Storage::Keychain => keychain_set(token),
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
            match op_item_ids(account)?.into_iter().next() {
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
fn op_item_ids(account: &str) -> Result<Vec<String>> {
    let out = op(
        account,
        &["item", "list", "--tags", "snak", "--format", "json"],
        None,
    )?;
    let items = serde_json::from_str::<Vec<serde_json::Value>>(&out)
        .map_err(|e| CoreError::Keychain(format!("1Password CLI: unexpected item list: {e}")))?;
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
    Ok(ours.into_iter().map(|(id, _)| id.to_string()).collect())
}

fn delete(storage: &Storage) -> Result<()> {
    match storage {
        Storage::Keychain => keychain_delete(),
        Storage::File => match std::fs::remove_file(token_path()?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(CoreError::Keychain(format!("token file: {e}")))
            }
            _ => Ok(()),
        },
        // Archived rather than deleted, so it can still be recovered from
        // 1Password's archive.
        Storage::OnePassword { account } => {
            for id in op_item_ids(account)? {
                op(account, &["item", "delete", &id, "--archive"], None)?;
            }
            Ok(())
        }
    }
}

#[cfg(not(test))]
fn keychain_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
        .map_err(|e| CoreError::Keychain(e.to_string()))
}

#[cfg(not(test))]
fn keychain_get() -> Result<Option<String>> {
    match keychain_entry()?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(CoreError::Keychain(e.to_string())),
    }
}

#[cfg(not(test))]
fn keychain_set(token: &str) -> Result<()> {
    keychain_entry()?
        .set_password(token)
        .map_err(|e| CoreError::Keychain(e.to_string()))
}

#[cfg(not(test))]
fn keychain_delete() -> Result<()> {
    match keychain_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(CoreError::Keychain(e.to_string())),
    }
}

// Tests sign out, which deletes everywhere; they must never reach the real
// login. The test Keychain is always empty and refuses writes.
#[cfg(test)]
fn keychain_get() -> Result<Option<String>> {
    Ok(None)
}

#[cfg(test)]
fn keychain_set(_token: &str) -> Result<()> {
    Err(CoreError::Keychain("no Keychain in tests".into()))
}

#[cfg(test)]
fn keychain_delete() -> Result<()> {
    Ok(())
}

fn token_path() -> Result<PathBuf> {
    token_file().ok_or_else(|| CoreError::Keychain("no home directory".into()))
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
    let path = token_path()?;
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

    /// Also undoes whatever a failed test left behind, so one failure
    /// doesn't cascade into the rest.
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // A sign-out thread from the last test may still be running; a test
        // that bumped the epoch by hand and then failed never clears.
        for _ in 0..500 {
            if !clear_pending() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        CLEARED.store(EPOCH.load(SeqCst), SeqCst);
        {
            let mut state = state();
            state.storage = Some(Storage::File);
            state.left_behind.clear();
        }
        let file = token_file().unwrap();
        let _ = std::fs::remove_dir(&file);
        let _ = std::fs::remove_file(&file);
        serial
    }

    async fn loaded() -> Option<String> {
        load().await.unwrap().map(|login| login.token)
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
        read(&Storage::File).unwrap()
    }

    fn current() -> Storage {
        state().current()
    }

    /// A directory where the token file goes makes reading and deleting it
    /// fail, standing in for a locked 1Password.
    fn break_token_file() -> PathBuf {
        let file = token_file().unwrap();
        let _ = std::fs::remove_file(&file);
        std::fs::create_dir(&file).unwrap();
        file
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
        // Answers without waiting for the delete.
        assert_eq!(loaded().await, None);
        drop(held);
        wait_cleared();
        assert_eq!(stored(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_login_after_sign_out_survives_the_delayed_delete() {
        let _serial = setup();
        save("OLD".into(), true, epoch()).await.unwrap();
        // The sign-out's delete is held up, say behind a Touch ID prompt, and
        // a new login is saved before it runs.
        let issued = EPOCH.fetch_add(1, SeqCst) + 1;
        save("NEW".into(), true, epoch()).await.unwrap();
        clear(issued);
        assert_eq!(stored().as_deref(), Some("NEW"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_while_signed_out_deletes_the_old_copy() {
        let _serial = setup();
        save("OLD".into(), true, epoch()).await.unwrap();
        let issued = EPOCH.fetch_add(1, SeqCst) + 1;
        set_storage(Storage::Keychain).await.unwrap();
        assert_eq!(current(), Storage::Keychain);
        assert_eq!(stored(), None);
        clear(issued);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_move_keeps_the_setting() {
        let _serial = setup();
        save("OLD".into(), true, epoch()).await.unwrap();
        // The test Keychain refuses writes.
        assert!(set_storage(Storage::Keychain).await.is_err());
        assert_eq!(current(), Storage::File);
        assert_eq!(stored().as_deref(), Some("OLD"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_that_cannot_read_keeps_the_setting() {
        let _serial = setup();
        let file = break_token_file();
        assert!(set_storage(Storage::Keychain).await.is_err());
        assert_eq!(current(), Storage::File);
        std::fs::remove_dir(file).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_login_a_sign_out_could_not_delete_stays_signed_out() {
        let _serial = setup();
        let file = break_token_file();
        sign_out();
        wait_cleared();
        std::fs::remove_dir(&file).unwrap();
        // What the failed delete left behind, now readable again.
        std::fs::write(&file, "OLD").unwrap();
        assert_eq!(loaded().await, None);
        save("NEW".into(), true, epoch()).await.unwrap();
        assert_eq!(loaded().await.as_deref(), Some("NEW"));
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
        assert!(loading.await.unwrap().unwrap().is_none());
        wait_cleared();
        assert_eq!(stored(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_switch_of_a_run_only_adopts_the_setting() {
        let _serial = setup();
        write(&Storage::File, "MINE").unwrap();
        state().storage = None;
        // Nothing to move from the default; the login stays where it is.
        set_storage(Storage::Keychain).await.unwrap();
        assert_eq!(current(), Storage::Keychain);
        assert_eq!(stored().as_deref(), Some("MINE"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_leftover_elsewhere_does_not_hide_the_current_login() {
        let _serial = setup();
        // A sign-out couldn't clear the Keychain.
        state().left_behind.push(Storage::Keychain);
        save("NEW".into(), true, epoch()).await.unwrap();
        assert_eq!(loaded().await.as_deref(), Some("NEW"));
        // Writing the File says nothing about what the Keychain holds.
        assert!(state().left_behind.contains(&Storage::Keychain));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_signed_out_store_that_cannot_be_cleared_can_be_left() {
        let _serial = setup();
        let file = break_token_file();
        sign_out();
        wait_cleared();
        assert!(state().left_behind.contains(&Storage::File));
        // Still broken, but there's nothing to move, so the switch goes ahead.
        set_storage(Storage::Keychain).await.unwrap();
        assert_eq!(current(), Storage::Keychain);
        assert!(state().left_behind.contains(&Storage::File));
        // The next sign-out tries it again.
        std::fs::remove_dir(&file).unwrap();
        std::fs::write(&file, "OLD").unwrap();
        sign_out();
        wait_cleared();
        assert_eq!(stored(), None);
        assert!(state().left_behind.is_empty());
    }
}
