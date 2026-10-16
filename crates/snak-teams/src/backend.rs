// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Teams as a `snak_chat` backend: maps Teams' shapes onto the neutral model.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use snak_chat::{
    Body, BoxFuture, ChatBackend, ChatError, Connector, Conversation, ConversationId,
    ConversationKind, CredentialStorage, Event, Group, ImageRef, LoginPrompt, Me, Message,
    MessageId, Reaction,
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
                } => channel_messages(
                    &self.client.channel_posts(&team_id, &channel_id).await?,
                    &self.me.id,
                ),
                Address::Chat { id, .. } => {
                    chat_messages(&self.client.chat_messages(&id).await?.messages, &self.me.id)
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

    fn image<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Vec<u8>> {
        Box::pin(async move { Ok(self.client.image(url).await?) })
    }

    fn mark_read<'a>(&'a self, id: &'a ConversationId) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let target = match self.address(id)? {
                Address::Channel { channel_id, .. } => channel_id,
                Address::Chat { id, .. } => id,
            };
            Ok(self.client.mark_read(&target).await?)
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
        // Guest UPNs contain `#EXT#`, which would otherwise start a URL fragment.
        let users: Vec<String> = upns
            .iter()
            .map(|u| url::form_urlencoded::byte_serialize(u.as_bytes()).collect())
            .collect();
        (!users.is_empty()).then(|| format!("msteams:/l/call/0/0?users={}", users.join(",")))
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
        let client = TeamsClient::connect(self.http.clone(), refresh_token)
            .await
            .inspect_err(|e| tracing::error!(error = %e, "Teams connect failed"))?;
        tracing::info!(region = client.region(), "connected to Teams");
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
            match crate::store::load().await {
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
        crate::store::clear();
    }

    fn set_credential_storage(&self, storage: CredentialStorage) -> BoxFuture<'_, ()> {
        let storage = match storage {
            CredentialStorage::Keychain => crate::store::Storage::Keychain,
            CredentialStorage::File => crate::store::Storage::File,
            CredentialStorage::OnePassword { account } => {
                crate::store::Storage::OnePassword { account }
            }
        };
        Box::pin(async move {
            crate::store::set_storage(storage).await;
            Ok(())
        })
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
                    group: Group {
                        id: team.id.clone(),
                        name: team.display_name.clone(),
                    },
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

fn to_message(message: &models::Message, me_id: &str) -> Option<Message> {
    // Mentions name people by MRI (8:orgid:<object id>); ours is the object id.
    let my_mentions: Vec<String> = message
        .mentions()
        .into_iter()
        .filter(|(_, mri)| !me_id.is_empty() && mri.rsplit(':').next() == Some(me_id))
        .map(|(item, _)| item)
        .collect();
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
            let content = display_html(content, &my_mentions);
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
        // Without an id a message can't anchor a thread or be told apart
        // from another, so it is skipped.
        id: MessageId(message.id.clone()?),
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
        images: if message_type.starts_with("RichText/Html") {
            message_images(message.content.as_deref().unwrap_or(""))
        } else {
            Vec::new()
        },
        replies: Vec::new(),
        pending: false,
        mentions_me: !my_mentions.is_empty(),
    })
}

/// Which kind of media host an image URL points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaHost {
    /// Microsoft's message media store; takes the Skype token.
    Teams,
    /// Giphy's public CDN, which Teams' GIF picker links to; no credentials.
    Giphy,
}

/// Classifies an `<img src>` value as it appears in message HTML.
///
/// Message HTML is written by other people, so this decodes entities first and
/// parses once with the same URL parser reqwest uses, then checks the parsed
/// host. The caller fetches the returned `Url`, never the raw string: checking
/// one representation and fetching another is how `\` or `&sol;` tricks would
/// steer the token to another host.
pub(crate) fn classify_media_url(raw: &str) -> Option<(url::Url, MediaHost)> {
    classify_parsed_url(html_escape::decode_html_entities(raw).trim())
}

/// The same check for a URL that is already plain text, such as one taken from
/// an `ImageRef`; decoding it again would turn a literal `&amp;` into `&`.
pub(crate) fn classify_parsed_url(text: &str) -> Option<(url::Url, MediaHost)> {
    let url = url::Url::parse(text).ok()?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let Some(url::Host::Domain(host)) = url.host() else {
        return None;
    };
    let host = host.to_ascii_lowercase();
    let kind = if host.ends_with(".asm.skype.com") || host.ends_with(".asyncgw.teams.microsoft.com")
    {
        MediaHost::Teams
    } else if host == "giphy.com"
        || host
            .strip_suffix(".giphy.com")
            .is_some_and(|sub| sub == "i" || (sub.starts_with("media") && !sub.contains('.')))
    {
        MediaHost::Giphy
    } else {
        return None;
    };
    Some((url, kind))
}

/// Byte offsets of each `<img ...>` tag, case-insensitively.
fn img_tags(html: &str) -> Vec<(usize, usize)> {
    // ASCII lowercasing keeps byte offsets identical to the original.
    let lower = html.to_ascii_lowercase();
    let mut tags = Vec::new();
    let mut from = 0;
    while let Some(found) = lower[from..].find("<img") {
        let start = from + found;
        let Some(len) = lower[start..].find('>') else {
            break;
        };
        tags.push((start, start + len + 1));
        from = start + len + 1;
    }
    tags
}

fn message_images(html: &str) -> Vec<ImageRef> {
    img_tags(html)
        .into_iter()
        .filter_map(|(start, end)| {
            let tag = &html[start..end];
            if tag.to_ascii_lowercase().contains("schema.skype.com/emoji") {
                return None;
            }
            let (url, _) = classify_media_url(&attribute(tag, "src")?)?;
            let size = |name| attribute(tag, name).and_then(|v| v.parse::<f32>().ok());
            Some(ImageRef {
                url: url.to_string(),
                width: size("width"),
                height: size("height"),
            })
        })
        .collect()
}

/// Images in Teams messages sit behind Teams auth, so the renderer can't fetch
/// them. Emoji become their alt text, media we fetch is drawn under the text,
/// and every other image becomes a placeholder: no `<img>` reaches the renderer.
fn display_html(html: &str, my_mentions: &[String]) -> String {
    let html = &render_mentions(html, my_mentions);
    let mut out = String::with_capacity(html.len());
    let mut copied = 0;
    for (start, end) in img_tags(html) {
        out.push_str(&html[copied..start]);
        let tag = &html[start..end];
        if tag.to_ascii_lowercase().contains("schema.skype.com/emoji") {
            // The alt text is attacker-written and never re-scanned, so it goes
            // back in as text, not markup.
            let alt = attribute(tag, "alt").unwrap_or_default();
            out.push_str(&html_escape::encode_text(
                &html_escape::decode_html_entities(&alt),
            ));
        } else if attribute(tag, "src")
            .and_then(|u| classify_media_url(&u))
            .is_none()
        {
            out.push_str("<i>[image]</i>");
        }
        copied = end;
    }
    out.push_str(&html[copied..]);
    collapse_blank_paragraphs(&out)
}

/// Teams writes a blank line as `<p>&nbsp;</p>`; keep one, drop the rest.
fn collapse_blank_paragraphs(html: &str) -> String {
    const BLANKS: [&str; 3] = ["<p>&nbsp;</p>", "<p></p>", "<p><br></p>"];
    let mut out = html.to_string();
    for blank in BLANKS {
        out = out.replace(blank, "<p>&nbsp;</p>");
    }
    let doubled = "<p>&nbsp;</p><p>&nbsp;</p>";
    while out.contains(doubled) {
        out = out.replace(doubled, "<p>&nbsp;</p>");
    }
    out.trim_end_matches("<p>&nbsp;</p>").to_string()
}

/// Teams marks a mention as `<span itemtype="http://schema.skype.com/Mention"
/// itemid="N">Name</span>`, and splits a full name into one span per word that
/// share the itemid. Each run becomes one `@Full Name` link to `snak-mention:N`,
/// bold when it's the signed-in user. The link scheme is never opened.
fn render_mentions(html: &str, my_mentions: &[String]) -> String {
    const MARKER: &str = "schema.skype.com/mention";
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut copied = 0;
    let mut at = 0;
    while let Some(found) = lower[at..].find("<span") {
        let start = at + found;
        let Some(open_len) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + open_len + 1;
        let tag = &html[start..open_end];
        if !tag.to_ascii_lowercase().contains(MARKER) {
            at = open_end;
            continue;
        }
        let Some(item) = attribute(tag, "itemid") else {
            at = open_end;
            continue;
        };
        // Collect this span and any directly following spans for the same person.
        let mut words = Vec::new();
        let mut end = start;
        let mut cursor = start;
        while let Some(open_len) = lower[cursor..].find('>') {
            let inner_start = cursor + open_len + 1;
            let Some(close) = lower[inner_start..].find("</span>") else {
                break;
            };
            let inner = &html[inner_start..inner_start + close];
            words.push(crate::html::to_text(inner));
            end = inner_start + close + "</span>".len();
            // Next span for the same itemid, separated only by spaces?
            let rest = &lower[end..];
            let gap = rest.len()
                - rest
                    .trim_start_matches(|c: char| c.is_whitespace())
                    .trim_start_matches("&nbsp;")
                    .trim_start_matches(|c: char| c.is_whitespace())
                    .len();
            let next = end + gap;
            let same_person = lower[next..].starts_with("<span")
                && lower[next..].find('>').is_some_and(|next_len| {
                    let next_tag = &html[next..next + next_len + 1];
                    next_tag.to_ascii_lowercase().contains(MARKER)
                        && attribute(next_tag, "itemid").as_deref() == Some(item.as_str())
                });
            if !same_person {
                break;
            }
            cursor = next;
        }
        if words.is_empty() {
            at = open_end;
            continue;
        }
        let name = words
            .iter()
            .map(|w| w.trim())
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&html[copied..start]);
        let label = html_escape::encode_text(&name);
        let item = html_escape::encode_double_quoted_attribute(&item);
        if my_mentions.iter().any(|m| m.as_str() == item.as_ref()) {
            out.push_str(&format!(
                "<a href=\"snak-mention:{item}\"><b>@{label}</b></a>"
            ));
        } else {
            out.push_str(&format!("<a href=\"snak-mention:{item}\">@{label}</a>"));
        }
        copied = end;
        at = end;
    }
    out.push_str(&html[copied..]);
    out
}

/// A quoted attribute value. Only matches the whole attribute name, so
/// `data-src` is not mistaken for `src`.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{}=\"", name.to_ascii_lowercase());
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let at = from + found;
        let standalone = at > 0 && lower.as_bytes()[at - 1].is_ascii_whitespace();
        let start = at + needle.len();
        if standalone {
            let end = tag[start..].find('"')?;
            return Some(tag[start..start + end].to_string());
        }
        from = start;
    }
    None
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
fn chat_messages(messages: &[models::Message], me_id: &str) -> Vec<Message> {
    let mut list: Vec<Message> = messages
        .iter()
        .filter_map(|m| to_message(m, me_id))
        .collect();
    list.sort_by_key(|m| m.sent_at);
    list
}

/// One message per channel post, with its replies folded in.
fn channel_messages(chains: &[ReplyChain], me_id: &str) -> Vec<Message> {
    let mut posts: Vec<Message> = chains
        .iter()
        .filter_map(|chain| {
            let is_root = |m: &&models::Message| m.id.as_deref() == Some(chain.id.as_str());
            let mut post = to_message(chain.messages.iter().find(is_root)?, me_id)?;
            let replies: Vec<models::Message> = chain
                .messages
                .iter()
                .filter(|m| !is_root(m))
                .cloned()
                .collect();
            post.replies = chat_messages(&replies, me_id);
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
        let list = chat_messages(
            &[
                message(
                    "2",
                    "a",
                    "2026-10-15T10:01:00Z",
                    "RichText/Html",
                    "<p>second</p>",
                ),
                message("x", "a", "2026-10-15T10:00:30Z", "Control/Typing", ""),
                message("1", "a", "2026-10-15T10:00:00Z", "Text", "first"),
            ],
            "me",
        );
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
        let posts = channel_messages(&[chain], "me");
        assert_eq!(posts.len(), 1);
        assert_eq!(text(&posts[0]), "post");
        let replies: Vec<&str> = posts[0].replies.iter().map(text).collect();
        assert_eq!(replies, ["reply one", "reply two"]);
    }

    #[test]
    fn images_are_replaced_and_emoji_keep_alt_text() {
        let html = r#"<p>hi <img itemtype="http://schema.skype.com/Emoji" alt="😀" src="x"> <img src="https://asm/x"></p>"#;
        assert_eq!(display_html(html, &[]), "<p>hi 😀 <i>[image]</i></p>");
    }

    #[test]
    fn only_microsoft_media_hosts_get_the_token() {
        let teams = |u| classify_media_url(u).map(|(_, k)| k) == Some(MediaHost::Teams);
        assert!(teams(
            "https://eu-api.asm.skype.com/v1/objects/0-weu-d1-abc/views/imgo"
        ));
        assert!(teams(
            "https://eu-prod.asyncgw.teams.microsoft.com/v1/objects/x"
        ));
        for bad in [
            "http://eu-api.asm.skype.com/v1/objects/x",
            "https://evil.example/eu-api.asm.skype.com",
            "https://eu-api.asm.skype.com.evil.example/x",
            "https://eu-api.asm.skype.com@evil.example/x",
            "https://eu-api.asm.skype.com:8443/x",
            // Found in review: each of these passed the old string check while
            // the fetched URL pointed elsewhere.
            "https://attacker.example\\.asm.skype.com/x",
            "https://attacker.example&sol;.asm.skype.com/x",
            "https://attacker.example&quest;.asm.skype.com",
            "https://127.0.0.1\\.asm.skype.com/x",
            "https://localhost&colon;8080&sol;.asm.skype.com",
            "javascript:alert(1)//.asm.skype.com",
        ] {
            assert!(classify_media_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_fetched_url_is_the_checked_one() {
        let (url, _) = classify_media_url("https://eu-api.asm.skype.com/v1/objects/a?x=1&amp;y=2")
            .expect("allowed");
        assert_eq!(url.host_str(), Some("eu-api.asm.skype.com"));
        assert_eq!(url.query(), Some("x=1&y=2"));
    }

    #[test]
    fn giphy_is_allowed_without_the_token() {
        let giphy = |u| classify_media_url(u).map(|(_, k)| k) == Some(MediaHost::Giphy);
        assert!(giphy("https://media2.giphy.com/media/abc/giphy.gif"));
        assert!(giphy("https://i.giphy.com/abc.gif"));
        assert!(!giphy("https://media2.giphy.com.evil.example/x.gif"));
        assert!(!giphy("https://evilgiphy.com/x.gif"));
        assert!(!giphy("https://media.attacker.example\\.giphy.com/x"));
        assert!(!giphy("https://media.evil.giphy.com/x"));
    }

    #[test]
    fn split_mention_spans_become_one_mention() {
        let html = concat!(
            r#"<p>hi <span itemtype="http://schema.skype.com/Mention" itemscope="" itemid="0">Allan</span>&nbsp;"#,
            r#"<span itemtype="http://schema.skype.com/Mention" itemscope="" itemid="0">Kimmer</span> "#,
            r#"<span itemtype="http://schema.skype.com/Mention" itemscope="" itemid="0">Jensen</span> and "#,
            r#"<span itemtype="http://schema.skype.com/Mention" itemid="1">Maja</span></p>"#,
        );
        assert_eq!(
            display_html(html, &["0".to_string()]),
            r#"<p>hi <a href="snak-mention:0"><b>@Allan Kimmer Jensen</b></a> and <a href="snak-mention:1">@Maja</a></p>"#
        );
    }

    #[test]
    fn mention_names_are_escaped() {
        let html = r#"<span itemtype="http://schema.skype.com/Mention" itemid="0">&lt;img src=x&gt;</span>"#;
        assert!(!display_html(html, &[]).contains("<img"));
    }

    #[test]
    fn messages_that_mention_me_are_flagged() {
        let message: models::Message = serde_json::from_value(serde_json::json!({
            "id": "1", "from": "8:orgid:other", "messagetype": "RichText/Html",
            "originalarrivaltime": "2026-10-15T10:00:00Z",
            "content": r#"<p><span itemtype="http://schema.skype.com/Mention" itemid="0">Allan</span></p>"#,
            "properties": { "mentions": r#"[{"itemid":"0","mri":"8:orgid:me-guid","mentionType":"person"}]"# }
        }))
        .expect("parses");
        assert!(to_message(&message, "me-guid").expect("kept").mentions_me);
        assert!(
            !to_message(&message, "someone-else")
                .expect("kept")
                .mentions_me
        );
    }

    #[test]
    fn emoji_alt_text_cannot_carry_markup() {
        let html = r#"<p><img itemtype="http://schema.skype.com/Emoji" alt="<img src=https://tracker.example/p.gif x="></p>"#;
        let out = display_html(html, &[]);
        assert!(!out.to_ascii_lowercase().contains("<img"), "{out}");
    }

    #[test]
    fn stored_image_urls_are_not_decoded_twice() {
        let (url, _) =
            classify_media_url("https://eu-api.asm.skype.com/v1/objects/a?q=1&amp;amp;b=2")
                .expect("allowed");
        let stored = url.to_string();
        let (again, _) = classify_parsed_url(&stored).expect("still allowed");
        assert_eq!(again.as_str(), stored);
    }

    #[test]
    fn uppercase_and_data_src_images_never_reach_the_renderer() {
        let html = r#"<p><IMG SRC="https://tracker.example/p.gif"><img data-src="https://eu-api.asm.skype.com/x" src="https://tracker.example/q.gif"></p>"#;
        assert_eq!(
            display_html(html, &[]),
            "<p><i>[image]</i><i>[image]</i></p>"
        );
        assert!(message_images(html).is_empty());
    }

    #[test]
    fn teams_images_become_attachments_and_others_placeholders() {
        let html = r#"<p>look <img itemtype="http://schema.skype.com/AMSImage" src="https://eu-api.asm.skype.com/v1/objects/0-a/views/imgo" width="250" height="180"><img src="https://tracker.example/p.gif"></p>"#;
        let images = message_images(html);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].width, Some(250.));
        assert_eq!(display_html(html, &[]), "<p>look <i>[image]</i></p>");
    }

    #[test]
    fn runs_of_blank_paragraphs_collapse_to_one() {
        let html = "<p>a</p><p>&nbsp;</p><p></p><p>&nbsp;</p><p>b</p><p>&nbsp;</p>";
        assert_eq!(display_html(html, &[]), "<p>a</p><p>&nbsp;</p><p>b</p>");
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
