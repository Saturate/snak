// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Lists chats from the stored login by a person's name, printing only
//! shapes: kinds, flags and counts, never names, message text or tokens.
//!
//! SNAK_FIND="name" cargo run -p snak-teams --example find-chat

mod common;

use common::redact;
use std::sync::Arc;

const SNAK_STORE: &str = "SNAK_STORE";
const SNAK_FIND: &str = "SNAK_FIND";

use snak_chat::{ChatBackend, Connector, ConversationKind};
use snak_teams::TeamsConnector;

#[tokio::main]
async fn main() {
    let Some(name) = std::env::var(SNAK_FIND).ok() else {
        return println!("set SNAK_FIND to the name to look for");
    };
    let name = name.to_lowercase();
    if std::env::var(SNAK_STORE).as_deref() == Ok("file") {
        snak_teams::store::use_storage_without_moving(snak_teams::store::Storage::File);
    }
    let backend: Arc<dyn ChatBackend> = match TeamsConnector::new().restore().await {
        Ok(Some(backend)) => backend,
        Ok(None) => return println!("no stored login; sign in through the app first"),
        Err(e) => return println!("connect failed: {}", redact(&e.to_string())),
    };
    let list = match backend.conversations().await {
        Ok(list) => list,
        Err(e) => return println!("conversations failed: {}", redact(&e.to_string())),
    };
    let chats: Vec<_> = list.iter().filter(|c| !c.is_channel()).collect();
    println!("chats: {}", chats.len());
    for chat in chats {
        let matched = chat.title.to_lowercase().contains(&name);
        if matched {
            println!(
                "chat: kind={:?} unread={} muted={} matched={}",
                match chat.kind {
                    ConversationKind::Direct => "direct",
                    ConversationKind::Group => "group",
                    ConversationKind::Channel { .. } => "channel",
                },
                chat.unread,
                chat.muted,
                matched
            );
        }
    }
}
