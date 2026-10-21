// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Teams as a `snak_chat` backend: maps Teams' shapes onto the neutral model.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use snak_chat::{
    Body, BoxFuture, ChatBackend, ChatError, Connector, Conversation, ConversationId,
    ConversationKind, CredentialStorage, Event, Group, ImageRef, LoginPrompt, Me, Message,
    MessageId, MessagesPage, Reaction,
};
use tokio::sync::mpsc;

use crate::auth::{self, DeviceCode};
use crate::models::{self, Chat, ReplyChain, Team};
use crate::store::StoredLogin;
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
    /// Member MRI by object id, to subscribe presence for DM partners.
    mris: HashMap<String, String>,
    /// MRIs subscribed on the current push session.
    subscribed: std::collections::HashSet<String>,
}

pub struct TeamsBackend {
    client: Arc<TeamsClient>,
    me: Me,
    directory: Mutex<Directory>,
    /// The live push session, so DM partners found after it connected can
    /// still be subscribed to presence.
    push_session: Mutex<Option<crate::trouter::PushSession>>,
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
            push_session: Mutex::new(None),
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

        let names = self.directory().names.clone();
        let list = {
            let mut directory = self.directory();
            let mut list = channel_conversations(&details.teams, &mut directory.addresses);
            list.extend(chat_conversations(
                &details.chats,
                &self.me.id,
                &names,
                &mut directory,
            ));
            list
        };
        self.subscribe_new_presences().await;
        Ok(list)
    }

    /// Subscribes presence for DM partners the live push session doesn't
    /// cover yet. Push often connects before the first conversation load
    /// fills `mris`, and new DMs turn up later, so this runs after both.
    async fn subscribe_new_presences(&self) {
        let session = self
            .push_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(session) = session else {
            return;
        };
        let new: Vec<String> = {
            let directory = self.directory();
            directory
                .mris
                .values()
                .filter(|mri| !directory.subscribed.contains(*mri))
                .cloned()
                .collect()
        };
        if new.is_empty() {
            return;
        }
        match self.client.subscribe_presence(&session, &new).await {
            // A reconnect during the request means it went to the old session.
            Ok(())
                if self
                    .push_session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    == Some(&session) =>
            {
                self.directory().subscribed.extend(new)
            }
            Ok(()) => {}
            Err(e) => tracing::warn!(error = %e, "presence subscription failed"),
        }
    }
}

impl ChatBackend for TeamsBackend {
    fn me(&self) -> &Me {
        &self.me
    }

    fn conversations(&self) -> BoxFuture<'_, Vec<Conversation>> {
        Box::pin(self.load_conversations())
    }

    fn messages<'a>(&'a self, id: &'a ConversationId) -> BoxFuture<'a, MessagesPage> {
        Box::pin(async move {
            Ok(match self.address(id)? {
                Address::Channel {
                    team_id,
                    channel_id,
                } => {
                    let page = self.client.channel_posts(&team_id, &channel_id).await?;
                    MessagesPage {
                        messages: channel_messages(&page.reply_chains, &self.me.id),
                        older: page.continuation_token.filter(|_| page.has_more),
                    }
                }
                Address::Chat { id, .. } => {
                    let page = self.client.chat_messages(&id).await?;
                    MessagesPage {
                        messages: chat_messages(&page.messages, &self.me.id),
                        older: page.metadata.backward_link,
                    }
                }
            })
        })
    }

    fn older_messages<'a>(
        &'a self,
        id: &'a ConversationId,
        older: &'a str,
    ) -> BoxFuture<'a, MessagesPage> {
        Box::pin(async move {
            Ok(match self.address(id)? {
                Address::Channel {
                    team_id,
                    channel_id,
                } => {
                    let page = self
                        .client
                        .channel_posts_before(&team_id, &channel_id, older)
                        .await?;
                    MessagesPage {
                        messages: channel_messages(&page.reply_chains, &self.me.id),
                        older: page.continuation_token.filter(|_| page.has_more),
                    }
                }
                Address::Chat { .. } => {
                    let page = self.client.chat_messages_before(older).await?;
                    MessagesPage {
                        messages: chat_messages(&page.messages, &self.me.id),
                        older: page.metadata.backward_link,
                    }
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
            // Stop as soon as the app drops its receiver (sign-out), rather
            // than at the next event: until then the push session, and its
            // token refreshes, would keep running for a session that's gone.
            loop {
                let event = tokio::select! {
                    event = push.recv() => event,
                    _ = tx.closed() => return,
                };
                let Some(event) = event else {
                    // Push is gone for good; later loads mustn't post to it.
                    *self.push_session.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    return;
                };
                if let PushEvent::Connected(session) = &event {
                    // A reconnect is a new session that knows no subscriptions.
                    *self.push_session.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(session.clone());
                    self.directory().subscribed.clear();
                    self.subscribe_new_presences().await;
                }
                let names = self.directory().names.clone();
                for event in translate(event, &names, &self.me.id) {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
            }
        });
        rx
    }
}

fn translate(event: PushEvent, names: &HashMap<String, String>, me_id: &str) -> Vec<Event> {
    // Channel replies arrive addressed as `<channel>;messageid=<root>`.
    let conversation = |id: &str| ConversationId(id.split(';').next().unwrap_or(id).to_string());
    // MRIs end in the object id: `8:orgid:<object id>`.
    let name = |mri: &str| names.get(mri.rsplit(':').next().unwrap_or(mri));
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
        // Our own typing on another device echoes back; it's not news.
        PushEvent::Typing { user_mri, .. } if user_mri.rsplit(':').next() == Some(me_id) => {
            Vec::new()
        }
        PushEvent::Typing {
            conversation_id,
            user_mri,
            stopped,
        } => vec![Event::Typing {
            conversation: conversation(&conversation_id),
            user: name(&user_mri).cloned().unwrap_or_default(),
            stopped,
        }],
        PushEvent::Presence(presences) => presences
            .iter()
            .map(|p| Event::PresenceChanged {
                user: name(&p.mri).cloned().unwrap_or_default(),
                presence: p.availability.clone().unwrap_or_else(|| "Offline".into()),
            })
            .collect(),
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

    async fn connect(&self, login: StoredLogin) -> Result<Arc<dyn ChatBackend>, ChatError> {
        let client = TeamsClient::connect(self.http.clone(), login)
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
            match crate::store::load().await? {
                Some(login) => self.connect(login).await.map(Some),
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
            let login = auth::complete_device_login(&self.http, &code).await?;
            self.connect(login).await
        })
    }

    fn sign_out(&self) {
        crate::store::sign_out();
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
            crate::store::set_storage(storage).await?;
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
    directory: &mut Directory,
) -> Vec<Conversation> {
    chats
        .iter()
        .filter(|chat| chat.is_conversation_deleted != Some(true))
        // Meeting chats belong with the meeting in Teams; they crowd out real DMs.
        .filter(|chat| chat.chat_type.as_deref() != Some("meeting"))
        // Teams hides chats you closed in its own list, but they still open and
        // receive messages, so they list like any other chat.
        .map(|chat| {
            let others: Vec<String> = chat
                .members
                .iter()
                .filter_map(|m| m.object_id.clone())
                .filter(|id| id != me_id)
                .collect();
            let id = ConversationId(chat.id.clone());
            directory.addresses.insert(
                id.clone(),
                Address::Chat {
                    id: chat.id.clone(),
                    others: others.clone(),
                },
            );
            // Presence only matters for the people you DM directly.
            if chat.is_one_on_one == Some(true) {
                for member in chat.members.iter() {
                    if let (Some(object_id), member_mri) =
                        (member.object_id.as_ref(), member.mri.as_str())
                        && object_id != me_id
                    {
                        directory
                            .mris
                            .insert(object_id.clone(), member_mri.to_string());
                    }
                }
            }
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
    let mentions = message.mentions();
    let my_mentions: Vec<String> = mentions
        .iter()
        .filter(|(_, mri)| {
            !me_id.is_empty()
                && mri
                    .rsplit(':')
                    .next()
                    .is_some_and(|id| id.eq_ignore_ascii_case(me_id))
        })
        .map(|(item, _)| item.clone())
        .collect();
    let message_type = message.message_type.as_deref().unwrap_or("");
    if message_type.starts_with("Control/") || message_type.starts_with("ThreadActivity/") {
        return None;
    }
    let body = if message.is_deleted() {
        Body::Deleted
    } else if message_type.starts_with("Event/Call") {
        Body::Event("Call".into())
    } else {
        let content = message.content.as_deref().unwrap_or("");
        let subject = message.subject();
        if message_type.starts_with("RichText/Html") {
            let content = display_html(content, &mentions, &my_mentions);
            Body::Html(match subject {
                // In its own block: GPUI Kit glues bare text that follows a
                // block onto that block's last line.
                Some(s) => format!(
                    "<p><b>{}</b></p><div>{content}</div>",
                    html_escape::encode_text(&s)
                ),
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

    // Tint only real text that mentions you, not deleted or plain messages
    // whose properties still list the mention.
    let mentions_me = !my_mentions.is_empty() && matches!(body, Body::Html(_));
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
                emoji: reaction_glyph(&r.key),
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
        mentions_me,
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
fn display_html(html: &str, mentions: &[(String, String)], my_mentions: &[String]) -> String {
    let html = &render_mentions(html, mentions, my_mentions);
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

/// Teams writes a blank line as an empty `<p>` or `<div>`, in several shapes
/// (`&nbsp;`, `<br>`, empty spans, attributes). Runs collapse to one blank line,
/// blank lines at either end go, and spaces before a line end are trimmed.
fn collapse_blank_paragraphs(html: &str) -> String {
    const BLANK: &str = "<p>&nbsp;</p>";
    let mut blocks: Vec<String> = Vec::new();
    let mut rest = html;
    // A search for a close tag runs to the end of the message, so once one
    // finds nothing, no later one will: remember that per tag, or a message
    // full of unclosed tags would rescan itself at every one.
    let mut unclosed = [false; 2];
    while !rest.is_empty() {
        // Code keeps its whitespace: a diff's context line is a single space.
        if let Some(tag) = code_tag(rest).filter(|&t| !unclosed[t]) {
            match code_span(rest, CODE_TAGS[tag]) {
                Some(len) => {
                    blocks.push(rest[..len].to_string());
                    rest = &rest[len..];
                    continue;
                }
                None => unclosed[tag] = true,
            }
        }
        match blank_block(rest) {
            Some(len) => {
                if blocks.last().is_none_or(|b| b != BLANK) {
                    blocks.push(BLANK.to_string());
                }
                rest = rest[len..].trim_start();
            }
            None => {
                // Copy up to the next block opening, so text between blocks stays.
                let next = rest
                    .char_indices()
                    .skip(1)
                    .find(|&(i, c)| {
                        c == '<' && {
                            let head = rest.as_bytes().get(i + 1..(i + 4).min(rest.len()));
                            head.is_some_and(|h| {
                                h.first().is_some_and(|b| b.eq_ignore_ascii_case(&b'p'))
                                    || h.eq_ignore_ascii_case(b"div")
                            }) || starts_with_ci(&rest[i + 1..], "codeblock")
                        }
                    })
                    .map_or(rest.len(), |(i, _)| i);
                blocks.push(trim_line_ends(&rest[..next]));
                rest = &rest[next..];
            }
        }
    }
    while blocks
        .first()
        .is_some_and(|b| b == BLANK || b.trim().is_empty())
    {
        blocks.remove(0);
    }
    while blocks
        .last()
        .is_some_and(|b| b == BLANK || b.trim().is_empty())
    {
        blocks.pop();
    }
    blocks.concat()
}

const CODE_TAGS: [&str; 2] = ["pre", "codeblock"];

/// Which of [`CODE_TAGS`] opens at the start of `html`, if any.
fn code_tag(html: &str) -> Option<usize> {
    let after = html.strip_prefix('<')?;
    CODE_TAGS
        .iter()
        .position(|tag| starts_with_ci(after, tag) && ends_tag_name(&after[tag.len()..]))
}

/// Length of the `tag` element opening at the start of `html`, up to and
/// including its closing tag.
fn code_span(html: &str, tag: &str) -> Option<usize> {
    let mut at = 1;
    loop {
        let close = at + html[at..].find("</")?;
        let after = &html[close + 2..];
        if starts_with_ci(after, tag) && ends_tag_name(&after[tag.len()..]) {
            return Some(close + html[close..].find('>')? + 1);
        }
        at = close + 2;
    }
}

/// The tag name is complete: `<pre` must not match `<prefix>`.
fn ends_tag_name(rest: &str) -> bool {
    rest.chars()
        .next()
        .is_some_and(|c| c == '>' || c == '/' || c.is_whitespace())
}

fn starts_with_ci(text: &str, prefix: &str) -> bool {
    text.as_bytes()
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Length of a blank `<p>`/`<div>` at the start of `html`, if there is one.
fn blank_block(html: &str) -> Option<usize> {
    // Blank lines are short; a bounded prefix keeps this linear overall.
    let cut = html.char_indices().nth(512).map_or(html.len(), |(i, _)| i);
    let lower = html[..cut].to_ascii_lowercase();
    let name = ["p", "div"].into_iter().find(|name| {
        lower
            .strip_prefix(&format!("<{name}"))
            .is_some_and(|after| after.starts_with('>') || after.starts_with(' '))
    })?;
    let open_end = lower.find('>')? + 1;
    let close = format!("</{name}>");
    let close_at = lower[open_end..].find(&close)? + open_end;
    let inner = &lower[open_end..close_at];
    // A nested block isn't blank in this simple sense; leave it alone.
    if inner.contains("<p") || inner.contains("<div") {
        return None;
    }
    let mut text = String::new();
    let mut in_tag = false;
    for c in inner.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    let empty = text
        .replace("&nbsp;", " ")
        .replace('\u{a0}', " ")
        .trim()
        .is_empty();
    // Only blank if the tags were line breaks or empty spans, not an image.
    let only_breaks = !inner.contains("<img");
    (empty && only_breaks).then_some(close_at + close.len())
}

/// Drops spaces and `&nbsp;` right before `</p>`, `</div>` and `<br>`.
fn trim_line_ends(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find('<') {
        let ends_line = ["</p>", "</div>", "<br>", "<br/>", "<br />"]
            .iter()
            .any(|t| {
                rest.as_bytes()
                    .get(at..at + t.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(t.as_bytes()))
            });
        let mut text = &rest[..at];
        if ends_line {
            loop {
                let trimmed = text.trim_end_matches([' ', '\u{a0}']);
                let trimmed = trimmed.strip_suffix("&nbsp;").unwrap_or(trimmed);
                if trimmed.len() == text.len() {
                    break;
                }
                text = trimmed;
            }
        }
        out.push_str(text);
        let tag_end = rest[at..].find('>').map_or(rest.len(), |i| at + i + 1);
        out.push_str(&rest[at..tag_end]);
        rest = &rest[tag_end..];
    }
    out.push_str(rest);
    out
}

/// Teams marks a mention as `<span itemtype="http://schema.skype.com/Mention"
/// itemid="N">Name</span>`, and splits a full name into one span per word.
/// The words may share an itemid or get one each; `mentions` (itemid, mri)
/// ties them to one person either way. Each run becomes one `@Full Name` link
/// to `snak-mention:N`, bold when it's the signed-in user. The link scheme is
/// never opened.
fn render_mentions(html: &str, mentions: &[(String, String)], my_mentions: &[String]) -> String {
    // The person behind an itemid: their MRI when listed, else the itemid.
    let person = |item: &str| -> String {
        mentions.iter().find(|(id, _)| id == item).map_or_else(
            || format!("item:{item}"),
            |(_, mri)| mri.to_ascii_lowercase(),
        )
    };
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
        let mut exhausted = false;
        let mut words = Vec::new();
        let mut end = start;
        let mut cursor = start;
        while let Some(open_len) = lower[cursor..].find('>') {
            let inner_start = cursor + open_len + 1;
            let Some(close) = lower[inner_start..].find("</span>") else {
                // No closing tag anywhere after here, so none for any later
                // span either; stop instead of rescanning to the end each time.
                exhausted = true;
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
                        && attribute(next_tag, "itemid")
                            .is_some_and(|next_item| person(&next_item) == person(&item))
                });
            if !same_person {
                break;
            }
            cursor = next;
        }
        if words.is_empty() {
            if exhausted {
                break;
            }
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
        // Compared raw; encoding is only for the attribute it goes into.
        let mine = my_mentions.contains(&item);
        let item = html_escape::encode_double_quoted_attribute(&item);
        if mine {
            out.push_str(&format!(
                "<a href=\"snak-mention:{item}\"><b>@{label}</b></a>"
            ));
        } else {
            out.push_str(&format!("<a href=\"snak-mention:{item}\">@{label}</a>"));
        }
        copied = end;
        at = end;
        if exhausted {
            break;
        }
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

/// Teams sends reaction ids, not emoji, in three shapes: code points before an
/// underscore (`1f389_partypopper`), a name (`like`, `clappinghands`), and
/// either one with a skin tone (`yes-tone1`). An unknown name shows as
/// `:name:` so it still reads as a reaction.
fn reaction_glyph(key: &str) -> String {
    let (base, tone) = match key.rsplit_once("-tone") {
        Some((base, n)) => (base, n.parse::<u32>().ok().filter(|n| (1..=5).contains(n))),
        None => (key, None),
    };
    let Some(mut glyph) = code_point_glyph(base).or_else(|| named_glyph(base).map(String::from))
    else {
        // Other people pick the key, so only plain characters, and not many.
        let name: String = key.chars().filter(|&c| !invisible(c)).take(32).collect();
        return format!(":{name}:");
    };
    if let Some(tone) = tone {
        // Fitzpatrick modifiers U+1F3FB..U+1F3FF; the variation selector some
        // bases carry has to go first or the modifier shows as a swatch.
        glyph.retain(|c| c != '\u{fe0f}');
        glyph.extend(char::from_u32(0x1F3FA + tone));
    }
    glyph
}

/// `1f389_partypopper` → 🎉; multi-code-point ids join with `-`. The key is
/// someone else's input, so it must be hex digits that make an emoji-sized
/// run of visible characters, not controls or a right-to-left override.
fn code_point_glyph(base: &str) -> Option<String> {
    const MAX_CODE_POINTS: usize = 10;
    let (points, _) = base.split_once('_')?;
    let parts: Vec<&str> = points.split('-').take(MAX_CODE_POINTS + 1).collect();
    if parts.len() > MAX_CODE_POINTS {
        return None;
    }
    let glyph: String = parts
        .into_iter()
        .map(|p| {
            // from_str_radix would also take a leading `+`.
            if p.is_empty() || p.len() > 6 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let c = char::from_u32(u32::from_str_radix(p, 16).ok()?)?;
            // Joiners and variation selectors are part of real emoji.
            let joins = matches!(c, '\u{200d}' | '\u{fe0f}' | '\u{fe0e}' | '\u{20e3}')
                || ('\u{e0020}'..='\u{e007f}').contains(&c);
            (joins || !invisible(c)).then_some(c)
        })
        .collect::<Option<_>>()?;
    // Joiners, marks or blanks alone would make a reaction that draws nothing.
    glyph.chars().any(draws_on_its_own).then_some(glyph)
}

/// A character that shows something by itself, not only by joining or
/// marking another: needed at least once in a reaction.
fn draws_on_its_own(c: char) -> bool {
    !invisible(c)
        && !matches!(c,
            '\u{200d}'
            | '\u{20e3}'
            | '\u{300}'..='\u{36f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe20}'..='\u{fe2f}'
            | '\u{1f3fb}'..='\u{1f3ff}')
}

/// Characters that draw nothing or reorder text: controls, whitespace and
/// blank fillers, bidi overrides and isolates, zero-width marks and
/// noncharacters.
fn invisible(c: char) -> bool {
    c.is_control()
        || c.is_whitespace()
        || matches!(c,
            '\u{ad}'
            | '\u{34f}'
            | '\u{61c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{180b}'..='\u{180f}'
            | '\u{2800}'
            | '\u{3164}'
            | '\u{ffa0}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{fdd0}'..='\u{fdef}'
            | '\u{e0000}'..='\u{e007f}')
        || (c as u32 & 0xfffe) == 0xfffe
}

fn named_glyph(name: &str) -> Option<&'static str> {
    Some(match name {
        "like" | "yes" | "thumbsup" => "👍",
        "no" | "thumbsdown" => "👎",
        "heart" => "❤️",
        "laugh" => "😆",
        "surprised" => "😮",
        "sad" | "cry" => "😢",
        "angry" => "😠",
        "clappinghands" | "clap" => "👏",
        "handsinair" => "🙌",
        "fire" => "🔥",
        "muscle" => "💪",
        "fistbump" => "👊",
        "cool" => "😎",
        "launch" | "rocket" => "🚀",
        "salute" => "🫡",
        "penguin" => "🐧",
        "eyes" => "👀",
        "party" | "partypopper" => "🎉",
        "pray" | "prayinghands" => "🙏",
        "ok" | "okhand" => "👌",
        "victory" => "✌️",
        "wave" | "hi" => "👋",
        "smile" => "🙂",
        "wink" => "😉",
        "hug" => "🤗",
        "rofl" => "🤣",
        "thinking" => "🤔",
        "facepalm" => "🤦",
        "shrug" => "🤷",
        "100" => "💯",
        "star" => "⭐",
        "coffee" => "☕",
        "cake" => "🍰",
        "check" | "checkmark" => "✅",
        _ => return None,
    })
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
        assert_eq!(display_html(html, &[], &[]), "<p>hi 😀 <i>[image]</i></p>");
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
            display_html(html, &[], &["0".to_string()]),
            r#"<p>hi <a href="snak-mention:0"><b>@Allan Kimmer Jensen</b></a> and <a href="snak-mention:1">@Maja</a></p>"#
        );
    }

    #[test]
    fn reaction_ids_become_emoji() {
        assert_eq!(reaction_glyph("like"), "👍");
        assert_eq!(reaction_glyph("1f389_partypopper"), "🎉");
        assert_eq!(reaction_glyph("2705_whiteheavycheckmark"), "✅");
        assert_eq!(reaction_glyph("yes-tone1"), "👍🏻");
        assert_eq!(reaction_glyph("muscle-tone5"), "💪🏿");
        assert_eq!(reaction_glyph("somethingnew"), ":somethingnew:");
        assert_eq!(reaction_glyph("zz_bad"), ":zz_bad:");
        // Keys are other people's input.
        assert_eq!(reaction_glyph("0_x"), ":0_x:");
        assert_eq!(reaction_glyph("202e_x"), ":202e_x:");
        assert_eq!(reaction_glyph("fffe_x-tone1"), ":fffe_x-tone1:");
        assert_eq!(reaction_glyph("+1f44d_x"), ":+1f44d_x:");
        assert_eq!(
            reaction_glyph(&format!("{}_x", "1f44d-".repeat(11)))
                .chars()
                .count(),
            34
        );
        assert_eq!(reaction_glyph("a\u{202e}b"), ":ab:");
        assert_eq!(reaction_glyph("200d_x"), ":200d_x:");
        assert_eq!(reaction_glyph("2028_x"), ":2028_x:");
        for blank in [
            "0020", "3000", "2800", "3164", "115f", "00a0", "2003", "0301", "1f3fb",
        ] {
            assert_eq!(
                reaction_glyph(&format!("{blank}_x")),
                format!(":{blank}_x:")
            );
        }
        assert_eq!(reaction_glyph("a\u{61c}\u{2028}b"), ":ab:");
        // Keycaps and subdivision flags keep their combining parts.
        assert_eq!(reaction_glyph("0023-fe0f-20e3_keycap"), "#\u{fe0f}\u{20e3}");
        assert_eq!(
            reaction_glyph("1f3f4-e0067-e0062-e0065-e006e-e0067-e007f_england")
                .chars()
                .count(),
            7
        );
        // Real multi-part emoji keep their joiners: 👩‍💻.
        assert_eq!(
            reaction_glyph("1f469-200d-1f4bb_womantechnologist"),
            "👩\u{200d}💻"
        );
    }

    #[test]
    fn mention_words_with_their_own_itemids_become_one_mention() {
        let span = |id: &str, word: &str| {
            format!(
                r#"<span itemtype="http://schema.skype.com/Mention" itemid="{id}">{word}</span>"#
            )
        };
        let html = format!(
            "<p>{} {} {} and {}</p>",
            span("0", "Allan"),
            span("1", "Kimmer"),
            span("2", "Jensen"),
            span("3", "Maja"),
        );
        let mri = |id: &str, who: &str| (id.to_string(), format!("8:orgid:{who}"));
        let mentions = [
            mri("0", "me"),
            mri("1", "me"),
            mri("2", "me"),
            mri("3", "maja"),
        ];
        assert_eq!(
            display_html(
                &html,
                &mentions,
                &["0".to_string(), "1".to_string(), "2".to_string()]
            ),
            r#"<p><a href="snak-mention:0"><b>@Allan Kimmer Jensen</b></a> and <a href="snak-mention:3">@Maja</a></p>"#
        );
    }

    #[test]
    fn unclosed_mention_spans_stay_linear() {
        let span = r#"<span itemtype="http://schema.skype.com/Mention" itemid="0">x"#;
        let html = span.repeat(20_000);
        let started = std::time::Instant::now();
        let _ = display_html(&html, &[], &[]);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn mention_names_are_escaped() {
        let html = r#"<span itemtype="http://schema.skype.com/Mention" itemid="0">&lt;img src=x&gt;</span>"#;
        assert!(!display_html(html, &[], &[]).contains("<img"));
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
        let out = display_html(html, &[], &[]);
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
            display_html(html, &[], &[]),
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
        assert_eq!(display_html(html, &[], &[]), "<p>look <i>[image]</i></p>");
    }

    #[test]
    fn blank_lines_in_any_shape_are_trimmed_and_collapsed() {
        let html = concat!(
            "<p>&nbsp;</p>",
            "<p>Efterårsferie:&nbsp; </p>",
            r#"<p style="margin:0"><br></p> <div><span></span>&nbsp;</div>"#,
            "<p>Sms - Henrik</p>",
            "<p><br></p>",
            r#"<div class="x"> </div>"#,
        );
        assert_eq!(
            display_html(html, &[], &[]),
            "<p>Efterårsferie:</p><p>&nbsp;</p><p>Sms - Henrik</p>"
        );
    }

    #[test]
    fn code_keeps_its_whitespace() {
        let html = "<p>diff:&nbsp; </p><pre><code>a  <br>&nbsp;<br>c</code></pre><p>&nbsp;</p>";
        assert_eq!(
            display_html(html, &[], &[]),
            "<p>diff:</p><pre><code>a  <br>&nbsp;<br>c</code></pre>"
        );
    }

    #[test]
    fn unclosed_code_tags_stay_linear() {
        for html in [
            "<pre>x".repeat(10_000) + &"<pre></prex>".repeat(10_000),
            "<codeblock>x".repeat(10_000) + "</pre>",
            "<pre>x".repeat(10_000) + "</codeblock>",
            "<pre>x".repeat(10_000) + "</pre ",
        ] {
            let started = std::time::Instant::now();
            let _ = display_html(&html, &[], &[]);
            assert!(started.elapsed() < std::time::Duration::from_secs(2));
        }
    }

    #[test]
    fn blocks_with_images_are_not_blank() {
        let html = r#"<p>a</p><p><img itemtype="http://schema.skype.com/Emoji" alt="🙂"></p>"#;
        assert_eq!(display_html(html, &[], &[]), "<p>a</p><p>🙂</p>");
    }

    #[test]
    fn runs_of_blank_paragraphs_collapse_to_one() {
        let html = "<p>a</p><p>&nbsp;</p><p></p><p>&nbsp;</p><p>b</p><p>&nbsp;</p>";
        assert_eq!(
            display_html(html, &[], &[]),
            "<p>a</p><p>&nbsp;</p><p>b</p>"
        );
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
    fn closed_and_hidden_chats_stay_listed() {
        let chats: Vec<Chat> = serde_json::from_value(serde_json::json!([
            { "id": "19:a@thread.tacv2", "members": [], "isOneOnOne": true,
              "lastMessage": { "from": "x", "originalArrivalTime": "2026-10-15T10:00:00Z" } },
            { "id": "19:b@thread.tacv2", "members": [], "hidden": true, "isOneOnOne": true,
              "lastMessage": { "from": "x", "originalArrivalTime": "2026-10-15T10:00:00Z" } },
            { "id": "19:c@thread.tacv2", "members": [], "hidden": true,
              "isConversationDeleted": true },
            { "id": "19:d@thread.tacv2", "members": [], "chatType": "meeting" }
        ]))
        .expect("valid chats");
        let list = chat_conversations(&chats, "", &HashMap::new(), &mut Directory::default());
        let ids: Vec<&str> = list.iter().map(|c| c.id.0.as_str()).collect();
        assert_eq!(ids, ["19:a@thread.tacv2", "19:b@thread.tacv2"]);
    }

    #[test]
    fn typing_and_presence_translate_with_names() {
        let mut names = HashMap::new();
        names.insert("a".to_string(), "Maja".to_string());
        let events = translate(
            PushEvent::Typing {
                conversation_id: "19:a@thread.tacv2".into(),
                user_mri: "8:orgid:a".into(),
                stopped: false,
            },
            &names,
            "me",
        );
        assert!(matches!(
            events.as_slice(),
            [Event::Typing { conversation, user, stopped: false }]
                if conversation.0 == "19:a@thread.tacv2" && user == "Maja"
        ));
        let mine = translate(
            PushEvent::Typing {
                conversation_id: "19:a@thread.tacv2".into(),
                user_mri: "8:orgid:me".into(),
                stopped: false,
            },
            &names,
            "me",
        );
        assert!(mine.is_empty());
        let events = translate(
            PushEvent::Presence(vec![crate::trouter::Presence {
                mri: "8:orgid:a".into(),
                availability: Some("Available".into()),
                activity: None,
                last_active_time: None,
                out_of_office: false,
            }]),
            &names,
            "me",
        );
        assert!(matches!(
            events.as_slice(),
            [Event::PresenceChanged { user, presence }]
                if user == "Maja" && presence == "Available"
        ));
    }

    #[test]
    fn dm_partners_subscribe_presence_by_mri() {
        let chats: Vec<Chat> = serde_json::from_value(serde_json::json!([
            { "id": "19:a@thread.tacv2", "isOneOnOne": true,
              "members": [ { "mri": "8:orgid:x", "objectId": "x" },
                            { "mri": "8:orgid:y", "objectId": "y" } ] },
            { "id": "19:b@thread.tacv2", "isOneOnOne": false,
              "members": [ { "mri": "8:orgid:z", "objectId": "z" } ] }
        ]))
        .expect("valid chats");
        let mut directory = Directory::default();
        chat_conversations(&chats, "x", &HashMap::new(), &mut directory);
        let mut mris: Vec<&str> = directory.mris.values().map(String::as_str).collect();
        mris.sort();
        assert_eq!(mris, ["8:orgid:y"]);
    }

    #[test]
    fn channel_reply_pushes_map_to_the_channel() {
        let events = translate(
            PushEvent::ConversationUpdated {
                conversation_id: "19:a@thread.tacv2".into(),
            },
            &HashMap::new(),
            "me",
        );
        assert!(matches!(events.as_slice(), [Event::ConversationsChanged]));
        let message = message("9", "a", "2026-10-15T10:00:00Z", "Text", "x");
        let events = translate(
            PushEvent::NewMessage {
                conversation_id: "19:a@thread.tacv2;messageid=123".into(),
                message,
            },
            &HashMap::new(),
            "me",
        );
        assert!(matches!(&events[0], Event::MessagesChanged(id) if id.0 == "19:a@thread.tacv2"));
    }
}
