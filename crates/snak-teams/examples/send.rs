// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. A copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Posts a probe message to the conversation named by SNAK_SEND_TO using
//! the stored login, to stir the push channel while watching frames.
//! Prints only counts, never message text, names or tokens.
//!
//! SNAK_SEND_TO="48:notes" cargo run -p snak-teams --example send

mod common;

use std::sync::Arc;

use common::redact;
use snak_chat::{ChatBackend, Connector, ConversationId};
use snak_teams::TeamsConnector;

const SNAK_STORE: &str = "SNAK_STORE";
const SNAK_SEND_TO: &str = "SNAK_SEND_TO";

#[tokio::main]
async fn main() {
    // No default target: the first chat in the list is usually a real
    // person, and a probe must never message someone by accident.
    let Some(id) = std::env::var(SNAK_SEND_TO).ok().filter(|id| !id.is_empty()) else {
        return println!("set {SNAK_SEND_TO} to the conversation id to post the probe in");
    };
    let id = ConversationId(id);
    if std::env::var(SNAK_STORE).as_deref() == Ok("file") {
        snak_teams::store::use_storage_without_moving(snak_teams::store::Storage::File);
    }
    let backend: Arc<dyn ChatBackend> = match TeamsConnector::new().restore().await {
        Ok(Some(backend)) => backend,
        Ok(None) => return println!("no stored login; sign in through the app first"),
        Err(e) => return println!("connect failed: {}", redact(&e.to_string())),
    };

    match backend.send(&id, None, "Snak push probe").await {
        Ok(()) => println!("sent"),
        Err(e) => println!("send failed: {}", redact(&e.to_string())),
    }
}
