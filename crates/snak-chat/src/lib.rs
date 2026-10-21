//! The chat model the UI is written against. Nothing here knows about Teams:
//! a backend translates its service into these types.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Local, Utc};
use tokio::sync::mpsc;

pub type Result<T> = std::result::Result<T, ChatError>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ChatError {
    /// Credentials are gone or rejected; the user has to sign in again.
    #[error("session expired: {0}")]
    SessionExpired(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConversationId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageId(pub String);

/// What channels belong to: a Teams team, a Slack workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Group {
    /// Stable across renames, so saved layouts keep working.
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationKind {
    /// A channel inside a group (a Teams team, a Slack workspace section).
    /// Channel posts carry threads.
    Channel {
        group: Group,
    },
    Direct,
    Group,
}

#[derive(Debug, Clone)]
pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    pub title: String,
    pub unread: bool,
    /// Muted in the service itself (e.g. in Teams).
    pub muted: bool,
    pub last_activity: Option<DateTime<Utc>>,
    /// Latest message as "Author: text".
    pub preview: Option<String>,
}

impl Conversation {
    pub fn is_channel(&self) -> bool {
        matches!(self.kind, ConversationKind::Channel { .. })
    }

    pub fn group(&self) -> Option<&Group> {
        match &self.kind {
            ConversationKind::Channel { group } => Some(group),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// Sanitised HTML the UI can render as-is.
    Html(String),
    Text(String),
    Deleted,
    /// Calls, joins and other system events, shown as one muted line.
    Event(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageRef {
    pub url: String,
    /// Size the sender's client reported, so space can be reserved before loading.
    pub width: Option<f32>,
    pub height: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct Reaction {
    pub emoji: String,
    pub count: usize,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: MessageId,
    pub author: String,
    pub author_id: Option<String>,
    pub sent_at: Option<DateTime<Utc>>,
    pub body: Body,
    pub edited: bool,
    pub reactions: Vec<Reaction>,
    /// Images shown under the text; fetched through [`ChatBackend::image`].
    pub images: Vec<ImageRef>,
    /// Replies, oldest first. Only channel posts have them.
    pub replies: Vec<Message>,
    /// Sent from this client but not yet seen back from the service.
    pub pending: bool,
    /// The message @mentions the signed-in user.
    pub mentions_me: bool,
}

impl Message {
    /// Slack groups a run of messages from one person; the header shows once.
    pub fn continues(&self, previous: &Message) -> bool {
        const GROUP_WINDOW_SECS: i64 = 5 * 60;
        let is_chat = |m: &Message| matches!(m.body, Body::Html(_) | Body::Text(_));
        is_chat(self)
            && is_chat(previous)
            && self.author_id.is_some()
            && self.author_id == previous.author_id
            && match (self.sent_at, previous.sent_at) {
                (Some(a), Some(b)) => (a - b).num_seconds().abs() < GROUP_WINDOW_SECS,
                _ => false,
            }
    }

    pub fn time_label(&self) -> String {
        let Some(sent) = self.sent_at else {
            return String::new();
        };
        let local = sent.with_timezone(&Local);
        if local.date_naive() == Local::now().date_naive() {
            local.format("%H:%M").to_string()
        } else {
            local.format("%a %e %b %H:%M").to_string()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Me {
    pub id: String,
    pub display_name: String,
}

/// One person's own details, for the profile panel.
#[derive(Debug, Clone)]
pub struct Profile {
    pub display_name: String,
    /// Profile photo bytes, when the backend can fetch one.
    pub photo: Option<Vec<u8>>,
    pub mail: Option<String>,
    pub job_title: Option<String>,
    pub phones: Vec<String>,
    /// Their 1:1 chat, when one exists.
    pub dm: Option<ConversationId>,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Something changed in this conversation; refetch its messages.
    MessagesChanged(ConversationId),
    /// Unread state or the conversation list changed.
    ConversationsChanged,
    Connected,
    Reconnecting,
    /// Push stopped because credentials expired; the user has to sign in again.
    SessionExpired,
    /// Someone is typing in this conversation, or stopped. A stop doesn't
    /// always arrive (closed laptop, lost connection), so a started
    /// indicator also expires on its own after a few seconds.
    Typing {
        conversation: ConversationId,
        /// Display name, empty when unknown.
        user: String,
        stopped: bool,
    },
    /// Someone's presence changed; `presence` is the service's own word for
    /// it, e.g. Teams' `Available`, `Away` or `Offline`. `activity` is what
    /// they're doing, and `last_online` when they were last seen, if known.
    PresenceChanged {
        user: String,
        presence: String,
        activity: Option<String>,
        last_online: Option<String>,
    },
}

/// A signed-in chat service.
pub trait ChatBackend: Send + Sync {
    fn me(&self) -> &Me;

    fn conversations(&self) -> BoxFuture<'_, Vec<Conversation>>;

    /// Messages oldest first. Channel posts carry their replies.
    fn messages<'a>(&'a self, id: &'a ConversationId) -> BoxFuture<'a, Vec<Message>>;

    fn send<'a>(
        &'a self,
        id: &'a ConversationId,
        reply_to: Option<&'a MessageId>,
        text: &'a str,
    ) -> BoxFuture<'a, ()>;

    /// Tells the service the user has read everything up to now, so other
    /// clients stop showing it as unread.
    fn mark_read<'a>(&'a self, id: &'a ConversationId) -> BoxFuture<'a, ()>;

    /// Image bytes for an [`ImageRef`]; services keep these behind auth.
    fn image<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Vec<u8>>;

    /// Link that hands a call for this conversation to another app, if any.
    fn call_url(&self, id: &ConversationId) -> Option<String>;

    /// A person's own details. The key is a conversation id, a member id or
    /// whatever the backend's mention links carry; backends that know no
    /// people of their own return `None`.
    fn person(&self, _key: &str) -> BoxFuture<'_, Option<Profile>> {
        Box::pin(async { Ok(None) })
    }

    /// Push updates. Backends without push return a receiver that never fires.
    fn subscribe(self: Arc<Self>) -> mpsc::Receiver<Event>;
}

/// What the user needs to sign in with a device code.
#[derive(Debug, Clone)]
pub struct LoginPrompt {
    pub code: String,
    pub url: String,
    /// Backend-private state for finishing the login.
    pub handle: String,
    pub expires_in_secs: u64,
    pub poll_interval_secs: u64,
}

/// Creates a backend: from saved credentials, or through a sign-in.
pub trait Connector: Send + Sync {
    /// Shown on the sign-in screen, e.g. "Microsoft Teams".
    fn service_name(&self) -> &str;

    /// Signs in with saved credentials; `Ok(None)` if there are none.
    fn restore(&self) -> BoxFuture<'_, Option<Arc<dyn ChatBackend>>>;

    fn start_login(&self) -> BoxFuture<'_, LoginPrompt>;

    /// Waits for the user to finish the browser step.
    fn finish_login<'a>(&'a self, prompt: &'a LoginPrompt) -> BoxFuture<'a, Arc<dyn ChatBackend>>;

    /// Forgets saved credentials. Must return quickly: it runs on the UI
    /// thread, so slow cleanup belongs in the background.
    fn sign_out(&self);

    /// Where saved credentials live; an existing login moves across. On an
    /// error the old place stays in use.
    fn set_credential_storage(&self, storage: CredentialStorage) -> BoxFuture<'_, ()>;
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum CredentialStorage {
    #[default]
    Keychain,
    /// A plain file only the user can open, but any of their processes can read.
    File,
    /// A 1Password item, through the `op` CLI, in this account (sign-in address).
    OnePassword { account: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(author: &str, minute: u32) -> Message {
        Message {
            id: MessageId(format!("{author}{minute}")),
            author: author.into(),
            author_id: Some(author.into()),
            sent_at: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, 15, 10, minute, 0).single(),
            body: Body::Text("x".into()),
            edited: false,
            reactions: Vec::new(),
            images: Vec::new(),
            replies: Vec::new(),
            pending: false,
            mentions_me: false,
        }
    }

    #[test]
    fn same_author_within_five_minutes_continues() {
        assert!(message("a", 4).continues(&message("a", 0)));
        assert!(!message("a", 20).continues(&message("a", 4)));
        assert!(!message("b", 5).continues(&message("a", 4)));
    }

    #[test]
    fn events_break_a_run() {
        let mut call = message("a", 1);
        call.body = Body::Event("Call".into());
        assert!(!call.continues(&message("a", 0)));
    }
}
