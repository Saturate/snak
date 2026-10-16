// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Prints the key structure of Teams responses: names and types only, no values.
//!
//! cargo run -p snak-teams --example shape

use serde_json::Value;
use snak_teams::TeamsClient;

fn shape(value: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                let kind = match v {
                    Value::Object(_) => "{}",
                    Value::Array(a) => &format!("[{}]", a.len()),
                    Value::String(s) if s.is_empty() => "\"\"",
                    Value::String(_) => "str",
                    Value::Number(_) => "num",
                    Value::Bool(_) => "bool",
                    Value::Null => "null",
                };
                out.push_str(&format!("{pad}{key}: {kind}\n"));
                if indent < 12 {
                    match v {
                        Value::Object(_) => shape(v, indent + 2, out),
                        Value::Array(a) => {
                            if let Some(first) = a.first() {
                                shape(first, indent + 2, out)
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Value::Array(a) => {
            if let Some(first) = a.first() {
                shape(first, indent, out)
            }
        }
        _ => {}
    }
}

#[tokio::main]
async fn main() {
    let Some(login) = snak_teams::store::load().await.ok().flatten() else {
        return println!("no stored login");
    };
    let client = match TeamsClient::connect(reqwest::Client::new(), login).await {
        Ok(c) => c,
        Err(e) => return println!("connect failed: {e}"),
    };
    let details = match client.raw_teams_and_chats().await {
        Ok(v) => v,
        Err(e) => return println!("teams failed: {e}"),
    };

    // The first channel with a lastMessage, and the busiest-looking team channel.
    let channels: Vec<(&Value, &Value)> = details["teams"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|t| {
            t["channels"]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |c| (t, c))
        })
        .collect();
    let with_last = channels
        .iter()
        .filter(|(_, c)| !c["lastMessage"].is_null())
        .count();
    println!(
        "channels: {}, with lastMessage: {with_last}",
        channels.len()
    );
    if let Some((_, channel)) = channels.iter().find(|(_, c)| !c["lastMessage"].is_null()) {
        let mut out = String::new();
        shape(channel, 2, &mut out);
        println!("channel object:\n{out}");
    }

    let mut best: Option<(usize, usize)> = None;
    for (i, (team, channel)) in channels.iter().enumerate().take(12) {
        let (Some(t), Some(c)) = (team["id"].as_str(), channel["id"].as_str()) else {
            continue;
        };
        let Ok(page) = client.raw_channel_posts(t, c).await else {
            continue;
        };
        let chains = page["replyChains"].as_array().map(Vec::len).unwrap_or(0);
        let max_msgs = page["replyChains"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| r["messages"].as_array().map(Vec::len).unwrap_or(0))
            .max()
            .unwrap_or(0);
        println!("channel #{i}: {chains} reply chains, largest has {max_msgs} messages");
        if best.is_none_or(|(_, m)| max_msgs > m) {
            best = Some((i, max_msgs));
        }
    }
    if let Some((i, _)) = best {
        let (team, channel) = channels[i];
        if let (Some(t), Some(c)) = (team["id"].as_str(), channel["id"].as_str())
            && let Ok(page) = client.raw_channel_posts(t, c).await
        {
            let mut top = String::new();
            for key in page.as_object().into_iter().flat_map(|m| m.keys()) {
                top.push_str(&format!("  {key}\n"));
            }
            println!("channel page top-level keys:\n{top}");
            if let Some(chain) = page["replyChains"].as_array().and_then(|a| {
                a.iter()
                    .max_by_key(|r| r["messages"].as_array().map(Vec::len))
            }) {
                let mut out = String::new();
                shape(chain, 2, &mut out);
                println!("largest reply chain:\n{out}");
            }
        }
    }
}
