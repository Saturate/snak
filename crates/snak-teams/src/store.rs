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

const KEYCHAIN_SERVICE: &str = "dev.snak.app";
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
    let previous = state().storage.clone();
    if previous == storage {
        return;
    }
    let target = storage.clone();
    let moved = tokio::task::spawn_blocking(move || {
        let Some(token) = read(&previous) else {
            return Ok(false);
        };
        write(&target, &token)?;
        // 1Password keeps its item; deleting there archives it, which is
        // surprising for something the user may have looked at.
        if !matches!(previous, Storage::OnePassword { .. }) {
            delete(&previous);
        }
        Ok::<bool, CoreError>(true)
    })
    .await;
    match moved {
        Ok(Ok(moved)) => {
            let mut state = state();
            state.storage = storage;
            if moved {
                state.last_op_write = Some(Instant::now());
            }
        }
        Ok(Err(e)) => tracing::warn!(error = %e, "login not moved; keeping the old storage"),
        Err(e) => tracing::warn!(error = %e, "login move task failed"),
    }
}

/// Points at a storage without moving anything, for tools that only read.
pub fn use_storage_without_moving(storage: Storage) {
    state().storage = storage;
}

pub async fn load() -> Option<String> {
    let storage = state().storage.clone();
    tokio::task::spawn_blocking(move || read(&storage))
        .await
        .ok()
        .flatten()
}

/// `force` skips the 1Password write throttle, for a fresh login.
pub(crate) async fn save(token: String, force: bool) -> Result<()> {
    let storage = {
        let state = state();
        let throttled = matches!(state.storage, Storage::OnePassword { .. })
            && !force
            && state
                .last_op_write
                .is_some_and(|at| at.elapsed() < OP_WRITE_INTERVAL);
        if throttled {
            return Ok(());
        }
        state.storage.clone()
    };
    let written = tokio::task::spawn_blocking(move || write(&storage, &token))
        .await
        .map_err(|e| CoreError::Keychain(e.to_string()))?;
    if written.is_ok() {
        state().last_op_write = Some(Instant::now());
    }
    written
}

/// Signs out everywhere Snak might have put the login.
pub fn clear() {
    delete(&Storage::Keychain);
    delete(&Storage::File);
    if let Storage::OnePassword { account } = state().storage.clone() {
        let _ = op(&account, &["item", "delete", OP_ITEM, "--archive"], None);
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
            let out = op(
                account,
                &[
                    "item",
                    "get",
                    OP_ITEM,
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
            // `credential` is the field our template creates; `title` is not a
            // field, so asking for it always failed and created duplicates.
            let exists = op(
                account,
                &["item", "get", OP_ITEM, "--fields", "label=credential"],
                None,
            )
            .is_ok();
            let args: &[&str] = if exists {
                &["item", "edit", OP_ITEM, "--template", "/dev/stdin"]
            } else {
                &["item", "create", "--template", "/dev/stdin"]
            };
            op(account, args, Some(&template)).map(|_| ())
        }
    }
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
        Storage::OnePassword { .. } => {}
    }
}

fn keychain_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
        .map_err(|e| CoreError::Keychain(e.to_string()))
}

fn token_file() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join("Library/Application Support/dev.snak.app/teams-refresh-token"))
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
