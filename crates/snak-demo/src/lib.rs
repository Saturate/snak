//! A fake workspace in memory. It lets the UI run with no account, and it
//! posts a message now and then so unread state and push updates get exercised.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use snak_chat::{
    Body, BoxFuture, ChatBackend, ChatError, Connector, Conversation, ConversationId,
    ConversationKind, Event, Group, ImageRef, LoginPrompt, Me, Message, MessageId, MessagesPage,
    Profile, Reaction,
};
use tokio::sync::mpsc;

const ME_ID: &str = "me";
/// One picture in Maja's DM, so the image viewer can be tried offline.
const BOARD_URL: &str = "demo:board";
const BOARD: &[u8] = include_bytes!("../assets/board.jpg");
const ACTIVITY_EVERY: Duration = Duration::from_secs(40);
/// The demo pages like a real service, so scrolling up has history to load.
const PAGE: usize = 20;

struct Person {
    id: &'static str,
    name: &'static str,
    title: &'static str,
}

const PEOPLE: [Person; 5] = [
    Person {
        id: "maja",
        name: "Maja Holm",
        title: "Product Designer",
    },
    Person {
        id: "jonas",
        name: "Jonas Berg",
        title: "Backend Engineer",
    },
    Person {
        id: "sara",
        name: "Sara Lind",
        title: "Engineering Manager",
    },
    Person {
        id: "omar",
        name: "Omar Haddad",
        title: "Site Reliability Engineer",
    },
    Person {
        id: "ida",
        name: "Ida Kjær",
        title: "Frontend Engineer",
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
        Self::default()
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
        // Played once: looping posted the same lines again every few minutes.
        let (conversation, author, html) = *SCRIPT.get(state.script_step)?;
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
        Self {
            me: Me {
                id: ME_ID.into(),
                display_name: "Demo User".into(),
            },
            state: Mutex::new(seed(Utc::now())),
        }
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

    fn messages<'a>(&'a self, id: &'a ConversationId) -> BoxFuture<'a, MessagesPage> {
        let result = match self.state().thread(id) {
            Some(thread) => {
                // Opening a conversation reads it, as it would in a real client.
                thread.conversation.unread = false;
                let start = thread.messages.len().saturating_sub(PAGE);
                Ok(MessagesPage {
                    messages: thread.messages[start..].to_vec(),
                    older: older_cursor(&thread.messages, start),
                })
            }
            None => Err(ChatError::Other(format!("no conversation {}", id.0))),
        };
        Box::pin(async move { result })
    }

    fn older_messages<'a>(
        &'a self,
        id: &'a ConversationId,
        older: &'a str,
    ) -> BoxFuture<'a, MessagesPage> {
        let result = match self.state().thread(id) {
            Some(thread) => {
                // The cursor is the id of the page's oldest message.
                let Some(end) = thread.messages.iter().position(|m| m.id.0 == older) else {
                    return Box::pin(async {
                        Err(ChatError::Other("stale older-messages cursor".into()))
                    });
                };
                let start = end.saturating_sub(PAGE);
                Ok(MessagesPage {
                    messages: thread.messages[start..end].to_vec(),
                    older: older_cursor(&thread.messages, start),
                })
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

    fn mark_read<'a>(&'a self, id: &'a ConversationId) -> BoxFuture<'a, ()> {
        if let Some(thread) = self.state().thread(id) {
            thread.conversation.unread = false;
        }
        Box::pin(async { Ok(()) })
    }

    fn image<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Vec<u8>> {
        let result = match url {
            BOARD_URL => Ok(BOARD.to_vec()),
            _ => Err(ChatError::Other(format!("the demo has no image {url}"))),
        };
        Box::pin(async move { result })
    }

    fn person(&self, key: &str) -> BoxFuture<'_, Option<Profile>> {
        // Keys are a person id from a message's author, or their DM's id.
        let id = key.strip_prefix("dm-").unwrap_or(key);
        let profile = PEOPLE.iter().find(|p| p.id == id).map(|p| {
            let dm = ConversationId(format!("dm-{}", p.id));
            Profile {
                display_name: p.name.to_string(),
                photo: None,
                mail: Some(format!("{}@example.com", p.id)),
                job_title: Some(p.title.to_string()),
                phones: Vec::new(),
                dm: self.state().thread(&dm).is_some().then_some(dm),
            }
        });
        Box::pin(async move { Ok(profile) })
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
            // A still workspace for screenshots: nothing arrives mid-capture.
            // Waiting forever with `tx` in scope keeps the stream open, so the
            // app still reads as live.
            if std::env::var_os("SNAK_DEMO_STILL").is_some() {
                std::future::pending::<()>().await;
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
            let backend: Arc<dyn ChatBackend> = Arc::new(DemoBackend::default());
            Ok(Some(backend))
        })
    }

    fn start_login(&self) -> BoxFuture<'_, LoginPrompt> {
        Box::pin(async {
            Ok(LoginPrompt {
                code: "DEMO".into(),
                url: "https://example.com".into(),
                handle: String::default(),
                expires_in_secs: 600,
                poll_interval_secs: 1,
            })
        })
    }

    fn finish_login<'a>(&'a self, _: &'a LoginPrompt) -> BoxFuture<'a, Arc<dyn ChatBackend>> {
        Box::pin(async {
            let backend: Arc<dyn ChatBackend> = Arc::new(DemoBackend::default());
            Ok(backend)
        })
    }

    fn sign_out(&self) {}

    fn set_credential_storage(&self, _: snak_chat::CredentialStorage) -> BoxFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// The cursor for the page starting at `start`: the id of the message
/// before it, if the history runs deeper.
fn older_cursor(messages: &[Message], start: usize) -> Option<String> {
    start
        .checked_sub(1)
        .and_then(|older| messages.get(older))
        .map(|m| m.id.0.clone())
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn preview(author: &str, html: &str) -> String {
    let mut text = String::default();
    let mut tag: Option<String> = None;
    for c in html.chars() {
        match (c, tag.as_mut()) {
            ('<', None) => tag = Some(String::new()),
            ('>', Some(name)) => {
                // Block ends separate words, or "<li>a</li><li>b</li>" reads "ab".
                let closes_block = ["/p", "/li", "/div", "/pre", "br"]
                    .iter()
                    .any(|t| name.starts_with(t));
                if closes_block && !text.ends_with(' ') {
                    text.push(' ');
                }
                tag = None;
            }
            (c, Some(name)) => name.push(c),
            (c, None) => text.push(c),
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
        images: Vec::new(),
        replies: Vec::new(),
        pending: false,
        mentions_me: false,
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
            group: Group {
                id: group.to_lowercase().replace(' ', "-"),
                name: group.into(),
            },
        },
        title: title.into(),
        unread: false,
        muted: false,
        last_activity: None,
        preview: None,
    };
    let chat = |id: &str, kind: ConversationKind, title: &str| Conversation {
        id: ConversationId(id.into()),
        kind,
        title: title.into(),
        unread: false,
        muted: false,
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

    backend_post.reactions = vec![Reaction {
        emoji: "🙏".into(),
        count: 2,
    }];
    let mut deploy_post = message(
        id(),
        "ida",
        ago(180),
        "<p>Heads up: the new order queue goes to <b>staging</b> at 14:00. Shout if you have anything in flight.</p>",
    );
    deploy_post.reactions = vec![Reaction {
        emoji: "👍".into(),
        count: 3,
    }];
    // The markup Teams sends for a code block: the language on the <pre>,
    // <br> between lines.
    let repro_post = message(
        id(),
        "sara",
        ago(40),
        "<p>Repro for the pool exhaustion, in case it comes back:</p>\
         <pre class=\"language-bash\"><code>for i in $(seq 50); do<br>\
         &nbsp;&nbsp;curl -s -o /dev/null -w '%{time_total}\\n' \\<br>\
         &nbsp;&nbsp;&nbsp;&nbsp;https://staging.example.com/orders &amp;<br>\
         done<br>wait</code></pre>",
    );
    let mut review_post = message(
        id(),
        "omar",
        ago(12),
        "<p><a href=\"snak-mention:0\">@Demo User</a> could you look at the pool-size alert before we merge? \
         It pages at 80% for five minutes.</p>",
    );
    review_post.mentions_me = true;

    // Enough backlog in #general that scrolling up has pages to load.
    let mut backlog: Vec<Message> = (1..=3 * PAGE)
        .map(|n| {
            let author = &PEOPLE[n % PEOPLE.len()];
            message(
                id(),
                author.id,
                ago(1440 + 60 * (3 * PAGE - n) as i64),
                &format!("<p>Old message {n}, from the backlog.</p>"),
            )
        })
        .collect();
    backlog.push(message(
        id(),
        "sara",
        ago(1440),
        "<p>Welcome Ida, who joins the platform team today 👋</p>",
    ));

    let mut threads = vec![
        Thread {
            conversation: channel("eng-general", "Engineering", "general"),
            messages: backlog,
        },
        Thread {
            conversation: channel("eng-backend", "Engineering", "backend"),
            messages: vec![deploy_post, backend_post, repro_post, review_post],
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
                Message {
                    images: vec![ImageRef {
                        url: BOARD_URL.into(),
                        width: Some(960.),
                        height: Some(673.),
                    }],
                    ..message(
                        id(),
                        "maja",
                        ago(15),
                        "<p>Here's where the board is at.</p>",
                    )
                },
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
        let backend = DemoBackend::default();
        let id = ConversationId("eng-backend".into());
        let before = block_on(backend.conversations()).expect("list");
        assert!(before.iter().any(|c| c.id == id && c.unread));
        block_on(backend.messages(&id)).expect("messages");
        let after = block_on(backend.conversations()).expect("list");
        assert!(after.iter().any(|c| c.id == id && !c.unread));
    }

    #[test]
    fn only_the_newest_page_is_loaded() {
        let backend = DemoBackend::default();
        let id = ConversationId("eng-general".into());
        let page = block_on(backend.messages(&id)).expect("messages");
        assert_eq!(page.messages.len(), PAGE);
        assert!(page.older.is_some());
    }

    #[test]
    fn older_pages_run_out() {
        let backend = DemoBackend::default();
        let id = ConversationId("eng-general".into());
        let first = block_on(backend.messages(&id)).expect("messages");
        assert_eq!(first.messages.len(), PAGE);
        let mut seen: Vec<String> = first.messages.iter().map(|m| m.id.0.clone()).collect();
        let mut cursor = first.older;
        while let Some(older) = cursor.take() {
            let page = block_on(backend.older_messages(&id, &older)).expect("older");
            assert!(!page.messages.is_empty());
            seen.extend(page.messages.iter().map(|m| m.id.0.clone()));
            cursor = page.older;
        }
        // Every id shows exactly once, and the backlog runs deeper than a
        // single page.
        assert!(seen.len() > PAGE);
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len());
    }

    #[test]
    fn replies_land_under_their_post() {
        let backend = DemoBackend::default();
        let id = ConversationId("eng-backend".into());
        let posts = block_on(backend.messages(&id)).expect("messages");
        let count = posts.messages.len();
        let root = posts.messages[0].id.clone();
        block_on(backend.send(&id, Some(&root), "on it")).expect("send");
        let posts = block_on(backend.messages(&id)).expect("messages");
        assert_eq!(posts.messages.len(), count);
        assert!(
            matches!(&posts.messages[0].replies.last().expect("reply").body, Body::Html(h) if h == "<p>on it</p>")
        );
    }

    #[test]
    fn sent_text_is_escaped() {
        let backend = DemoBackend::default();
        let id = ConversationId("dm-jonas".into());
        block_on(backend.send(&id, None, "<b>hi</b>")).expect("send");
        let messages = block_on(backend.messages(&id)).expect("messages");
        assert!(
            matches!(&messages.messages.last().expect("message").body, Body::Html(h) if h == "<p>&lt;b&gt;hi&lt;/b&gt;</p>")
        );
    }

    #[test]
    fn previews_keep_list_items_apart() {
        assert_eq!(
            preview(
                "maja",
                "<ul><li>Scope agreed</li><li>Estimate due Friday</li></ul>"
            ),
            "Maja Holm: Scope agreed Estimate due Friday"
        );
    }

    #[test]
    fn activity_plays_once() {
        let backend = DemoBackend::default();
        let landed = std::iter::from_fn(|| backend.simulate_activity())
            .take(10)
            .count();
        assert_eq!(landed, 4);
    }

    #[test]
    fn activity_marks_target_unread() {
        let backend = DemoBackend::default();
        let changed = backend.simulate_activity().expect("lands");
        let list = block_on(backend.conversations()).expect("list");
        assert!(list.iter().any(|c| c.id == changed && c.unread));
    }
}
