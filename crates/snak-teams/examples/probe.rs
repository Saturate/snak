// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Checks the Teams endpoints against a real account using the stored login.
//! Prints only counts and shapes, never message text, names or tokens.
//!
//! cargo run -p snak-teams --example probe

use std::sync::Arc;
use std::time::Duration;

const SNAK_STORE: &str = "SNAK_STORE";

use snak_chat::{Body, ChatBackend, Connector, Event};
use snak_teams::TeamsConnector;

#[tokio::main]
async fn main() {
    let connector = TeamsConnector::new();
    // Use the same place the app keeps the login: SNAK_STORE=file|keychain.
    // Without moving: a probe must never relocate the app's login.
    if std::env::var(SNAK_STORE).as_deref() == Ok("file") {
        snak_teams::store::use_storage_without_moving(snak_teams::store::Storage::File);
    }
    let backend: Arc<dyn ChatBackend> = match connector.restore().await {
        Ok(Some(backend)) => backend,
        Ok(None) => return println!("no stored login; sign in through the app first"),
        Err(e) => return println!("connect failed: {}", redact(&e.to_string())),
    };
    println!(
        "connect: ok (me id present: {})",
        !backend.me().id.is_empty()
    );

    let list = match backend.conversations().await {
        Ok(list) => list,
        Err(e) => return println!("conversations failed: {}", redact(&e.to_string())),
    };
    let channels: Vec<_> = list.iter().filter(|c| c.is_channel()).collect();
    let chats: Vec<_> = list.iter().filter(|c| !c.is_channel()).collect();
    println!(
        "conversations: {} channels ({} unread, {} with activity time, {} with preview), {} chats ({} unread, {} titled 'Chat')",
        channels.len(),
        channels.iter().filter(|c| c.unread).count(),
        channels
            .iter()
            .filter(|c| c.last_activity.is_some())
            .count(),
        channels.iter().filter(|c| c.preview.is_some()).count(),
        chats.len(),
        chats.iter().filter(|c| c.unread).count(),
        chats.iter().filter(|c| c.title == "Chat").count(),
    );
    println!(
        "calls: {} of {} chats have a call link",
        chats
            .iter()
            .filter(|c| backend.call_url(&c.id).is_some())
            .count(),
        chats.len()
    );

    let newest = |items: &Vec<&snak_chat::Conversation>| {
        items
            .iter()
            .max_by_key(|c| c.last_activity)
            .map(|c| c.id.clone())
    };
    for (label, id) in [("channel", newest(&channels)), ("chat", newest(&chats))] {
        let Some(id) = id else { continue };
        match backend.messages(&id).await {
            Ok(messages) => {
                let kinds = messages.iter().fold([0; 4], |mut k, m| {
                    k[match m.body {
                        Body::Html(_) => 0,
                        Body::Text(_) => 1,
                        Body::Deleted => 2,
                        Body::Event(_) => 3,
                    }] += 1;
                    k
                });
                println!(
                    "{label} messages: {} (html {}, text {}, deleted {}, events {}), {} with replies, {} replies total, {} with author id",
                    messages.len(),
                    kinds[0],
                    kinds[1],
                    kinds[2],
                    kinds[3],
                    messages.iter().filter(|m| !m.replies.is_empty()).count(),
                    messages.iter().map(|m| m.replies.len()).sum::<usize>(),
                    messages.iter().filter(|m| m.author_id.is_some()).count(),
                );
            }
            Err(e) => println!("{label} messages failed: {}", redact(&e.to_string())),
        }
    }

    // First embedded image in the newest few conversations, to check fetching works.
    let mut by_recent: Vec<_> = list.iter().collect();
    by_recent.sort_by_key(|c| std::cmp::Reverse(c.last_activity));
    'search: for conversation in by_recent.iter().take(15) {
        let Ok(messages) = backend.messages(&conversation.id).await else {
            continue;
        };
        for message in messages
            .iter()
            .flat_map(|m| std::iter::once(m).chain(m.replies.iter()))
        {
            if let Some(image) = message.images.first() {
                match backend.image(&image.url).await {
                    Ok(bytes) => println!(
                        "image: fetched {} bytes, magic {:02x?}, reported size {:?}x{:?}",
                        bytes.len(),
                        &bytes[..bytes.len().min(4)],
                        image.width,
                        image.height
                    ),
                    Err(e) => println!("image: fetch failed: {}", redact(&e.to_string())),
                }
                break 'search;
            }
        }
    }

    println!("push: connecting, listening 20s…");
    let mut events = backend.clone().subscribe();
    let deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break println!("push: done"),
            event = events.recv() => match event {
                Some(Event::Connected) => println!("push: connected"),
                Some(Event::Reconnecting) => println!("push: reconnecting"),
                Some(Event::SessionExpired) => break println!("push: session expired"),
                Some(Event::MessagesChanged(_)) => println!("push: messages changed"),
                Some(Event::ConversationsChanged) => println!("push: conversations changed"),
                None => break println!("push: channel closed"),
            },
        }
    }
}

// Error bodies can echo request details; keep the status and the first line only.
fn redact(error: &str) -> String {
    error
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}
