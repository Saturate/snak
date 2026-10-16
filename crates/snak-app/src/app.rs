use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogAction, DialogClose, DialogFooter},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    menu::{ContextMenuExt as _, PopupMenuItem},
    message_scroller::{MessageScroller, MessageScrollerState},
    text::TextView,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use snak_chat::{
    Body, ChatBackend, ChatError, Connector, Conversation, ConversationId, Event, LoginPrompt,
    Message, MessageId,
};

use crate::prefs::{Placement, Prefs};
use crate::sidebar;
use crate::{CloseThread, JumpTo, NextConversation, PreviousConversation, io};

// Push is the main update path; polling is the fallback when push is down
// or a backend doesn't have it.
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const DM_LIMIT: usize = 40;
const SIDEBAR_WIDTH: f32 = 272.;
const THREAD_WIDTH: f32 = 400.;

struct SidebarGroups {
    starred: Vec<Conversation>,
    sections: Vec<(usize, String, Vec<Conversation>)>,
    channels: Vec<Conversation>,
    dms: Vec<Conversation>,
}

impl SidebarGroups {
    fn flat(self) -> Vec<Conversation> {
        let mut all = self.starred;
        for (_, _, entries) in self.sections {
            all.extend(entries);
        }
        all.extend(self.channels);
        all.extend(self.dms);
        all
    }
}

/// Opens the thread panel for a channel post.
type OnThread = Rc<dyn Fn(MessageId, &mut Window, &mut App)>;

enum Screen {
    Starting,
    Login { prompt: Option<LoginPrompt> },
    Main,
}

pub struct Snak {
    focus: FocusHandle,
    connector: Arc<dyn Connector>,
    backend: Option<Arc<dyn ChatBackend>>,
    screen: Screen,
    error: Option<SharedString>,
    live: bool,

    conversations: Vec<Conversation>,
    jump: Entity<InputState>,
    prefs: Prefs,
    show_hidden: bool,
    section_name: Entity<InputState>,

    selected: Option<ConversationId>,
    showing_unreads: bool,
    rows: Rc<Vec<Message>>,
    scroller: Entity<MessageScrollerState>,
    composer: Entity<TextareaState>,

    thread_root: Option<MessageId>,
    thread_rows: Rc<Vec<Message>>,
    thread_scroller: Entity<MessageScrollerState>,
    thread_composer: Entity<TextareaState>,

    auth_task: Option<Task<()>>,
    sidebar_task: Option<Task<()>>,
    conversation_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    events_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Snak {
    pub fn new(connector: Arc<dyn Connector>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let jump = cx.new(|cx| InputState::new(window, cx).placeholder("Jump to…  ⌘K"));
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("Message")
        });
        let thread_composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder("Reply")
        });
        let scroller = cx.new(|cx| MessageScrollerState::new(0, cx));
        let thread_scroller = cx.new(|cx| MessageScrollerState::new(0, cx));

        let subscriptions = vec![
            cx.observe(&scroller, |_, _, cx| cx.notify()),
            cx.observe(&thread_scroller, |_, _, cx| cx.notify()),
            cx.subscribe_in(&jump, window, |this, _, event, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => this.open_first_match(window, cx),
                _ => {}
            }),
            cx.subscribe_in(&composer, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.send(false, window, cx);
                }
            }),
            cx.subscribe_in(&thread_composer, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.send(true, window, cx);
                }
            }),
        ];

        let connector_name = connector.service_name().to_string();
        let connector_name = connector_name.as_str();
        let mut this = Self {
            focus: cx.focus_handle(),
            connector,
            backend: None,
            screen: Screen::Starting,
            error: None,
            live: false,
            conversations: Vec::new(),
            jump,
            prefs: Prefs::load(connector_name),
            show_hidden: false,
            section_name: cx.new(|cx| InputState::new(window, cx).placeholder("Section name")),
            selected: None,
            showing_unreads: true,
            rows: Rc::default(),
            scroller,
            composer,
            thread_root: None,
            thread_rows: Rc::default(),
            thread_scroller,
            thread_composer,
            auth_task: None,
            sidebar_task: None,
            conversation_task: None,
            poll_task: None,
            events_task: None,
            _subscriptions: subscriptions,
        };
        window.focus(&this.focus, cx);
        this.restore(cx);
        this
    }

    // Sign-in

    fn restore(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Starting;
        let connector = self.connector.clone();
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = io(async move { connector.restore().await }).await;
            this.update(cx, |this, cx| match result {
                Ok(Some(backend)) => this.signed_in(backend, cx),
                Ok(None) => this.begin_login(cx),
                Err(e) => this.show_error(e, cx),
            })
            .ok();
        }));
        cx.notify();
    }

    fn begin_login(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Login { prompt: None };
        let connector = self.connector.clone();
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let prompt = match io({
                let connector = connector.clone();
                async move { connector.start_login().await }
            })
            .await
            {
                Ok(prompt) => prompt,
                Err(e) => {
                    this.update(cx, |this, cx| this.show_error(e, cx)).ok();
                    return;
                }
            };
            this.update(cx, |this, cx| {
                this.screen = Screen::Login {
                    prompt: Some(prompt.clone()),
                };
                cx.notify();
            })
            .ok();

            let result = io(async move { connector.finish_login(&prompt).await }).await;
            this.update(cx, |this, cx| match result {
                Ok(backend) => this.signed_in(backend, cx),
                Err(e) => this.show_error(e, cx),
            })
            .ok();
        }));
        cx.notify();
    }

    fn signed_in(&mut self, backend: Arc<dyn ChatBackend>, cx: &mut Context<Self>) {
        self.backend = Some(backend.clone());
        self.screen = Screen::Main;
        self.error = None;
        self.load_conversations(cx);
        self.listen(backend, cx);
        self.start_polling(cx);
    }

    fn show_error(&mut self, error: ChatError, cx: &mut Context<Self>) {
        if let ChatError::SessionExpired(_) = error {
            self.session_expired(cx);
            return;
        }
        self.error = Some(error.to_string().into());
        if self.backend.is_none() && !matches!(self.screen, Screen::Login { .. }) {
            self.screen = Screen::Login { prompt: None };
        }
        cx.notify();
    }

    fn session_expired(&mut self, cx: &mut Context<Self>) {
        self.connector.sign_out();
        self.backend = None;
        self.events_task = None;
        self.poll_task = None;
        self.error = Some("Your session expired. Sign in again.".into());
        self.begin_login(cx);
    }

    // Updates

    fn listen(&mut self, backend: Arc<dyn ChatBackend>, cx: &mut Context<Self>) {
        self.events_task = Some(cx.spawn(async move |this, cx| {
            // The backend spawns its push task on the network runtime.
            let Ok(mut events) = io(async move { Ok(backend.subscribe()) }).await else {
                return;
            };
            while let Some(event) = events.recv().await {
                let alive = this.update(cx, |this, cx| this.on_event(event, cx));
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::MessagesChanged(id) => {
                if self.selected.as_ref() == Some(&id) && !self.showing_unreads {
                    self.load_conversation(cx);
                }
            }
            Event::ConversationsChanged => self.load_conversations(cx),
            Event::Connected => {
                // Anything that happened while disconnected was missed.
                self.live = true;
                self.load_conversations(cx);
                self.load_conversation(cx);
            }
            Event::Reconnecting => {
                self.live = false;
                cx.notify();
            }
            Event::SessionExpired => self.session_expired(cx),
        }
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let alive = this.update(cx, |this, cx| {
                    if this.live {
                        return;
                    }
                    this.load_conversations(cx);
                    if !this.showing_unreads {
                        this.load_conversation(cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    // Sidebar

    fn load_conversations(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        self.sidebar_task = Some(cx.spawn(async move |this, cx| {
            let result = io(async move { backend.conversations().await }).await;
            this.update(cx, |this, cx| match result {
                Ok(list) => {
                    this.conversations = list;
                    cx.notify();
                }
                Err(e) => this.show_error(e, cx),
            })
            .ok();
        }));
    }

    fn update_prefs(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Prefs)) {
        change(&mut self.prefs);
        if let Err(e) = self.prefs.save(self.connector.service_name()) {
            self.error = Some(format!("Could not save sidebar settings: {e}").into());
        }
        cx.notify();
    }

    fn new_section_dialog(
        &mut self,
        first_member: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self.section_name.clone();
        input.update(cx, |state, cx| state.set_value("", window, cx));
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let view = view.clone();
            let first_member = first_member.clone();
            dialog
                .title("New section")
                .child(Input::new(&input))
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .child(Button::new("cancel").label("Cancel").outline()),
                        )
                        .child(
                            DialogAction::new()
                                .child(Button::new("create").label("Create").primary()),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let name = input.read(cx).value().to_string();
                    view.update(cx, |this, cx| {
                        this.update_prefs(cx, |p| p.add_section(&name, first_member.as_deref()))
                    });
                    true
                })
        });
        self.section_name
            .update(cx, |state, cx| state.focus(window, cx));
    }

    /// The local mute flag flips the service's own mute, so "Unmute" also
    /// works on something muted in Teams.
    fn is_muted(&self, conversation: &Conversation) -> bool {
        conversation.muted != self.prefs.muted.contains(&conversation.id.0)
    }

    fn selected_conversation(&self) -> Option<&Conversation> {
        let id = self.selected.as_ref()?;
        self.conversations.iter().find(|c| &c.id == id)
    }

    fn query(&self, cx: &App) -> String {
        self.jump.read(cx).value().trim().to_string()
    }

    fn open_first_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query(cx);
        let first = self
            .conversations
            .iter()
            .find(|c| sidebar::matches(c, &query))
            .map(|c| c.id.clone());
        if let Some(id) = first {
            self.jump
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.select(id, window, cx);
        }
    }

    fn jump_to(&mut self, _: &JumpTo, window: &mut Window, cx: &mut Context<Self>) {
        self.jump.update(cx, |state, cx| state.focus(window, cx));
    }

    // Conversations

    fn select(&mut self, id: ConversationId, window: &mut Window, cx: &mut Context<Self>) {
        self.showing_unreads = false;
        if self.selected.as_ref() != Some(&id) {
            self.selected = Some(id);
            self.thread_root = None;
            self.set_rows(Vec::new(), cx);
        }
        self.composer
            .update(cx, |state, cx| state.focus(window, cx));
        self.load_conversation(cx);
    }

    fn load_conversation(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some(id)) = (self.backend.clone(), self.selected.clone()) else {
            return;
        };
        self.conversation_task = Some(cx.spawn(async move |this, cx| {
            let requested = id.clone();
            let result = io(async move { backend.messages(&id).await }).await;
            this.update(cx, |this, cx| {
                // The user may have moved on while this was loading.
                if this.selected.as_ref() != Some(&requested) {
                    return;
                }
                match result {
                    Ok(rows) => {
                        this.set_rows(rows, cx);
                        this.mark_read(&requested, cx);
                    }
                    Err(e) => this.show_error(e, cx),
                }
            })
            .ok();
        }));
    }

    /// Only conversations the service still calls unread get a request, so
    /// refetches of an already-read conversation stay free.
    fn mark_read(&mut self, id: &ConversationId, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let Some(conversation) = self.conversations.iter_mut().find(|c| &c.id == id) else {
            return;
        };
        if !conversation.unread {
            return;
        }
        conversation.unread = false;
        let id = id.clone();
        cx.spawn(async move |this, cx| {
            let result = io(async move { backend.mark_read(&id).await }).await;
            if let Err(e) = result {
                this.update(cx, |this, cx| this.show_error(e, cx)).ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn set_rows(&mut self, rows: Vec<Message>, cx: &mut Context<Self>) {
        sync_scroller(&self.scroller, &self.rows, &rows, cx);
        self.rows = Rc::new(rows);

        if let Some(root_id) = &self.thread_root {
            let thread = self
                .rows
                .iter()
                .find(|r| &r.id == root_id)
                .map(thread_rows)
                .unwrap_or_default();
            sync_scroller(&self.thread_scroller, &self.thread_rows, &thread, cx);
            self.thread_rows = Rc::new(thread);
        }
        cx.notify();
    }

    fn open_thread(&mut self, root_id: MessageId, window: &mut Window, cx: &mut Context<Self>) {
        let thread = self
            .rows
            .iter()
            .find(|r| r.id == root_id)
            .map(thread_rows)
            .unwrap_or_default();
        self.thread_scroller
            .update(cx, |state, cx| state.reset(thread.len(), cx));
        self.thread_rows = Rc::new(thread);
        self.thread_root = Some(root_id);
        self.thread_composer
            .update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    fn close_thread(&mut self, cx: &mut Context<Self>) {
        self.thread_root = None;
        cx.notify();
    }

    fn send(&mut self, in_thread: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(backend), Some(id)) = (self.backend.clone(), self.selected.clone()) else {
            return;
        };
        let input = if in_thread {
            self.thread_composer.clone()
        } else {
            self.composer.clone()
        };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        input.update(cx, |state, cx| state.set_value("", window, cx));
        let reply_to = if in_thread {
            self.thread_root.clone()
        } else {
            None
        };
        cx.spawn(async move |this, cx| {
            let result = io(async move { backend.send(&id, reply_to.as_ref(), &text).await }).await;
            this.update(cx, |this, cx| match result {
                Ok(()) => this.load_conversation(cx),
                Err(e) => this.show_error(e, cx),
            })
            .ok();
        })
        .detach();
    }

    fn start_call(&self, cx: &mut Context<Self>) {
        let (Some(backend), Some(id)) = (&self.backend, &self.selected) else {
            return;
        };
        if let Some(url) = backend.call_url(id) {
            cx.open_url(&url);
        }
    }
}

fn thread_rows(root: &Message) -> Vec<Message> {
    let mut head = root.clone();
    head.replies = Vec::new();
    std::iter::once(head)
        .chain(root.replies.iter().cloned())
        .collect()
}

// A refetch returns the whole page; only touch the scroller when rows changed,
// so the reader's position survives.
fn sync_scroller(
    scroller: &Entity<MessageScrollerState>,
    old: &[Message],
    new: &[Message],
    cx: &mut App,
) {
    let same_prefix = old.len() <= new.len() && old.iter().zip(new).all(|(a, b)| a.id == b.id);
    scroller.update(cx, |state, cx| {
        if same_prefix && new.len() > old.len() {
            state.append(new.len() - old.len(), cx);
        } else if !same_prefix || old.len() != new.len() {
            state.reset(new.len(), cx);
        } else {
            state.remeasure(cx);
        }
    });
}

fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

fn avatar_color(name: &str) -> Hsla {
    let hash = name
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    hsla((hash % 360) as f32 / 360., 0.45, 0.45, 1.)
}

fn render_row(
    row: &Message,
    previous: Option<&Message>,
    on_thread: Option<OnThread>,
    index: usize,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let continues = previous.is_some_and(|p| row.continues(p));

    let body = match &row.body {
        Body::Html(html) => div()
            .text_sm()
            .text_color(theme.foreground)
            .child(
                TextView::html(
                    ElementId::Name(format!("msg-{}", row.id.0).into()),
                    SharedString::from(html.clone()),
                )
                .selectable(true),
            )
            .into_any_element(),
        Body::Text(text) => div()
            .text_sm()
            .text_color(theme.foreground)
            .child(text.clone())
            .into_any_element(),
        Body::Deleted | Body::Event(_) => div()
            .text_sm()
            .italic()
            .text_color(theme.muted_foreground)
            .child(match &row.body {
                Body::Event(text) => text.clone(),
                _ => "This message was deleted".to_string(),
            })
            .into_any_element(),
    };

    let footer = h_flex()
        .gap_3()
        .text_xs()
        .text_color(theme.muted_foreground)
        .when(row.edited, |el| el.child("(edited)"))
        .when(!row.reactions.is_empty(), |el| {
            el.child(
                row.reactions
                    .iter()
                    .map(|r| format!("{} {}", r.emoji, r.count))
                    .collect::<Vec<_>>()
                    .join("   "),
            )
        })
        .when_some(on_thread, |el, on_thread| {
            let label: SharedString = match row.replies.len() {
                0 => "Reply in thread".into(),
                1 => "1 reply".into(),
                n => format!("{n} replies").into(),
            };
            let id = row.id.clone();
            el.child(
                div()
                    .id(("thread-link", index))
                    .cursor_pointer()
                    .text_color(if !row.replies.is_empty() {
                        theme.link
                    } else {
                        theme.muted_foreground
                    })
                    .hover(|s| s.underline())
                    .child(label)
                    .on_click(move |_, window, cx| on_thread(id.clone(), window, cx)),
            )
        });

    let content = v_flex()
        .flex_1()
        .min_w_0()
        .gap_0p5()
        .child(body)
        .child(footer);

    if continues {
        return div()
            .id(("row", index))
            .w_full()
            .pl(px(60.))
            .pr_4()
            .py_0p5()
            .hover(|s| s.bg(theme.muted))
            .child(content)
            .into_any_element();
    }

    h_flex()
        .id(("row", index))
        .w_full()
        .items_start()
        .gap_3()
        .px_4()
        .pt_3()
        .pb_0p5()
        .hover(|s| s.bg(theme.muted))
        .child(
            div()
                .flex_none()
                .size(px(32.))
                .rounded_md()
                .bg(avatar_color(&row.author))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(white())
                .child(initials(&row.author)),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap_2()
                        .items_baseline()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::BOLD)
                                .child(row.author.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(row.time_label()),
                        ),
                )
                .child(content),
        )
        .into_any_element()
}

impl Snak {
    fn render_login(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let prompt = match &self.screen {
            Screen::Login { prompt } => prompt.clone(),
            _ => None,
        };
        let service = self.connector.service_name().to_string();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(theme.background)
            .child(div().text_3xl().font_weight(FontWeight::BOLD).child("Snak"))
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(format!("Sign in to {service}")),
            )
            .map(|el| match prompt {
                None => el.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Getting a sign-in code…"),
                ),
                Some(prompt) => {
                    let url = prompt.url.clone();
                    let user_code = prompt.code.clone();
                    el.child(
                        div()
                            .text_3xl()
                            .font_family("Menlo")
                            .font_weight(FontWeight::BOLD)
                            .child(prompt.code.clone()),
                    )
                    .child(
                        Button::new("open-login")
                            .primary()
                            .label("Copy code and open sign-in page")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(user_code.clone()));
                                cx.open_url(&url);
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("Or go to {} and enter the code.", prompt.url)),
                    )
                }
            })
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    v_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .max_w(px(560.))
                                .text_sm()
                                .text_color(theme.danger)
                                .child(error),
                        )
                        .child(
                            Button::new("retry")
                                .label("Try again")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    this.restore(cx);
                                })),
                        ),
                )
            })
    }

    fn render_sidebar_entry(
        &self,
        entry: &Conversation,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let key = entry.id.0.clone();
        let active = !self.showing_unreads && self.selected.as_ref() == Some(&entry.id);
        let muted = self.is_muted(entry);
        let hidden = self.prefs.hidden.contains(&key);
        // Muted entries never shout; that is the point of muting.
        let unread = entry.unread && !muted;
        let prefix = if entry.is_channel() { "#" } else { "•" };
        let clicked = entry.id.clone();
        let view = cx.entity();
        let placement = self.prefs.placement(&key);
        let sections: Vec<(usize, String)> = self
            .prefs
            .sections
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.name.clone()))
            .collect();

        h_flex()
            .id(("entry", ix))
            .w_full()
            .gap_1p5()
            .px_2()
            .py(px(3.))
            .rounded_md()
            .cursor_pointer()
            .text_sm()
            .map(|el| {
                if active {
                    el.bg(theme.sidebar_primary)
                        .text_color(theme.sidebar_primary_foreground)
                } else {
                    el.text_color(if unread {
                        theme.sidebar_foreground
                    } else {
                        theme.muted_foreground
                    })
                    .hover(|s| s.bg(theme.sidebar_accent))
                }
            })
            .when(muted || hidden, |el| el.opacity(0.55))
            .when(unread, |el| el.font_weight(FontWeight::BOLD))
            .child(div().flex_none().w(px(12.)).child(prefix))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(entry.title.clone()),
            )
            .when_some(entry.group().map(str::to_string), |el, detail| {
                el.child(
                    div()
                        .flex_none()
                        .max_w(px(96.))
                        .truncate()
                        .text_xs()
                        .font_weight(FontWeight::NORMAL)
                        .text_color(if active {
                            theme.sidebar_primary_foreground
                        } else {
                            theme.muted_foreground
                        })
                        .child(detail),
                )
            })
            .on_click(
                cx.listener(move |this, _, window, cx| this.select(clicked.clone(), window, cx)),
            )
            .context_menu(move |menu, window, _| {
                let item = |label: String, key: &str, f: fn(&mut Prefs, &str)| {
                    let key = key.to_string();
                    PopupMenuItem::new(label).on_click(
                        window.listener_for(&view, move |this, _, _, cx| {
                            this.update_prefs(cx, |p| f(p, &key))
                        }),
                    )
                };
                let mut menu = menu
                    .item(item(
                        if placement == Placement::Starred {
                            "Unstar"
                        } else {
                            "Star"
                        }
                        .into(),
                        &key,
                        Prefs::toggle_star,
                    ))
                    .item(item(
                        if muted { "Unmute" } else { "Mute" }.into(),
                        &key,
                        Prefs::toggle_muted,
                    ))
                    .item(item(
                        if hidden {
                            "Show in sidebar"
                        } else {
                            "Hide from sidebar"
                        }
                        .into(),
                        &key,
                        Prefs::toggle_hidden,
                    ))
                    .separator();
                for (i, name) in &sections {
                    if placement == Placement::Section(*i) {
                        continue;
                    }
                    let (i, key) = (*i, key.clone());
                    menu = menu.item(PopupMenuItem::new(format!("Move to {name}")).on_click(
                        window.listener_for(&view, move |this, _, _, cx| {
                            this.update_prefs(cx, |p| p.move_to_section(&key, i))
                        }),
                    ));
                }
                if let Placement::Section(_) = placement {
                    menu = menu.item(item(
                        "Remove from section".into(),
                        &key,
                        Prefs::remove_from_section,
                    ));
                }
                let key = key.clone();
                menu.item(
                    PopupMenuItem::new("New section…").on_click(window.listener_for(
                        &view,
                        move |this, _, window, cx| {
                            this.new_section_dialog(Some(key.clone()), window, cx)
                        },
                    )),
                )
            })
    }

    /// The sidebar's groups in display order, shared by rendering and Option+arrow navigation.
    fn sidebar_groups(&self, query: &str) -> SidebarGroups {
        let searching = !query.is_empty();
        let prefs = &self.prefs;
        let visible = |e: &&Conversation| {
            let key = e.id.0.as_str();
            let is_selected = self.selected.as_ref() == Some(&e.id);
            sidebar::matches(e, query)
                // Search reaches everything, including what you hid.
                && (searching || self.show_hidden || !prefs.hidden.contains(key))
                && (searching || !prefs.unreads_only || is_selected
                    || (e.unread && !self.is_muted(e)))
        };

        let mut all: Vec<&Conversation> = self.conversations.iter().filter(visible).collect();
        // Channels read best grouped by team; DMs by recency.
        all.sort_by(|a, b| match (a.group(), b.group()) {
            (Some(x), Some(y)) => x
                .cmp(y)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase())),
            _ => b.last_activity.cmp(&a.last_activity),
        });
        let filed = |placement: Placement| -> Vec<Conversation> {
            all.iter()
                .filter(|e| prefs.placement(&e.id.0) == placement)
                .map(|e| (*e).clone())
                .collect()
        };
        let unfiled = filed(Placement::Default);
        SidebarGroups {
            starred: filed(Placement::Starred),
            sections: prefs
                .sections
                .iter()
                .enumerate()
                .map(|(i, s)| (i, s.name.clone(), filed(Placement::Section(i))))
                .collect(),
            channels: unfiled.iter().filter(|e| e.is_channel()).cloned().collect(),
            dms: unfiled
                .iter()
                .filter(|e| !e.is_channel())
                .take(if searching { usize::MAX } else { DM_LIMIT })
                .cloned()
                .collect(),
        }
    }

    fn step_conversation(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query(cx);
        let order = self.sidebar_groups(&query).flat();
        if order.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .filter(|_| !self.showing_unreads)
            .and_then(|s| order.iter().position(|e| &e.id == s));
        let next = match current {
            Some(i) => (i as isize + delta).clamp(0, order.len() as isize - 1) as usize,
            None if delta > 0 => 0,
            None => order.len() - 1,
        };
        self.select(order[next].id.clone(), window, cx);
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.query(cx);
        let prefs = &self.prefs;
        let SidebarGroups {
            starred,
            sections,
            channels,
            dms,
        } = self.sidebar_groups(&query);
        let hidden_count = prefs.hidden.len();

        let me = self
            .backend
            .as_ref()
            .map(|b| b.me().display_name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Snak".into());

        let heading = |label: String, cx: &mut Context<Self>| {
            div()
                .px_2()
                .pt_4()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(cx.theme().muted_foreground)
                .child(label)
        };

        let mut ix = 0;
        let mut list = v_flex().px_2().pb_4();
        let mut push = |list: Div, entries: &[Conversation], cx: &mut Context<Self>| {
            let mut list = list;
            for entry in entries {
                list = list.child(self.render_sidebar_entry(entry, ix, cx));
                ix += 1;
            }
            list
        };

        let unread_count = self.unread_entries().len();
        let theme = cx.theme();
        list = list.child(
            h_flex()
                .id("all-unreads")
                .mt_1()
                .px_2()
                .py(px(3.))
                .gap_1p5()
                .rounded_md()
                .cursor_pointer()
                .text_sm()
                .map(|el| {
                    if self.showing_unreads {
                        el.bg(theme.sidebar_primary)
                            .text_color(theme.sidebar_primary_foreground)
                    } else {
                        el.text_color(theme.sidebar_foreground)
                            .hover(|s| s.bg(theme.sidebar_accent))
                    }
                })
                .child(div().flex_1().child("All unreads"))
                .when(unread_count > 0, |el| {
                    el.child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .child(unread_count.to_string()),
                    )
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.showing_unreads = true;
                    this.thread_root = None;
                    cx.notify();
                })),
        );
        if !starred.is_empty() {
            list = list.child(heading("Starred".into(), cx));
            list = push(list, &starred, cx);
        }
        for (i, name, entries) in &sections {
            let view = cx.entity();
            let i = *i;
            list = list.child(heading(name.clone(), cx).id(("section", i)).context_menu(
                move |menu, window, _| {
                    menu.item(PopupMenuItem::new("Delete section").on_click(
                        window.listener_for(&view, move |this, _, _, cx| {
                            this.update_prefs(cx, |p| p.delete_section(i))
                        }),
                    ))
                },
            ));
            list = push(list, entries, cx);
        }
        list = list.child(heading("Channels".into(), cx));
        list = push(list, &channels, cx);
        list = list.child(heading("Direct messages".into(), cx));
        list = push(list, &dms, cx);

        let theme = cx.theme();
        let unreads_only = prefs.unreads_only;
        v_flex()
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.sidebar_foreground)
                            .child(me),
                    )
                    .child(
                        Button::new("unreads-only")
                            .xsmall()
                            .map(|b| if unreads_only { b.primary() } else { b.ghost() })
                            .label("Unreads")
                            .tooltip("Show only unread conversations")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.update_prefs(cx, |p| p.unreads_only = !p.unreads_only)
                            })),
                    ),
            )
            .child(div().px_3().pb_2().child(Input::new(&self.jump).small()))
            .child(
                div()
                    .id("sidebar-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .when(hidden_count > 0, |el| {
                let label = if self.show_hidden {
                    format!("Hide {hidden_count} hidden")
                } else {
                    format!("{hidden_count} hidden · Show")
                };
                el.child(
                    div()
                        .id("toggle-hidden")
                        .px_4()
                        .py_2()
                        .text_xs()
                        .cursor_pointer()
                        .text_color(theme.muted_foreground)
                        .hover(|s| s.text_color(theme.sidebar_foreground))
                        .child(label)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_hidden = !this.show_hidden;
                            cx.notify();
                        })),
                )
            })
    }

    fn render_timeline(
        id: &'static str,
        scroller: &Entity<MessageScrollerState>,
        rows: Rc<Vec<Message>>,
        on_thread: Option<OnThread>,
    ) -> impl IntoElement {
        MessageScroller::new(id, scroller.clone(), move |index, _, cx| {
            let Some(row) = rows.get(index) else {
                return div().into_any_element();
            };
            let previous = index.checked_sub(1).and_then(|i| rows.get(i));
            render_row(row, previous, on_thread.clone(), index, cx)
        })
    }

    fn unread_entries(&self) -> Vec<Conversation> {
        let mut unread: Vec<Conversation> = self
            .conversations
            .iter()
            .filter(|e| {
                let key = e.id.0.as_str();
                e.unread && !self.is_muted(e) && !self.prefs.hidden.contains(key)
            })
            .cloned()
            .collect();
        unread.sort_by_key(|e| std::cmp::Reverse(e.last_activity));
        unread
    }

    fn render_unreads(&self, cx: &mut Context<Self>) -> AnyElement {
        let unread = self.unread_entries();
        let theme = cx.theme();
        let mut list = v_flex().gap_1().p_4();
        if unread.is_empty() {
            list = list.child(
                div()
                    .pt_16()
                    .w_full()
                    .text_center()
                    .text_color(theme.muted_foreground)
                    .child("You're all caught up."),
            );
        }
        for (i, entry) in unread.into_iter().enumerate() {
            let title: SharedString = if entry.is_channel() {
                format!("# {}", entry.title).into()
            } else {
                entry.title.clone().into()
            };
            let clicked = entry.id.clone();
            list = list.child(
                v_flex()
                    .id(("unread", i))
                    .w_full()
                    .gap_1()
                    .px_4()
                    .py_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.muted))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().font_weight(FontWeight::BOLD).child(title))
                            .when_some(entry.group().map(str::to_string), |el, team| {
                                el.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(team),
                                )
                            }),
                    )
                    .when_some(entry.preview.clone(), |el, preview| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .line_clamp(2)
                                .child(preview),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select(clicked.clone(), window, cx)
                    })),
            );
        }
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .flex_none()
                    .h(px(48.))
                    .px_4()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().font_weight(FontWeight::BOLD).child("All unreads")),
            )
            .child(
                div()
                    .id("unreads")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    fn render_conversation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let selected = match self.selected_conversation() {
            Some(selected) if !self.showing_unreads => selected,
            _ => return self.render_unreads(cx),
        };
        let is_channel = selected.is_channel();
        let title = selected.title.clone();
        let subtitle = selected.group().map(str::to_string);
        let can_call = self
            .backend
            .as_ref()
            .is_some_and(|b| b.call_url(&selected.id).is_some());

        let on_thread: Option<OnThread> = is_channel.then(|| {
            let weak = cx.entity().downgrade();
            Rc::new(move |id: MessageId, window: &mut Window, cx: &mut App| {
                weak.update(cx, |this, cx| this.open_thread(id, window, cx))
                    .ok();
            }) as OnThread
        });

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .flex_none()
                    .h(px(48.))
                    .px_4()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().font_weight(FontWeight::BOLD).child(if is_channel {
                        format!("# {title}")
                    } else {
                        title
                    }))
                    .when_some(subtitle, |el, sub| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(sub),
                        )
                    })
                    .child(div().flex_1())
                    .when(can_call, |el| {
                        el.child(
                            Button::new("call")
                                .small()
                                .ghost()
                                .label("Call")
                                .on_click(cx.listener(|this, _, _, cx| this.start_call(cx))),
                        )
                    }),
            )
            .child(div().flex_1().min_h_0().child(Self::render_timeline(
                "timeline",
                &self.scroller,
                self.rows.clone(),
                on_thread,
            )))
            .child(
                div()
                    .flex_none()
                    .px_4()
                    .pb_4()
                    .pt_1()
                    .child(Textarea::new(&self.composer)),
            )
            .into_any_element()
    }

    fn render_thread(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .flex_none()
            .w(px(THREAD_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .flex_none()
                    .h(px(48.))
                    .px_4()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().font_weight(FontWeight::BOLD).child("Thread"))
                    .child(div().flex_1())
                    .child(
                        Button::new("close-thread")
                            .small()
                            .ghost()
                            .label("Close")
                            .on_click(cx.listener(|this, _, _, cx| this.close_thread(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(Self::render_timeline(
                "thread",
                &self.thread_scroller,
                self.thread_rows.clone(),
                None,
            )))
            .child(
                div()
                    .flex_none()
                    .px_4()
                    .pb_4()
                    .pt_1()
                    .child(Textarea::new(&self.thread_composer)),
            )
    }
}

impl Render for Snak {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let root = div()
            .size_full()
            .track_focus(&self.focus)
            .key_context("Snak")
            .on_action(cx.listener(Self::jump_to))
            .on_action(cx.listener(|this, _: &NextConversation, window, cx| {
                this.step_conversation(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousConversation, window, cx| {
                this.step_conversation(-1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseThread, _, cx| this.close_thread(cx)))
            .bg(theme.background)
            .text_color(theme.foreground);

        match self.screen {
            Screen::Starting => root
                .child(
                    v_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .text_color(theme.muted_foreground)
                        .child(format!("Connecting to {}…", self.connector.service_name())),
                )
                .into_any_element(),
            Screen::Login { .. } => root.child(self.render_login(cx)).into_any_element(),
            Screen::Main => {
                let error = self.error.clone();
                root.child(
                    v_flex()
                        .size_full()
                        .child(
                            h_flex()
                                .flex_1()
                                .min_h_0()
                                .child(self.render_sidebar(cx))
                                .child(self.render_conversation(cx))
                                .when(self.thread_root.is_some(), |el| {
                                    el.child(self.render_thread(cx))
                                }),
                        )
                        .when_some(error, |el, error| {
                            el.child(
                                h_flex()
                                    .id("error-bar")
                                    .flex_none()
                                    .px_4()
                                    .py_1()
                                    .gap_2()
                                    .text_xs()
                                    .bg(cx.theme().danger)
                                    .text_color(cx.theme().danger_foreground)
                                    .cursor_pointer()
                                    .child(div().flex_1().truncate().child(error))
                                    .child("Dismiss")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.error = None;
                                        cx.notify();
                                    })),
                            )
                        }),
                )
                .into_any_element()
            }
        }
    }
}
