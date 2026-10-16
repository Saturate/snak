// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Teams as a `snak_chat` backend: maps Teams' shapes onto the neutral model.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use snak_chat::{
    Body, BoxFuture, ChatBackend, ChatError, Connector, Conversation, ConversationId,
    ConversationKind, Event, LoginPrompt, Me, Message, MessageId, Reaction,
};
use tokio::sync::mpsc;

use crate::auth::{self, DeviceCode};
use crate::models::{self, Chat, ReplyChain, Team};
use crate::{CoreError, PushEvent, TeamsClient, html};

impl From<CoreError> for ChatError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::SessionExpired(detail) => ChatError::SessionExpired(detail),
            other => ChatError::Other(other.to_string()),
        }
    }
}

#[derive(Clone)]
enum Address {
    Channel { team_id: String, channel_id: String },
    Chat { id: String, others: Vec<String> },
}

#[derive(Default)]
struct Directory {
    addresses: HashMap<ConversationId, Address>,
    names: HashMap<String, String>,
    upns: HashMap<String, String>,
}

pub struct TeamsBackend {
    client: Arc<TeamsClient>,
    me: Me,
    directory: Mutex<Directory>,
}

impl TeamsBackend {
    pub fn new(client: TeamsClient) -> Self {
        let me = Me {
            id: client.me().id.clone(),
            display_name: client.me().display_name.clone().unwrap_or_default(),
        };
        Self {
            client: Arc::new(client),
            me,
            directory: Mutex::new(Directory::default()),
        }
    }

    // A poisoned lock only means a panic elsewhere; the maps are still usable.
    fn directory(&self) -> MutexGuard<'_, Directory> {
        self.directory.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn address(&self, id: &ConversationId) -> Result<Address, ChatError> {
        self.directory()
            .addresses
            .get(id)
            .cloned()
            .ok_or_else(|| ChatError::Other(format!("unknown conversation {}", id.0)))
    }

    async fn load_conversations(&self) -> Result<Vec<Conversation>, ChatError> {
        let details = self.client.teams_and_chats().await?;
        let missing = {
            let directory = self.directory();
            unnamed_members(&details.chats, &self.me.id, &directory.names)
        };
        if !missing.is_empty() {
            // Names only make untitled chats readable; a failed lookup isn't fatal.
            if let Ok(people) = self.client.people(&missing).await {
                let mut directory = self.directory();
                for (id, person) in people {
                    if let Some(name) = person.display_name {
                        directory.names.insert(id.clone(), name);
                    }
                    if let Some(upn) = person.upn {
                        directory.upns.insert(id, upn);
                    }
                }
            }
        }

        let mut directory = self.directory();
        let mut list = channel_conversations(&details.teams, &mut directory.addresses);
        let names = directory.names.clone();
        list.extend(chat_conversations(
            &details.chats,
            &self.me.id,
            &names,
            &mut directory.addresses,
        ));
        Ok(list)
    }
}

impl ChatBackend for TeamsBackend {
    fn me(&self) -> &Me {
        &self.me
    }

    fn conversations(&self) -> BoxFuture<'_, Vec<Conversation>> {
        Box::pin(self.load_conversations())
    }

    fn messages<'a>(&'a self, id: &'a ConversationId) -> BoxFuture<'a, Vec<Message>> {
        Box::pin(async move {
            Ok(match self.address(id)? {
                Address::Channel {
                    team_id,
                    channel_id,
                } => channel_messages(&self.client.channel_posts(&team_id, &channel_id).await?),
                Address::Chat { id, .. } => {
                    chat_messages(&self.client.chat_messages(&id).await?.messages)
                }
            })
        })
    }

    fn send<'a>(
        &'a self,
        id: &'a ConversationId,
        reply_to: Option<&'a MessageId>,
        text: &'a str,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let target = match self.address(id)? {
                Address::Channel { channel_id, .. } => channel_id,
                Address::Chat { id, .. } => id,
            };
            let reply_to = reply_to.map(|m| m.0.as_str());
            Ok(self.client.send_message(&target, reply_to, text).await?)
        })
    }

    fn call_url(&self, id: &ConversationId) -> Option<String> {
        let directory = self.directory();
        let Address::Chat { others, .. } = directory.addresses.get(id)? else {
            return None;
        };
        let upns: Vec<&str> = others
            .iter()
            .filter_map(|o| directory.upns.get(o).map(String::as_str))
            .collect();
        // Hands the call to the installed Teams app; Snak has no media stack.
        (!upns.is_empty()).then(|| format!("msteams:/l/call/0/0?users={}", upns.join(",")))
    }

    fn subscribe(self: Arc<Self>) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(64);
        let client = self.client.clone();
        tokio::spawn(async move {
            let mut push = match client.subscribe().await {
                Ok(push) => push,
                Err(CoreError::SessionExpired(_)) => {
                    let _ = tx.send(Event::SessionExpired).await;
                    return;
                }
                Err(_) => {
                    let _ = tx.send(Event::Reconnecting).await;
                    return;
                }
            };
            while let Some(event) = push.recv().await {
                for event in translate(event) {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
            }
        });
        rx
    }
}

fn translate(event: PushEvent) -> Vec<Event> {
    // Channel replies arrive addressed as `<channel>;messageid=<root>`.
    let conversation = |id: &str| ConversationId(id.split(';').next().unwrap_or(id).to_string());
    match event {
        PushEvent::Connected(_) => vec![Event::Connected],
        PushEvent::Reconnecting { .. } => vec![Event::Reconnecting],
        PushEvent::SessionExpired => vec![Event::SessionExpired],
        PushEvent::NewMessage {
            conversation_id, ..
        }
        | PushEvent::MessageEdited {
            conversation_id, ..
        } => vec![
            Event::MessagesChanged(conversation(&conversation_id)),
            Event::ConversationsChanged,
        ],
        PushEvent::ConversationUpdated { .. } => vec![Event::ConversationsChanged],
        PushEvent::Typing { .. } | PushEvent::Presence(_) => Vec::new(),
    }
}

pub struct TeamsConnector {
    http: reqwest::Client,
}

impl TeamsConnector {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    async fn connect(&self, refresh_token: String) -> Result<Arc<dyn ChatBackend>, ChatError> {
        let client = TeamsClient::connect(self.http.clone(), refresh_token).await?;
        let backend: Arc<dyn ChatBackend> = Arc::new(TeamsBackend::new(client));
        Ok(backend)
    }
}

impl Default for TeamsConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl Connector for TeamsConnector {
    fn service_name(&self) -> &str {
        "Microsoft Teams"
    }

    fn restore(&self) -> BoxFuture<'_, Option<Arc<dyn ChatBackend>>> {
        Box::pin(async move {
            match auth::load_refresh_token() {
                Some(token) => self.connect(token).await.map(Some),
                None => Ok(None),
            }
        })
    }

    fn start_login(&self) -> BoxFuture<'_, LoginPrompt> {
        Box::pin(async move {
            let code = auth::start_device_login(&self.http).await?;
            Ok(LoginPrompt {
                code: code.user_code,
                url: code.verification_url,
                handle: code.device_code,
                expires_in_secs: code.expires_in,
                poll_interval_secs: code.interval,
            })
        })
    }

    fn finish_login<'a>(&'a self, prompt: &'a LoginPrompt) -> BoxFuture<'a, Arc<dyn ChatBackend>> {
        Box::pin(async move {
            let code = DeviceCode {
                user_code: prompt.code.clone(),
                device_code: prompt.handle.clone(),
                verification_url: prompt.url.clone(),
                expires_in: prompt.expires_in_secs,
                interval: prompt.poll_interval_secs,
                message: String::new(),
            };
            let token = auth::complete_device_login(&self.http, &code).await?;
            self.connect(token).await
        })
    }

    fn sign_out(&self) {
        auth::clear_refresh_token();
    }
}

// Mapping Teams shapes onto snak_chat

fn preview(message: Option<&models::Message>) -> Option<String> {
    const MAX_CHARS: usize = 160;
    let message = message?;
    let text = html::to_text(message.content.as_deref().unwrap_or(""));
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let line: String = line.chars().take(MAX_CHARS).collect();
    Some(match &message.display_name {
        Some(author) => format!("{author}: {line}"),
        None => line,
    })
}

fn channel_conversations(
    teams: &[Team],
    addresses: &mut HashMap<ConversationId, Address>,
) -> Vec<Conversation> {
    let mut list = Vec::new();
    for team in teams.iter().filter(|t| t.is_archived != Some(true)) {
        for channel in team.channels.iter().filter(|c| c.is_member != Some(false)) {
            let id = ConversationId(channel.id.clone());
            addresses.insert(
                id.clone(),
                Address::Channel {
                    team_id: team.id.clone(),
                    channel_id: channel.id.clone(),
                },
            );
            list.push(Conversation {
                id,
                kind: ConversationKind::Channel {
                    group: team.display_name.clone(),
                },
                title: if channel.is_general == Some(true) {
                    "general".to_string()
                } else {
                    channel.display_name.clone()
                },
                unread: channel.is_unread(),
                muted: channel.is_muted == Some(true),
                last_activity: channel.last_activity(),
                preview: preview(channel.last_message.as_ref()),
            });
        }
    }
    list
}

fn chat_conversations(
    chats: &[Chat],
    me_id: &str,
    names: &HashMap<String, String>,
    addresses: &mut HashMap<ConversationId, Address>,
) -> Vec<Conversation> {
    chats
        .iter()
        .filter(|chat| chat.hidden != Some(true) && chat.is_conversation_deleted != Some(true))
        // Meeting chats belong with the meeting in Teams; they crowd out real DMs.
        .filter(|chat| chat.chat_type.as_deref() != Some("meeting"))
        .map(|chat| {
            let others: Vec<String> = chat
                .members
                .iter()
                .filter_map(|m| m.object_id.clone())
                .filter(|id| id != me_id)
                .collect();
            let id = ConversationId(chat.id.clone());
            addresses.insert(
                id.clone(),
                Address::Chat {
                    id: chat.id.clone(),
                    others: others.clone(),
                },
            );
            let title = chat
                .title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| fallback_title(chat, &others, names));
            Conversation {
                id,
                kind: if chat.is_one_on_one == Some(true) {
                    ConversationKind::Direct
                } else {
                    ConversationKind::Group
                },
                title,
                unread: chat.is_read == Some(false),
                muted: false,
                last_activity: chat.last_message.as_ref().and_then(|m| m.sent_at()),
                preview: preview(chat.last_message.as_ref()),
            }
        })
        .collect()
}

fn fallback_title(chat: &Chat, others: &[String], names: &HashMap<String, String>) -> String {
    let known: Vec<&str> = others
        .iter()
        .filter_map(|id| names.get(id).map(String::as_str))
        .collect();
    if !known.is_empty() {
        return known.join(", ");
    }
    // Until Graph answers, the last sender's name is usually right for a DM.
    chat.last_message
        .as_ref()
        .and_then(|m| m.display_name.clone())
        .unwrap_or_else(|| "Chat".to_string())
}

fn unnamed_members(chats: &[Chat], me_id: &str, names: &HashMap<String, String>) -> Vec<String> {
    let mut ids: Vec<String> = chats
        .iter()
        .flat_map(|chat| chat.members.iter())
        .filter_map(|m| m.object_id.clone())
        .filter(|id| id != me_id && !names.contains_key(id))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn to_message(message: &models::Message) -> Option<Message> {
    let message_type = message.message_type.as_deref().unwrap_or("");
    if message_type.starts_with("Control/") || message_type.starts_with("ThreadActivity/") {
        return None;
    }
    let body = if message.is_deleted() {
        Body::Deleted
    } else if message_type.starts_with("Event/Call") {
        Body::Event("📞 Call".into())
    } else {
        let content = message.content.as_deref().unwrap_or("");
        let subject = message.subject();
        if message_type.starts_with("RichText/Html") {
            let content = display_html(content);
            Body::Html(match subject {
                Some(s) => format!("<p><b>{}</b></p>{content}", html_escape::encode_text(&s)),
                None => content,
            })
        } else {
            let text = html::to_text(content);
            let text = match subject {
                Some(s) if text.is_empty() => s,
                Some(s) => format!("{s}\n{text}"),
                None => text,
            };
            if text.is_empty() {
                return None;
            }
            Body::Text(text)
        }
    };

    Some(Message {
        id: MessageId(message.id.clone().unwrap_or_default()),
        author: message
            .display_name
            .clone()
            .unwrap_or_else(|| "Unknown".into()),
        author_id: message.sender_id().map(str::to_string),
        sent_at: message.sent_at(),
        body,
        edited: message.is_edited(),
        reactions: message
            .reactions()
            .into_iter()
            .map(|r| Reaction {
                emoji: reaction_glyph(&r.key).to_string(),
                count: r.count,
            })
            .collect(),
        replies: Vec::new(),
    })
}

/// Images in Teams messages sit behind Teams auth, so the renderer can't fetch
/// them. Emoji become their alt text; other images become a placeholder.
fn display_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<img") {
        out.push_str(&rest[..start]);
        let Some(len) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[start..start + len + 1];
        if tag.to_ascii_lowercase().contains("schema.skype.com/emoji") {
            out.push_str(&attribute(tag, "alt").unwrap_or_default());
        } else {
            out.push_str("<i>[image]</i>");
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')?;
    Some(tag[start..start + end].to_string())
}

// Teams sends reaction names, not emoji.
fn reaction_glyph(key: &str) -> &str {
    match key {
        "like" => "👍",
        "heart" => "❤️",
        "laugh" => "😆",
        "surprised" => "😮",
        "sad" => "😢",
        "angry" => "😠",
        other => other,
    }
}

/// Chat pages arrive newest first; the timeline wants oldest first.
fn chat_messages(messages: &[models::Message]) -> Vec<Message> {
    let mut list: Vec<Message> = messages.iter().filter_map(to_message).collect();
    list.sort_by_key(|m| m.sent_at);
    list
}

/// One message per channel post, with its replies folded in.
fn channel_messages(chains: &[ReplyChain]) -> Vec<Message> {
    let mut posts: Vec<Message> = chains
        .iter()
        .filter_map(|chain| {
            let is_root = |m: &&models::Message| m.id.as_deref() == Some(chain.id.as_str());
            let mut post = to_message(chain.messages.iter().find(is_root)?)?;
            let replies: Vec<models::Message> = chain
                .messages
                .iter()
                .filter(|m| !is_root(m))
                .cloned()
                .collect();
            post.replies = chat_messages(&replies);
            Some(post)
        })
        .collect();
    posts.sort_by_key(|m| m.sent_at);
    posts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, from: &str, time: &str, kind: &str, content: &str) -> models::Message {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "from": format!("8:orgid:{from}"),
            "imdisplayname": from,
            "originalarrivaltime": time,
            "messagetype": kind,
            "content": content,
        }))
        .expect("valid message")
    }

    fn text(m: &Message) -> &str {
        match &m.body {
            Body::Html(s) | Body::Text(s) | Body::Event(s) => s,
            Body::Deleted => "",
        }
    }

    #[test]
    fn chats_are_oldest_first_and_skip_control_messages() {
        let list = chat_messages(&[
            message(
                "2",
                "a",
                "2026-10-15T10:01:00Z",
                "RichText/Html",
                "<p>second</p>",
            ),
            message("x", "a", "2026-10-15T10:00:30Z", "Control/Typing", ""),
            message("1", "a", "2026-10-15T10:00:00Z", "Text", "first"),
        ]);
        let texts: Vec<&str> = list.iter().map(text).collect();
        assert_eq!(texts, ["first", "<p>second</p>"]);
    }

    #[test]
    fn channel_replies_fold_under_their_post() {
        let chain: ReplyChain = serde_json::from_value(serde_json::json!({
            "id": "1",
            "messages": [
                { "id": "3", "from": "8:orgid:b", "originalarrivaltime": "2026-10-15T10:02:00Z", "messagetype": "Text", "content": "reply two" },
                { "id": "2", "from": "8:orgid:b", "originalarrivaltime": "2026-10-15T10:01:00Z", "messagetype": "Text", "content": "reply one" },
                { "id": "1", "from": "8:orgid:a", "originalarrivaltime": "2026-10-15T10:00:00Z", "messagetype": "Text", "content": "post" },
            ]
        }))
        .expect("valid chain");
        let posts = channel_messages(&[chain]);
        assert_eq!(posts.len(), 1);
        assert_eq!(text(&posts[0]), "post");
        let replies: Vec<&str> = posts[0].replies.iter().map(text).collect();
        assert_eq!(replies, ["reply one", "reply two"]);
    }

    #[test]
    fn images_are_replaced_and_emoji_keep_alt_text() {
        let html = r#"<p>hi <img itemtype="http://schema.skype.com/Emoji" alt="😀" src="x"> <img src="https://asm/x"></p>"#;
        assert_eq!(display_html(html), "<p>hi 😀 <i>[image]</i></p>");
    }

    #[test]
    fn channel_list_uses_read_flag_and_message_id_time() {
        // Mirrors the live shape: lastMessage present but all-null.
        let team: Team = serde_json::from_value(serde_json::json!({
            "id": "t", "displayName": "Team",
            "channels": [
                { "id": "c1", "displayName": "a", "isMessageRead": false, "isMuted": true,
                  "lastL2MessageIdNFS": 1_760_000_000_000_i64,
                  "lastMessage": { "id": null, "content": null, "originalArrivalTime": null } },
                { "id": "c2", "displayName": "b", "isMessageRead": true, "lastL2MessageIdNFS": 42 },
            ]
        }))
        .expect("valid team");
        let list = channel_conversations(&[team], &mut HashMap::new());
        assert!(list[0].unread && list[0].muted);
        assert!(list[0].last_activity.is_some());
        assert!(list[0].preview.is_none());
        assert!(!list[1].unread);
        assert!(
            list[1].last_activity.is_none(),
            "small IDs are not timestamps"
        );
    }

    #[test]
    fn channel_reply_pushes_map_to_the_channel() {
        let events = translate(PushEvent::ConversationUpdated {
            conversation_id: "19:a@thread.tacv2".into(),
        });
        assert!(matches!(events.as_slice(), [Event::ConversationsChanged]));
        let message = message("9", "a", "2026-10-15T10:00:00Z", "Text", "x");
        let events = translate(PushEvent::NewMessage {
            conversation_id: "19:a@thread.tacv2;messageid=123".into(),
            message,
        });
        assert!(matches!(&events[0], Event::MessagesChanged(id) if id.0 == "19:a@thread.tacv2"));
    }
}
