// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Lists the kinds of <img> tags in recent messages: itemtype and host only.
//!
//! SNAK_STORE=file cargo run -p snak-teams --example images

use std::collections::BTreeMap;

use snak_teams::{TeamsClient, store};

#[tokio::main]
async fn main() {
    // Without moving: a probe must never relocate the app's login.
    if std::env::var("SNAK_STORE").as_deref() == Ok("file") {
        store::use_storage_without_moving(store::Storage::File);
    }
    let Some(login) = store::load().await.ok().flatten() else {
        return println!("no stored login");
    };
    let client = match TeamsClient::connect(reqwest::Client::new(), login).await {
        Ok(c) => c,
        Err(e) => return println!("connect failed: {e}"),
    };
    let Ok(details) = client.teams_and_chats().await else {
        return println!("list failed");
    };
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut scan = |html: &str| {
        let mut rest = html;
        while let Some(start) = rest.find("<img") {
            let Some(len) = rest[start..].find('>') else {
                break;
            };
            let tag = &rest[start..start + len + 1];
            let attr = |name: &str| {
                let needle = format!("{name}=\"");
                tag.find(&needle).and_then(|i| {
                    let v = &tag[i + needle.len()..];
                    v.find('"').map(|e| v[..e].to_string())
                })
            };
            let itemtype = attr("itemtype").unwrap_or_else(|| "-".into());
            let host = attr("src")
                .and_then(|s| s.split('/').nth(2).map(str::to_string))
                .unwrap_or_else(|| "-".into());
            *counts.entry((itemtype, host)).or_default() += 1;
            rest = &rest[start + len + 1..];
        }
    };
    for chat in details.chats.iter().take(10) {
        if let Ok(page) = client.chat_messages(&chat.id).await {
            page.messages
                .iter()
                .filter_map(|m| m.content.as_deref())
                .for_each(&mut scan);
        }
    }
    for team in details.teams.iter().take(6) {
        for channel in team.channels.iter().take(4) {
            if let Ok(chains) = client.channel_posts(&team.id, &channel.id).await {
                chains
                    .iter()
                    .flat_map(|c| c.messages.iter())
                    .filter_map(|m| m.content.as_deref())
                    .for_each(&mut scan);
            }
        }
    }
    for ((itemtype, host), n) in counts {
        println!("{n:>4}  {itemtype}  {host}");
    }
}
