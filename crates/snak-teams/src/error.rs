// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// The refresh token is gone or rejected; the user has to log in again.
    #[error("session expired: {0}")]
    SessionExpired(String),
    #[error("device code login timed out")]
    LoginTimedOut,
    #[error("HTTP {status} from {url}: {body}")]
    Http {
        status: u16,
        url: String,
        body: String,
    },
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    #[error("unexpected response from {url}: {source}")]
    Decode {
        url: String,
        source: serde_json::Error,
    },
    #[error("unexpected response from {url}: missing {field}")]
    MissingField { url: String, field: &'static str },
    /// The push connection failed; it reconnects on its own.
    #[error("push channel: {0}")]
    Push(String),
    #[error("keychain: {0}")]
    Keychain(String),
    /// A link from a response body pointed somewhere the token doesn't go.
    #[error("refusing to follow a link outside the chat service")]
    UntrustedLink,
    #[error("background task stopped: {0}")]
    TaskFailed(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

// Error codes Entra ID returns when the refresh token can no longer be used.
const EXPIRED_MARKERS: [&str; 5] = [
    "AADSTS700082",
    "AADSTS50173",
    "AADSTS70000",
    "AADSTS700084",
    "invalid_grant",
];

pub(crate) fn is_expired_session(body: &str) -> bool {
    EXPIRED_MARKERS.iter().any(|marker| body.contains(marker))
}
