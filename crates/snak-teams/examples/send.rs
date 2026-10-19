// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. A copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Posts a message to your own self-chat (the "Chat" with yourself) using
//! the stored login, to to stir the push channel while watching frames.
//! Prints only counts, never message text, names or tokens.
//!
//! cargo run -p snak-teams --example send

use std::sync::Arc;

use snak_chat::{ChatBackend, Connector, ConversationId};
use snak_teams::TeamsConnector;

#[tokio::main]
async fn main() {
    let connector = TeamsConnector::new();
    if std::env::var("SNAK_STORE").as_deref() == Ok("file") {
        snak_teams::store::use_storage_without_moving(snak_teams::store::Storage::File);
    }
    let backend: Arc<dyn ChatBackend> = match connector.restore().await {
        Ok(Some(backend)) => backend,
        Ok(None) => return println!("no stored login; sign in through the app first"),
        Err(e) => return println!("connect failed: {}", redact(&e.to_string())),
    };

    let list = match backend.conversations().await {
        Ok(list) => list,
        Err(e) => return println!("conversations failed: {}", redact(&e.to_string())),
    };
    let Some(self_chat) = list.iter().find(|c| !c.is_channel()) else {
        return println!("no chats");
    };
    let id = ConversationId(self_chat.id.0.clone());
    let count = list.len();

    match backend.send(&id, None, "Snak push probe").await {
        Ok(()) => println!("sent to self chat ({count} conversations)"),
        Err(e) => println!("send failed: {}", redact(&e.to_string())),
    }
}

fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for sep in ['?', '#', '&'] {
        if let Some(i) = out.find(sep) {
            out.truncate(i);
        }
    }
    out
}
