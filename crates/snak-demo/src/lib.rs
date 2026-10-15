//! A fake workspace in memory. It lets the UI run with no account, and it
//! posts a message now and then so unread state and push updates get exercised.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use snak_chat::{
    Body, BoxFuture, ChatBackend, ChatError, Connector, Conversation, ConversationId,
    ConversationKind, Event, LoginPrompt, Me, Message, MessageId, Reaction,
};
use tokio::sync::mpsc;

const ME_ID: &str = "me";
const ACTIVITY_EVERY: Duration = Duration::from_secs(40);

struct Person {
    id: &'static str,
    name: &'static str,
}

const PEOPLE: [Person; 5] = [
    Person {
        id: "maja",
        name: "Maja Holm",
    },
    Person {
        id: "jonas",
        name: "Jonas Berg",
    },
    Person {
        id: "sara",
        name: "Sara Lind",
    },
    Person {
        id: "omar",
        name: "Omar Haddad",
    },
    Person {
        id: "ida",
        name: "Ida Kjær",
    },
];

fn person(id: &str) -> &'static str {
    PEOPLE
        .iter()
        .find(|p| p.id == id)
        .map_or("Someone", |p| p.name)
}

struct Thread {
    conversation: Conversation,
    messages: Vec<Message>,
}

struct State {
    threads: Vec<Thread>,
    next_id: u64,
    script_step: usize,
}

impl State {
    fn thread(&mut self, id: &ConversationId) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|t| &t.conversation.id == id)
    }

    fn new_id(&mut self) -> MessageId {
        self.next_id += 1;
        MessageId(format!("m{}", self.next_id))
    }
}

pub struct DemoBackend {
    me: Me,
    state: Mutex<State>,
}

impl DemoBackend {
    pub fn new() -> Self {
        Self {
            me: Me {
                id: ME_ID.into(),
                display_name: "Demo User".into(),
            },
            state: Mutex::new(seed(Utc::now())),
        }
    }

    // A poisoned lock only means a panic elsewhere; the data is still usable.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Posts the next scripted message; returns where it landed.
    fn simulate_activity(&self) -> Option<ConversationId> {
        const SCRIPT: [(&str, &str, &str); 4] = [
            ("dm-maja", "maja", "<p>Got a minute after lunch?</p>"),
            (
                "eng-backend",
                "omar",
                "<p>Deploy to staging is green 🚀</p>",
            ),
            (
                "cx-alpha",
                "ida",
                "<p>Customer signed off on <b>phase 2</b>.</p>",
            ),
            (
                "grp-launch",
                "jonas",
                "<p>Moved the launch sync to 14:00.</p>",
            ),
        ];
        let mut state = self.state();
        let (conversation, author, html) = SCRIPT[state.script_step % SCRIPT.len()];
        state.script_step += 1;
        let id = state.new_id();
        let id_conv = ConversationId(conversation.into());
        let thread = state.thread(&id_conv)?;
        thread.messages.push(message(id, author, Utc::now(), html));
        thread.conversation.unread = true;
        thread.conversation.last_activity = Some(Utc::now());
        thread.conversation.preview = Some(preview(author, html));
        Some(id_conv)
    }
}

impl Default for DemoBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatBackend for DemoBackend {
    fn me(&self) -> &Me {
        &self.me
    }

    fn conversations(&self) -> BoxFuture<'_, Vec<Conversation>> {
        let list = self
            .state()
            .threads
            .iter()
            .map(|t| t.conversation.clone())
            .collect();
        Box::pin(async move { Ok(list) })
    }

    fn messages<'a>(&'a self, id: &'a ConversationId) -> BoxFuture<'a, Vec<Message>> {
        let result = match self.state().thread(id) {
            Some(thread) => {
                // Opening a conversation reads it, as it would in a real client.
                thread.conversation.unread = false;
                Ok(thread.messages.clone())
            }
            None => Err(ChatError::Other(format!("no conversation {}", id.0))),
        };
        Box::pin(async move { result })
    }

    fn send<'a>(
        &'a self,
        id: &'a ConversationId,
        reply_to: Option<&'a MessageId>,
        text: &'a str,
    ) -> BoxFuture<'a, ()> {
        let mut state = self.state();
        let message_id = state.new_id();
        let html = format!("<p>{}</p>", escape(text).replace('\n', "<br>"));
        let result = match state.thread(id) {
            None => Err(ChatError::Other(format!("no conversation {}", id.0))),
            Some(thread) => {
                let new = message(message_id, ME_ID, Utc::now(), &html);
                match reply_to {
                    Some(root) => match thread.messages.iter_mut().find(|m| &m.id == root) {
                        Some(post) => post.replies.push(new),
                        None => {
                            return Box::pin(async {
                                Err(ChatError::Other("no such post".into()))
                            });
                        }
                    },
                    None => thread.messages.push(new),
                }
                thread.conversation.last_activity = Some(Utc::now());
                thread.conversation.preview = Some(preview(ME_ID, &html));
                Ok(())
            }
        };
        Box::pin(async move { result })
    }

    fn call_url(&self, id: &ConversationId) -> Option<String> {
        let state = self.state();
        let thread = state.threads.iter().find(|t| &t.conversation.id == id)?;
        (!thread.conversation.is_channel()).then(|| "https://example.com/demo-call".to_string())
    }

    fn subscribe(self: Arc<Self>) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(16);
        let weak = Arc::downgrade(&self);
        tokio::spawn(async move {
            if tx.send(Event::Connected).await.is_err() {
                return;
            }
            loop {
                tokio::time::sleep(ACTIVITY_EVERY).await;
                let Some(backend) = weak.upgrade() else {
                    return;
                };
                let Some(changed) = backend.simulate_activity() else {
                    continue;
                };
                let delivered = tx.send(Event::MessagesChanged(changed)).await.is_ok()
                    && tx.send(Event::ConversationsChanged).await.is_ok();
                if !delivered {
                    return;
                }
            }
        });
        rx
    }
}

/// Signs straight in; there is nothing to authenticate.
pub struct DemoConnector;

impl Connector for DemoConnector {
    fn service_name(&self) -> &str {
        "Demo workspace"
    }

    fn restore(&self) -> BoxFuture<'_, Option<Arc<dyn ChatBackend>>> {
        Box::pin(async {
            let backend: Arc<dyn ChatBackend> = Arc::new(DemoBackend::new());
            Ok(Some(backend))
        })
    }

    fn start_login(&self) -> BoxFuture<'_, LoginPrompt> {
        Box::pin(async {
            Ok(LoginPrompt {
                code: "DEMO".into(),
                url: "https://example.com".into(),
                handle: String::new(),
                expires_in_secs: 600,
                poll_interval_secs: 1,
            })
        })
    }

    fn finish_login<'a>(&'a self, _: &'a LoginPrompt) -> BoxFuture<'a, Arc<dyn ChatBackend>> {
        Box::pin(async {
            let backend: Arc<dyn ChatBackend> = Arc::new(DemoBackend::new());
            Ok(backend)
        })
    }

    fn sign_out(&self) {}
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn preview(author: &str, html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    let name = if author == ME_ID {
        "You"
    } else {
        person(author)
    };
    format!("{name}: {}", text.trim())
}

fn message(id: MessageId, author: &str, at: DateTime<Utc>, html: &str) -> Message {
    Message {
        id,
        author: if author == ME_ID {
            "Demo User".into()
        } else {
            person(author).into()
        },
        author_id: Some(author.into()),
        sent_at: Some(at),
        body: Body::Html(html.into()),
        edited: false,
        reactions: Vec::new(),
        replies: Vec::new(),
    }
}

fn seed(now: DateTime<Utc>) -> State {
    let mut next_id = 0;
    let mut id = || {
        next_id += 1;
        MessageId(format!("m{next_id}"))
    };
    let ago = |minutes: i64| now - chrono::Duration::minutes(minutes);

    let channel = |id: &str, group: &str, title: &str| Conversation {
        id: ConversationId(id.into()),
        kind: ConversationKind::Channel {
            group: group.into(),
        },
        title: title.into(),
        unread: false,
        last_activity: None,
        preview: None,
    };
    let chat = |id: &str, kind: ConversationKind, title: &str| Conversation {
        id: ConversationId(id.into()),
        kind,
        title: title.into(),
        unread: false,
        last_activity: None,
        preview: None,
    };

    let mut releases_post = message(
        id(),
        "omar",
        ago(300),
        "<p><b>Release 4.2</b> is cut. Notes: <a href=\"https://example.com/notes\">release notes</a></p>",
    );
    releases_post.reactions = vec![
        Reaction {
            emoji: "🎉".into(),
            count: 4,
        },
        Reaction {
            emoji: "👍".into(),
            count: 2,
        },
    ];
    releases_post.replies = vec![
        message(
            id(),
            "maja",
            ago(290),
            "<p>Nice. Are the migration steps in there?</p>",
        ),
        message(id(), "omar", ago(285), "<p>Yes, section 3.</p>"),
    ];

    let mut backend_post = message(
        id(),
        "jonas",
        ago(95),
        "<p>Seeing timeouts on <code>/orders</code> since this morning. Anyone else?</p>",
    );
    backend_post.replies = vec![
        message(
            id(),
            "omar",
            ago(90),
            "<p>The pool size dropped to 10 in the last config change.</p>",
        ),
        message(
            id(),
            "jonas",
            ago(88),
            "<p>That's it, thanks. Reverting.</p>",
        ),
        message(
            id(),
            "sara",
            ago(60),
            "<p>Reverted and verified on staging.</p>",
        ),
    ];

    let mut threads = vec![
        Thread {
            conversation: channel("eng-general", "Engineering", "general"),
            messages: vec![message(
                id(),
                "sara",
                ago(1440),
                "<p>Welcome Ida, who joins the platform team today 👋</p>",
            )],
        },
        Thread {
            conversation: channel("eng-backend", "Engineering", "backend"),
            messages: vec![backend_post],
        },
        Thread {
            conversation: channel("eng-releases", "Engineering", "releases"),
            messages: vec![releases_post],
        },
        Thread {
            conversation: channel("cx-general", "Customer X", "general"),
            messages: vec![message(
                id(),
                "ida",
                ago(2000),
                "<p>Kickoff deck is in the files tab.</p>",
            )],
        },
        Thread {
            conversation: channel("cx-alpha", "Customer X", "project-alpha"),
            messages: vec![message(
                id(),
                "maja",
                ago(45),
                "<ul><li>Scope agreed</li><li>Estimate due Friday</li></ul>",
            )],
        },
        Thread {
            conversation: chat("dm-maja", ConversationKind::Direct, "Maja Holm"),
            messages: vec![
                message(
                    id(),
                    "maja",
                    ago(30),
                    "<p>Did you see the estimate for alpha?</p>",
                ),
                message(
                    id(),
                    "maja",
                    ago(29),
                    "<p>I think we're low on testing hours.</p>",
                ),
                message(id(), ME_ID, ago(20), "<p>Agreed, let's add a week.</p>"),
            ],
        },
        Thread {
            conversation: chat("dm-jonas", ConversationKind::Direct, "Jonas Berg"),
            messages: vec![message(id(), "jonas", ago(400), "<p>Lunch tomorrow?</p>")],
        },
        Thread {
            conversation: chat("grp-launch", ConversationKind::Group, "Jonas, Sara, Omar"),
            messages: vec![message(
                id(),
                "sara",
                ago(120),
                "<p>Launch checklist updated.</p>",
            )],
        },
    ];

    let unread: HashMap<&str, bool> = [
        ("eng-backend", true),
        ("cx-alpha", true),
        ("dm-maja", false),
        ("grp-launch", true),
    ]
    .into();
    for thread in &mut threads {
        let latest = thread
            .messages
            .iter()
            .flat_map(|m| std::iter::once(m).chain(m.replies.iter()))
            .max_by_key(|m| m.sent_at);
        if let Some(latest) = latest {
            thread.conversation.last_activity = latest.sent_at;
            if let Body::Html(html) = &latest.body {
                thread.conversation.preview =
                    Some(preview(latest.author_id.as_deref().unwrap_or(""), html));
            }
        }
        thread.conversation.unread = unread
            .get(thread.conversation.id.0.as_str())
            .copied()
            .unwrap_or(false);
    }

    State {
        threads,
        next_id: 1000,
        script_step: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<T>(future: BoxFuture<'_, T>) -> snak_chat::Result<T> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(|e| ChatError::Other(e.to_string()))?
            .block_on(future)
    }

    #[test]
    fn opening_a_conversation_marks_it_read() {
        let backend = DemoBackend::new();
        let id = ConversationId("eng-backend".into());
        let before = block_on(backend.conversations()).expect("list");
        assert!(before.iter().any(|c| c.id == id && c.unread));
        block_on(backend.messages(&id)).expect("messages");
        let after = block_on(backend.conversations()).expect("list");
        assert!(after.iter().any(|c| c.id == id && !c.unread));
    }

    #[test]
    fn replies_land_under_their_post() {
        let backend = DemoBackend::new();
        let id = ConversationId("eng-backend".into());
        let posts = block_on(backend.messages(&id)).expect("messages");
        let root = posts[0].id.clone();
        block_on(backend.send(&id, Some(&root), "on it")).expect("send");
        let posts = block_on(backend.messages(&id)).expect("messages");
        assert_eq!(posts.len(), 1);
        assert!(
            matches!(&posts[0].replies.last().expect("reply").body, Body::Html(h) if h == "<p>on it</p>")
        );
    }

    #[test]
    fn sent_text_is_escaped() {
        let backend = DemoBackend::new();
        let id = ConversationId("dm-jonas".into());
        block_on(backend.send(&id, None, "<b>hi</b>")).expect("send");
        let messages = block_on(backend.messages(&id)).expect("messages");
        assert!(
            matches!(&messages.last().expect("message").body, Body::Html(h) if h == "<p>&lt;b&gt;hi&lt;/b&gt;</p>")
        );
    }

    #[test]
    fn activity_marks_target_unread() {
        let backend = DemoBackend::new();
        let changed = backend.simulate_activity().expect("lands");
        let list = block_on(backend.conversations()).expect("list");
        assert!(list.iter().any(|c| c.id == changed && c.unread));
    }
}
