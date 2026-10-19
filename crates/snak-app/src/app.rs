use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, WindowExt as _,
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    command::CommandState,
    dialog::{DialogAction, DialogClose, DialogFooter},
    h_flex,
    highlighter::HighlightTheme,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
    message_scroller::{MessageScroller, MessageScrollerState},
    status_bar::StatusBar,
    text::{TextView, TextViewStyle},
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::code_blocks;
use snak_chat::{
    Body, ChatBackend, ChatError, Connector, Conversation, ConversationId, ConversationKind, Event,
    Group, ImageRef, LoginPrompt, Message, MessageId, Profile,
};

use crate::prefs::{Placement, Prefs, group_key, team_key};
use crate::settings::{Appearance, Settings};
use crate::settings_window::SettingsWindow;
use crate::sidebar;
use crate::*;

// Push is the main update path; polling is the fallback when push is down
// or a backend doesn't have it.
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const SIDEBAR_WIDTH: f32 = 272.;
const THREAD_WIDTH: f32 = 400.;

enum SidebarRow {
    Header(Header),
    Item {
        conversation: Conversation,
        depth: usize,
        /// Show the team name next to a channel shown outside its team.
        show_group: bool,
    },
}

#[derive(Clone)]
struct Header {
    /// Collapse key in prefs.
    key: String,
    label: String,
    depth: usize,
    collapsed: bool,
    /// Set for team headers, which can be filed into sections.
    team: Option<String>,
    section: Option<usize>,
}

impl Header {
    fn new(key: &str, label: &str, depth: usize) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            depth,
            collapsed: false,
            team: None,
            section: None,
        }
    }
}

#[derive(Clone)]
enum ImageState {
    Loading,
    Ready(LoadedImage),
    Failed(std::time::Instant),
}

#[derive(Clone)]
struct LoadedImage {
    data: Arc<Image>,
    /// Real pixel size, read from the header.
    size: Option<(u32, u32)>,
    /// First frame of an animated GIF, shown while it isn't playing;
    /// GPUI's img has no pause, so a still is the way to stop it.
    still: Option<Arc<Image>>,
}

fn first_frame_png(bytes: &[u8]) -> Option<Arc<Image>> {
    use image::AnimationDecoder as _;
    let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).ok()?;
    let mut frames = decoder.into_frames();
    let first = frames.next()?.ok()?;
    // A single-frame GIF is just a picture; nothing to pause.
    frames.next()?.ok()?;
    let mut png = Vec::new();
    first
        .into_buffer()
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(Arc::new(Image::from_bytes(ImageFormat::Png, png)))
}

/// GIFs the user clicked; they play when this differs from the autoplay setting.
type GifToggles = Rc<RefCell<std::collections::HashSet<String>>>;

/// Shared with the timeline renderers, which run outside this view's update.
type ImageCache = Rc<RefCell<HashMap<String, ImageState>>>;

/// Opens the thread panel for a channel post.
type OnThread = Rc<dyn Fn(MessageId, &mut Window, &mut App)>;
type OpenProfile = Arc<dyn Fn(String, &mut Window, &mut App) + Send + Sync>;

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
    /// Bumped whenever a session ends. Detached requests (send, mark read)
    /// outlive `end_session`, so they check it before reporting back: an old
    /// session's "expired" must not sign the next one out.
    session: u64,
    /// The `session` value of the last sign-out, which throws drafts away;
    /// an expiry keeps them.
    drafts_cleared: u64,

    conversations: Vec<Conversation>,
    jump: Entity<InputState>,
    pub(crate) palette: Entity<CommandState>,
    prefs: Prefs,
    show_hidden: bool,
    section_name: Entity<InputState>,

    selected: Option<ConversationId>,
    showing_unreads: bool,
    /// A conversation to open once the list has loaded (`SNAK_OPEN`), so
    /// `scripts/screenshot.sh` can start on a channel without clicking.
    open_on_load: Option<ConversationId>,
    rows: Rc<Vec<Message>>,
    scroller: Entity<MessageScrollerState>,
    composer: Entity<TextareaState>,

    images: ImageCache,
    gif_toggles: GifToggles,
    pending: Vec<Pending>,
    next_pending: u64,
    refetch_task: Option<Task<()>>,
    refetch_started: Option<(Option<ConversationId>, std::time::Instant)>,
    login_in_flight: bool,
    reset_composer: bool,
    reset_thread_composer: bool,
    refocus_main: bool,
    /// Unsent thread replies, per conversation and thread root.
    thread_drafts: HashMap<(ConversationId, MessageId), String>,
    sidebar_loading: bool,
    sidebar_dirty: bool,
    /// Unsent composer text per conversation.
    drafts: HashMap<ConversationId, String>,
    /// Presence by person, from push; the service's own word for it.
    presences: HashMap<String, String>,
    /// Who is typing where, and until when: no "stopped" arrives.
    typing: HashMap<ConversationId, (String, std::time::Instant)>,
    thread_root: Option<MessageId>,
    thread_rows: Rc<Vec<Message>>,
    thread_scroller: Entity<MessageScrollerState>,
    thread_composer: Entity<TextareaState>,

    /// The profile panel's person, when one is open.
    profile: Option<Profile>,
    /// Their DM's messages, listed under the profile.
    profile_rows: Rc<Vec<Message>>,
    profile_scroller: Entity<MessageScrollerState>,

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
        let profile_scroller = cx.new(|cx| MessageScrollerState::new(0, cx));

        let subscriptions = vec![
            cx.observe_window_appearance(window, |this, window, cx| {
                this.apply_appearance(window, cx)
            }),
            cx.observe(&scroller, |_, _, cx| cx.notify()),
            cx.observe(&thread_scroller, |_, _, cx| cx.notify()),
            cx.observe(&profile_scroller, |_, _, cx| cx.notify()),
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
            session: 0,
            drafts_cleared: 0,
            conversations: Vec::new(),
            jump,
            palette: cx.new(|cx| CommandState::new(window, cx)),
            prefs: Prefs::load(connector_name),
            show_hidden: false,
            section_name: cx.new(|cx| InputState::new(window, cx).placeholder("Section name")),
            selected: None,
            showing_unreads: true,
            open_on_load: std::env::var("SNAK_OPEN").ok().map(ConversationId),
            rows: Rc::default(),
            scroller,
            composer,
            images: ImageCache::default(),
            gif_toggles: GifToggles::default(),
            pending: Vec::new(),
            next_pending: 0,
            refetch_task: None,
            refetch_started: None,
            login_in_flight: false,
            reset_composer: false,
            reset_thread_composer: false,
            refocus_main: false,
            thread_drafts: HashMap::new(),
            sidebar_loading: false,
            sidebar_dirty: false,
            drafts: HashMap::new(),
            presences: HashMap::new(),
            typing: HashMap::new(),
            thread_root: None,
            thread_rows: Rc::default(),
            thread_scroller,
            thread_composer,
            profile: None,
            profile_rows: Rc::default(),
            profile_scroller,
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
        // This replaces any sign-in task, which then never clears the flag itself.
        self.login_in_flight = false;
        let connector = self.connector.clone();
        // Storage first, so the saved login is read from wherever the user keeps it.
        let storage = Settings::get(cx).credential_storage();
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = io(async move {
                connector.set_credential_storage(storage).await?;
                connector.restore().await
            })
            .await;
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
        self.login_in_flight = true;
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
                    this.update(cx, |this, cx| {
                        this.login_in_flight = false;
                        this.show_error(e, cx)
                    })
                    .ok();
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
            this.update(cx, |this, cx| {
                this.login_in_flight = false;
                match result {
                    Ok(backend) => this.signed_in(backend, cx),
                    Err(e) => this.show_error(e, cx),
                }
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
        tracing::error!(error = %error, "shown to user");
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
        // Several in-flight requests can each report the expiry; the first
        // one starts the sign-in, and later ones must not replace its code.
        // A stale saved login at startup lands here too, with no backend yet.
        if self.login_in_flight {
            return;
        }
        tracing::warn!("session expired; asking to sign in again");
        // Expiry isn't the user's choice, so unsent text survives it.
        self.end_session(false, cx);
        self.error = Some("Your session expired. Sign in again.".into());
        self.begin_login(cx);
    }

    /// Drops everything tied to the signed-in account, including requests in
    /// flight, so nothing from it lands after the next sign-in.
    fn end_session(&mut self, clear_drafts: bool, cx: &mut Context<Self>) {
        self.session += 1;
        if clear_drafts {
            self.drafts_cleared = self.session;
            self.drafts.clear();
            self.thread_drafts.clear();
        } else {
            self.stash_drafts(cx);
        }
        // Composers need a window to change, so they're emptied on the next render.
        self.reset_composer = true;
        self.reset_thread_composer = true;
        // Returns at once; any slow cleanup (1Password, Touch ID) happens in
        // the background, and the sign-out already counts from here.
        self.connector.sign_out();
        self.backend = None;
        self.events_task = None;
        self.poll_task = None;
        self.sidebar_task = None;
        self.conversation_task = None;
        self.refetch_task = None;
        self.sidebar_loading = false;
        self.sidebar_dirty = false;
        self.live = false;
        self.conversations.clear();
        self.selected = None;
        self.showing_unreads = true;
        self.thread_root = None;
        self.pending.clear();
        self.error = None;
        self.set_rows(Vec::new(), cx);
    }

    /// Keeps what's typed in both composers as drafts of where it was typed.
    /// Puts text that didn't send back into its conversation's (or thread's)
    /// draft, ahead of anything typed there since.
    fn keep_unsent(
        &mut self,
        to: &ConversationId,
        in_thread: bool,
        root: Option<MessageId>,
        text: &str,
    ) {
        if in_thread {
            if let Some(root) = root {
                let key = (to.clone(), root);
                let existing = self.thread_drafts.remove(&key).unwrap_or_default();
                self.thread_drafts.insert(key, join_drafts(text, &existing));
            }
        } else {
            let existing = self.drafts.remove(to).unwrap_or_default();
            self.drafts.insert(to.clone(), join_drafts(text, &existing));
        }
    }

    fn stash_drafts(&mut self, cx: &mut Context<Self>) {
        if let Some(conversation) = self.selected.clone() {
            let typed = self.composer.read(cx).value().to_string();
            put_draft(&mut self.drafts, conversation.clone(), typed);
            if let Some(root) = self.thread_root.clone() {
                let typed = self.thread_composer.read(cx).value().to_string();
                put_draft(&mut self.thread_drafts, (conversation, root), typed);
            }
        }
    }

    /// Saves the thread composer as the open thread's draft and empties it.
    fn put_away_thread_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(conversation), Some(root)) = (self.selected.clone(), self.thread_root.clone())
        {
            let typed = self.thread_composer.read(cx).value().to_string();
            put_draft(&mut self.thread_drafts, (conversation, root), typed);
        }
        self.thread_composer
            .update(cx, |state, cx| state.set_value("", window, cx));
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
                // A message arriving ends any typing indicator for it.
                self.typing.remove(&id);
                if self.selected.as_ref() == Some(&id) && !self.showing_unreads {
                    self.refetch_soon(cx);
                }
                // Messages arriving in the profile panel's DM belong to it.
                if let Some(dm) = self.profile.as_ref().and_then(|p| p.dm.clone())
                    && dm == id
                {
                    self.open_profile(dm.0.clone(), cx);
                }
            }
            Event::ConversationsChanged => self.load_conversations(cx),
            Event::Typing { conversation, user } => {
                // No "stopped" arrives, so it expires on its own.
                self.typing.insert(
                    conversation,
                    (
                        user,
                        std::time::Instant::now() + std::time::Duration::from_secs(4),
                    ),
                );
                cx.notify();
            }
            Event::PresenceChanged { user, presence } => {
                self.presences.insert(user, presence);
                cx.notify();
            }
            Event::Connected => {
                // Anything that happened while disconnected was missed.
                self.live = true;
                self.load_conversations(cx);
                // Loading marks it read, so only when it's actually on screen.
                if !self.showing_unreads {
                    self.load_conversation(cx);
                }
            }
            Event::Reconnecting => {
                tracing::info!("push down, falling back to polling");
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
        // Push sends a change for every message anywhere. Restarting the fetch
        // each time could starve it; instead one more fetch follows this one.
        if self.sidebar_loading {
            self.sidebar_dirty = true;
            return;
        }
        self.sidebar_loading = true;
        self.sidebar_task = Some(cx.spawn(async move |this, cx| {
            let result = io(async move { backend.conversations().await }).await;
            this.update(cx, |this, cx| {
                this.sidebar_loading = false;
                match result {
                    Ok(list) => {
                        this.conversations = list;
                        cx.notify();
                    }
                    Err(e) => this.show_error(e, cx),
                }
                if std::mem::take(&mut this.sidebar_dirty) {
                    this.load_conversations(cx);
                }
            })
            .ok();
        }));
    }

    fn apply_appearance(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if Settings::get(cx).appearance == Appearance::System {
            Settings::apply(cx);
        }
    }

    fn cycle_appearance(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        Settings::update(cx, |s| s.appearance = s.appearance.next());
    }

    pub fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }

    pub fn focus_main(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    fn toggle_pref_for_selected(&mut self, cx: &mut Context<Self>, change: fn(&mut Prefs, &str)) {
        if let Some(id) = self.selected.clone().filter(|_| !self.showing_unreads) {
            self.update_prefs(cx, |p| change(p, &id.0));
        }
    }

    fn set_all_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if !collapsed {
            self.update_prefs(cx, |p| p.collapsed.clear());
            return;
        }
        // Every group, not just the visible ones: teams inside a collapsed
        // section don't render, but should still end up collapsed.
        let mut keys = vec!["starred".to_string(), "dms".to_string()];
        keys.extend(
            self.prefs
                .sections
                .iter()
                .map(|s| group_key("section", &s.name)),
        );
        keys.extend(
            self.conversations
                .iter()
                .filter_map(Conversation::group)
                .map(|g| team_key(&g.id)),
        );
        self.update_prefs(cx, |p| p.collapsed.extend(keys));
    }

    pub fn signed_in_as(&self) -> Option<String> {
        self.backend
            .as_ref()
            .map(|b| b.me().display_name.clone())
            .filter(|n| !n.is_empty())
    }

    /// Moves the saved login to wherever the settings now say. `previous`
    /// is the settings from before the change, restored if the move fails so
    /// the setting keeps pointing where the login still is.
    pub fn apply_credential_storage(&mut self, previous: Settings, cx: &mut Context<Self>) {
        let connector = self.connector.clone();
        let storage = Settings::get(cx).credential_storage();
        cx.spawn(async move |this, cx| {
            let result = io({
                let storage = storage.clone();
                async move { connector.set_credential_storage(storage).await }
            })
            .await;
            if let Err(e) = result {
                this.update(cx, |this, cx| {
                    // Unless a later change has moved on already.
                    if Settings::get(cx).credential_storage() == storage {
                        Settings::update(cx, |s| {
                            s.login_storage = previous.login_storage;
                            s.onepassword_account = previous.onepassword_account;
                        });
                    }
                    this.show_error(e, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    pub fn service_name(&self) -> String {
        self.connector.service_name().to_string()
    }

    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.end_session(true, cx);
        self.begin_login(cx);
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

    /// Teams' availability words map to Slack-style dot colors; anything
    /// unknown gets the grey of Offline rather than nothing, so a status
    /// is never silently missing.
    fn presence_color(presence: &str, theme: &gpui_kit::component::Theme) -> Hsla {
        match presence {
            "Available" => theme.success,
            "Busy" | "DoNotDisturb" => theme.danger,
            "Away" | "BeRightBack" => theme.warning,
            _ => theme.muted_foreground,
        }
    }

    /// Who is typing in a conversation, if their indicator is still live.
    fn typing_in(&self, id: &ConversationId) -> Option<String> {
        let (user, until) = self.typing.get(id)?;
        (*until > std::time::Instant::now()).then(|| user.clone())
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
        // The top row the sidebar shows, not the backend's order.
        let first = self
            .sidebar_rows(&query, cx)
            .into_iter()
            .find_map(|row| match row {
                SidebarRow::Item { conversation, .. } => Some(conversation.id),
                SidebarRow::Header(_) => None,
            });
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

    pub(crate) fn select(
        &mut self,
        id: ConversationId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.showing_unreads = false;
        if self.selected.as_ref() != Some(&id) {
            // Each conversation keeps its own unsent text, so switching never
            // carries a half-written message into the wrong place.
            let typed = self.composer.read(cx).value().to_string();
            let previous_for_thread = self.selected.clone();
            if let Some(previous) = self.selected.take() {
                put_draft(&mut self.drafts, previous, typed);
            }
            if self.thread_root.is_some() {
                // `selected` is already taken above, so restore it briefly.
                self.selected = previous_for_thread.clone();
                self.put_away_thread_draft(window, cx);
                self.selected = None;
            }
            let draft = self.drafts.remove(&id).unwrap_or_default();
            self.composer
                .update(cx, |state, cx| state.set_value(draft, window, cx));
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
                        let rows = keep_older_rows(&this.rows, rows);
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
        let session = self.session;
        cx.spawn(async move |this, cx| {
            let result = io(async move { backend.mark_read(&id).await }).await;
            if let Err(e) = result {
                this.update(cx, |this, cx| {
                    if this.session == session {
                        this.show_error(e, cx);
                    } else {
                        tracing::debug!(error = %e, "ignoring a mark-read error from an ended session");
                    }
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn set_rows(&mut self, mut rows: Vec<Message>, cx: &mut Context<Self>) {
        // Drop local copies once the service has them; give up after a while
        // so a message lost in transit doesn't sit there as sending forever.
        const PENDING_TIMEOUT_SECS: i64 = 120;
        let now = chrono::Utc::now();
        let selected = self.selected.clone();
        // Oldest first, so each server copy goes to the send it belongs to.
        let mut claimed = std::collections::HashSet::new();
        self.pending.retain(|p| {
            let fresh = p
                .message
                .sent_at
                .is_some_and(|t| (now - t).num_seconds() < PENDING_TIMEOUT_SECS);
            // Rows only say anything about the conversation they came from.
            let here = Some(&p.conversation) == selected.as_ref();
            let copy = here.then(|| arrival(&rows, p, &claimed)).flatten();
            if let Some(id) = &copy {
                claimed.insert(id.clone());
            }
            fresh && copy.is_none()
        });
        rows.retain(|m| !m.pending);
        for row in &mut rows {
            row.replies.retain(|m| !m.pending);
        }
        for p in self
            .pending
            .iter()
            .filter(|p| Some(&p.conversation) == self.selected.as_ref())
        {
            match &p.reply_to {
                None => rows.push(p.message.clone()),
                Some(root) => {
                    if let Some(post) = rows.iter_mut().find(|m| &m.id == root) {
                        post.replies.push(p.message.clone());
                    }
                }
            }
        }
        sync_scroller(&self.scroller, &self.rows, &rows, cx);
        self.rows = Rc::new(rows);
        self.load_images(cx);

        if let Some(root_id) = self.thread_root.clone() {
            match self.rows.iter().find(|r| r.id == root_id).map(thread_rows) {
                Some(thread) => {
                    sync_scroller(&self.thread_scroller, &self.thread_rows, &thread, cx);
                    self.thread_rows = Rc::new(thread);
                }
                // An empty page is a load in progress; a loaded page without
                // the root means the post is gone, so the panel goes too.
                None if !self.rows.is_empty() => {
                    self.stash_drafts(cx);
                    self.thread_root = None;
                    self.thread_rows = Rc::default();
                    self.reset_thread_composer = true;
                    self.refocus_main = true;
                }
                None => {}
            }
        }
        cx.notify();
    }

    /// Starts fetching every image in the loaded messages that isn't cached yet.
    fn load_images(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        const RETRY_AFTER: Duration = Duration::from_secs(60);
        let mut wanted: Vec<String> = self
            .rows
            .iter()
            .flat_map(|m| std::iter::once(m).chain(m.replies.iter()))
            .flat_map(|m| m.images.iter().map(|i| i.url.clone()))
            .filter(|url| match self.images.borrow().get(url) {
                None => true,
                // A blip or an expired token shouldn't hide an image for the whole session.
                Some(ImageState::Failed(at)) => at.elapsed() > RETRY_AFTER,
                Some(_) => false,
            })
            .collect();
        wanted.sort();
        wanted.dedup();
        for url in wanted {
            self.images
                .borrow_mut()
                .insert(url.clone(), ImageState::Loading);
            let backend = backend.clone();
            cx.spawn(async move |this, cx| {
                let fetched = {
                    let url = url.clone();
                    io(async move { backend.image(&url).await }).await
                };
                let state = match fetched {
                    Ok(bytes) => match image_format(&bytes) {
                        Some(format) => {
                            let size = image::ImageReader::new(std::io::Cursor::new(&bytes))
                                .with_guessed_format()
                                .ok()
                                .and_then(|r| r.into_dimensions().ok());
                            let still = (format == ImageFormat::Gif)
                                .then(|| first_frame_png(&bytes))
                                .flatten();
                            ImageState::Ready(LoadedImage {
                                data: Arc::new(Image::from_bytes(format, bytes)),
                                size,
                                still,
                            })
                        }
                        None => {
                            tracing::warn!(
                                url = crate::log::redact_url(&url),
                                magic = ?&bytes[..bytes.len().min(8)],
                                "image in an unknown format"
                            );
                            ImageState::Failed(std::time::Instant::now())
                        }
                    },
                    Err(e) => {
                        tracing::warn!(url = crate::log::redact_url(&url), error = %e, "image failed");
                        ImageState::Failed(std::time::Instant::now())
                    }
                };
                this.update(cx, |this, cx| {
                    this.images.borrow_mut().insert(url, state);
                    // Loaded images can differ from the reserved size.
                    this.scroller.update(cx, |s, cx| s.remeasure(cx));
                    this.thread_scroller.update(cx, |s, cx| s.remeasure(cx));
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    fn open_thread(&mut self, root_id: MessageId, window: &mut Window, cx: &mut Context<Self>) {
        if self.thread_root.as_ref() != Some(&root_id) {
            self.put_away_thread_draft(window, cx);
            if let Some(conversation) = self.selected.clone()
                && let Some(draft) = self.thread_drafts.remove(&(conversation, root_id.clone()))
            {
                self.thread_composer
                    .update(cx, |state, cx| state.set_value(draft, window, cx));
            }
        }
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

    // Removing the focused composer would leave key presses with no target
    // under this view, which silently disables every shortcut.
    fn close_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Escape reaches here from the main composer too; with no thread
        // open there's nothing to close and focus must stay put.
        if self.thread_root.is_none() {
            return;
        }
        self.put_away_thread_draft(window, cx);
        self.thread_root = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Opens the profile panel for whoever `key` names: a DM conversation, a
    /// member id, a mention link.
    fn open_profile(&mut self, key: String, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let key = key.to_string();
        cx.spawn(async move |this, cx| {
            let result = io(async move {
                let Some(person) = backend.person(&key).await.ok().flatten() else {
                    return Ok(None);
                };
                let rows = match person.dm.as_ref() {
                    Some(dm) => backend.messages(dm).await.ok().unwrap_or_default(),
                    None => Vec::new(),
                };
                Ok(Some((person, rows)))
            })
            .await;
            this.update(cx, |this, cx| match result.ok().flatten() {
                Some((person, rows)) => {
                    sync_scroller(&this.profile_scroller, &this.profile_rows, &rows, cx);
                    this.profile = Some(person);
                    this.profile_rows = Rc::new(rows);
                    cx.notify();
                }
                None => {
                    tracing::debug!("profile panel found nobody");
                }
            })
            .ok();
        })
        .detach();
    }

    fn close_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.profile.is_none() {
            return;
        }
        self.profile = None;
        self.profile_rows = Rc::default();
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn show_unreads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.showing_unreads = true;
        if self.thread_root.is_some() {
            self.put_away_thread_draft(window, cx);
        }
        self.thread_root = None;
        window.focus(&self.focus, cx);
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

        // Shown at once; the service's own copy replaces it when it shows up.
        self.next_pending += 1;
        let me = backend.me().clone();
        let local = Message {
            id: MessageId(format!("pending-{}", self.next_pending)),
            author: me.display_name.clone(),
            author_id: Some(me.id.clone()),
            sent_at: Some(chrono::Utc::now()),
            body: Body::Text(text.clone()),
            edited: false,
            reactions: Vec::new(),
            images: Vec::new(),
            replies: Vec::new(),
            pending: true,
            mentions_me: false,
        };
        let local_id = local.id.clone();
        let seen = self
            .rows
            .iter()
            .flat_map(|m| std::iter::once(m).chain(m.replies.iter()))
            .filter(|m| !m.pending)
            .map(|m| m.id.clone())
            .collect();
        self.pending.push(Pending {
            conversation: id.clone(),
            reply_to: reply_to.clone(),
            message: local,
            seen,
        });
        let rows = (*self.rows).clone();
        self.set_rows(rows, cx);

        let started = std::time::Instant::now();
        let sent_to = id.clone();
        let sent_in_thread = reply_to.clone();
        let session = self.session;
        cx.spawn_in(window, async move |this, cx| {
            let sent = text.clone();
            let result = io(async move { backend.send(&id, reply_to.as_ref(), &sent).await }).await;
            this.update_in(cx, |this, window, cx| match result {
                // The session it was sent from is gone. A sign-out threw its
                // text away; an expiry keeps unsent text, and the composer
                // was already emptied, so this text goes back into its draft.
                Err(_) if this.session != session => {
                    if this.drafts_cleared <= session {
                        this.keep_unsent(&sent_to, in_thread, sent_in_thread.clone(), &text);
                    }
                    tracing::debug!("a send from an ended session failed");
                }
                Ok(()) if this.session != session => {}
                Ok(()) => {
                    tracing::debug!(elapsed_ms = started.elapsed().as_millis() as u64, "sent");
                    this.refetch_soon(cx);
                }
                Err(e) => {
                    this.pending.retain(|p| p.message.id != local_id);
                    let rows = (*this.rows).clone();
                    this.set_rows(rows, cx);
                    // Nothing typed is lost: back into the box it came from if
                    // that's still on screen and empty, otherwise into its draft.
                    let same_place = this.selected.as_ref() == Some(&sent_to)
                        && (!in_thread || this.thread_root == sent_in_thread);
                    if same_place {
                        let input = if in_thread {
                            this.thread_composer.clone()
                        } else {
                            this.composer.clone()
                        };
                        let current = input.read(cx).value().to_string();
                        let restored = join_drafts(&text, &current);
                        input.update(cx, |state, cx| state.set_value(restored, window, cx));
                    } else {
                        this.keep_unsent(&sent_to, in_thread, sent_in_thread.clone(), &text);
                    }
                    this.show_error(e, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Teams' read endpoints lag a few seconds behind writes, so one refetch
    /// right after sending often misses the new message.
    fn refetch_soon(&mut self, cx: &mut Context<Self>) {
        const DELAYS_MS: [u64; 4] = [0, 1500, 4000, 10000];
        // A burst of push events would otherwise keep cancelling the fetch at
        // delay 0 before it finishes; within a second, the running schedule covers it.
        let conversation = self.selected.clone();
        if let Some((started_for, at)) = &self.refetch_started
            && *started_for == conversation
            && at.elapsed() < Duration::from_secs(1)
        {
            return;
        }
        self.refetch_started = Some((conversation.clone(), std::time::Instant::now()));
        self.refetch_task = Some(cx.spawn(async move |this, cx| {
            let mut waited = 0;
            for delay in DELAYS_MS {
                cx.background_executor()
                    .timer(Duration::from_millis(delay - waited))
                    .await;
                waited = delay;
                let still_here = this
                    .update(cx, |this, cx| {
                        let same = this.selected == conversation && !this.showing_unreads;
                        if same {
                            this.load_conversation(cx);
                        }
                        same
                    })
                    .unwrap_or(false);
                if !still_here {
                    return;
                }
            }
        }));
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

#[cfg_attr(test, derive(Clone))]
struct Pending {
    conversation: ConversationId,
    reply_to: Option<MessageId>,
    message: Message,
    /// Messages already shown when this was sent; none of them can be its copy,
    /// however close in time, so sending the same text twice works.
    seen: std::collections::HashSet<MessageId>,
}

/// Whether the service's copy of a locally sent message is in `rows`:
/// same author, same text, sent within a couple of minutes.
#[cfg(test)]
fn arrived(rows: &[Message], pending: &Pending) -> bool {
    arrival(rows, pending, &Default::default()).is_some()
}

/// The service's copy of `pending` in `rows`, if any, skipping copies another
/// pending message already claimed: two quick sends of the same text must
/// each get their own copy.
fn arrival(
    rows: &[Message],
    pending: &Pending,
    claimed: &std::collections::HashSet<MessageId>,
) -> Option<MessageId> {
    let Body::Text(text) = &pending.message.body else {
        return None;
    };
    let candidates: Vec<&Message> = match &pending.reply_to {
        None => rows.iter().collect(),
        Some(root) => rows
            .iter()
            .filter(|m| &m.id == root)
            .flat_map(|m| m.replies.iter())
            .collect(),
    };
    let want = normalise(text);
    candidates
        .into_iter()
        .filter(|m| !m.pending && !pending.seen.contains(&m.id) && !claimed.contains(&m.id))
        .find(|m| {
            m.author_id == pending.message.author_id
            // An older copy of the same text is an earlier message, not this
            // one; a little slack covers clock skew against the server.
            && match (m.sent_at, pending.message.sent_at) {
                (Some(server), Some(local)) => (-30..180).contains(&(server - local).num_seconds()),
                _ => false,
            }
            && match &m.body {
                Body::Html(html) => normalise(&strip_tags(html)) == want,
                Body::Text(t) => normalise(t) == want,
                _ => false,
            }
        })
        .map(|m| m.id.clone())
}

/// A refetch returns the newest page only. When a new message pushes the
/// oldest off, keep the rows already shown so the list grows at the end
/// instead of shifting, which would jump the reader to the bottom.
fn keep_older_rows(old: &[Message], new: Vec<Message>) -> Vec<Message> {
    const MAX_ROWS: usize = 2000;
    let Some(first) = new.first() else {
        return new;
    };
    let ids: std::collections::HashSet<&MessageId> = new.iter().map(|m| &m.id).collect();
    let mut merged: Vec<Message> = old
        .iter()
        .filter(|m| !m.pending && !ids.contains(&m.id))
        .filter(|m| match (m.sent_at, first.sent_at) {
            (Some(a), Some(b)) => a < b,
            _ => false,
        })
        .cloned()
        .collect();
    merged.extend(new);
    if merged.len() > MAX_ROWS {
        merged.drain(..merged.len() - MAX_ROWS);
    }
    merged
}

fn put_draft<K: std::hash::Hash + Eq>(drafts: &mut HashMap<K, String>, key: K, text: String) {
    if text.trim().is_empty() {
        drafts.remove(&key);
    } else {
        drafts.insert(key, text);
    }
}

/// Puts failed text back without dropping what's been typed since.
fn join_drafts(failed: &str, current: &str) -> String {
    match (failed.trim().is_empty(), current.trim().is_empty()) {
        (_, true) => failed.to_string(),
        (true, false) => current.to_string(),
        (false, false) => format!("{current}\n\n{failed}"),
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    html_escape::decode_html_entities(&out).into_owned()
}

fn normalise(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
    // Pending rows sit at the end and get swapped for the service's copy,
    // so they don't count against the prefix; that swap is a remeasure.
    let confirmed: Vec<&MessageId> = old.iter().filter(|m| !m.pending).map(|m| &m.id).collect();
    let same_prefix =
        confirmed.len() <= new.len() && confirmed.iter().zip(new).all(|(a, b)| *a == &b.id);
    scroller.update(cx, |state, cx| {
        if same_prefix && new.len() > old.len() {
            state.append(new.len() - old.len(), cx);
            // Rows already shown may have changed too, e.g. an edit or a reaction.
            state.remeasure(cx);
        } else if same_prefix && new.len() == old.len() {
            state.remeasure(cx);
        } else {
            state.reset(new.len(), cx);
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

/// Slack-style marker for a message that @mentions you: a tint and an accent bar.
fn mentioned<E: Styled>(el: E, cx: &App) -> E {
    let accent = cx.theme().warning;
    el.bg(accent.opacity(0.10))
        .border_l_2()
        .border_color(accent)
}

fn is_safe_link(url: &str) -> bool {
    let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
    matches!(scheme.as_str(), "https" | "http" | "mailto" | "msteams")
}

fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some(ImageFormat::Png),
        [0xFF, 0xD8, 0xFF, ..] => Some(ImageFormat::Jpeg),
        [b'G', b'I', b'F', b'8', ..] => Some(ImageFormat::Gif),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some(ImageFormat::Webp),
        [b'B', b'M', ..] => Some(ImageFormat::Bmp),
        _ => None,
    }
}

/// Fits the image into the timeline, keeping its aspect ratio. The real size
/// wins once known; until then the sender's reported size reserves space.
fn image_size(image: &ImageRef, actual: Option<(u32, u32)>) -> (f32, f32) {
    const MAX_W: f32 = 360.;
    const MAX_H: f32 = 300.;
    let reported = match (image.width, image.height) {
        // The attributes come from other people's messages; "inf" parses as f32.
        (Some(w), Some(h)) if w.is_finite() && h.is_finite() && w > 0. && h > 0. => Some((w, h)),
        _ => None,
    };
    let (w, h) = actual
        .filter(|(w, h)| *w > 0 && *h > 0)
        .map(|(w, h)| (w as f32, h as f32))
        .or(reported)
        .unwrap_or((240., 160.));
    let scale = (MAX_W / w).min(MAX_H / h).min(1.);
    (w * scale, h * scale)
}

fn return_image(frame: Div, data: Arc<Image>) -> AnyElement {
    frame
        .child(
            img(ImageSource::Image(data))
                .size_full()
                .object_fit(ObjectFit::Cover),
        )
        .into_any_element()
}

fn render_images(
    row: &Message,
    images: &ImageCache,
    gif_toggles: &GifToggles,
    cx: &App,
) -> Option<AnyElement> {
    if row.images.is_empty() {
        return None;
    }
    let theme = cx.theme();
    let cache = images.borrow();
    let mut list = h_flex().flex_wrap().gap_2().pt_1();
    for image in &row.images {
        let state = cache.get(&image.url);
        let actual = match state {
            Some(ImageState::Ready(loaded)) => loaded.size,
            _ => None,
        };
        let (w, h) = image_size(image, actual);
        // Sized to the image itself, so there's no letterboxing inside the frame.
        let frame = div().w(px(w)).h(px(h)).rounded_md().overflow_hidden();
        list = list.child(match state {
            Some(ImageState::Ready(loaded)) if loaded.still.is_none() => {
                return_image(frame, loaded.data.clone())
            }
            Some(ImageState::Ready(loaded)) => {
                let still = loaded.still.clone().unwrap_or_else(|| loaded.data.clone());
                let toggled = gif_toggles.borrow().contains(&image.url);
                let playing = Settings::get(cx).autoplay_gifs != toggled;
                let url = image.url.clone();
                let toggles = gif_toggles.clone();
                frame
                    .id(ElementId::Name(format!("gif-{}", image.url).into()))
                    .relative()
                    .cursor_pointer()
                    .child(
                        // GPUI keeps the GIF frame counter in element state, which needs an id.
                        img(ImageSource::Image(if playing {
                            loaded.data.clone()
                        } else {
                            still
                        }))
                        .id(ElementId::Name(image.url.clone().into()))
                        .size_full()
                        .object_fit(ObjectFit::Cover),
                    )
                    .when(!playing, |el| {
                        el.child(
                            div()
                                .absolute()
                                .left_2()
                                .bottom_2()
                                .px_1p5()
                                .rounded_sm()
                                .bg(hsla(0., 0., 0., 0.6))
                                .text_color(white())
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .flex()
                                .items_center()
                                .gap_0p5()
                                .child(Icon::new(IconName::Play).xsmall())
                                .child("GIF"),
                        )
                    })
                    .on_click(move |_, window, _| {
                        let mut toggles = toggles.borrow_mut();
                        if !toggles.remove(&url) {
                            toggles.insert(url.clone());
                        }
                        window.refresh();
                    })
                    .into_any_element()
            }
            Some(ImageState::Failed(_)) => frame
                .bg(theme.muted)
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Image unavailable")
                .into_any_element(),
            _ => frame.bg(theme.muted).into_any_element(),
        });
    }
    Some(list.into_any_element())
}

fn message_text(view: TextView, dark: bool, open_profile: OpenProfile) -> TextView {
    // Message HTML is written by other people; a click must not open file:,
    // smb: or some app's custom scheme.
    view.on_link_click(move |url, _, window, cx| {
        // Mentions open the person's profile panel.
        if let Some(key) = url.strip_prefix("snak-mention:") {
            open_profile(key.to_string(), window, cx);
        } else if is_safe_link(url) {
            cx.open_url(url);
        } else {
            tracing::warn!(scheme = url.split(':').next().unwrap_or(""), "blocked link");
        }
    })
    .style(TextViewStyle {
        // Chat messages are mostly one-line paragraphs; document spacing
        // makes every line look double-spaced.
        paragraph_gap: rems(0.2),
        highlight_theme: if dark {
            HighlightTheme::default_dark()
        } else {
            HighlightTheme::default_light()
        },
        ..TextViewStyle::default()
    })
    .selectable(true)
}

#[allow(clippy::too_many_arguments)]
fn render_row(
    row: &Message,
    previous: Option<&Message>,
    on_thread: Option<OnThread>,
    open_profile: OpenProfile,
    index: usize,
    images: &ImageCache,
    gif_toggles: &GifToggles,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let continues = previous.is_some_and(|p| row.continues(p));

    let body = match &row.body {
        Body::Html(html) => {
            let segments = code_blocks::split(html);
            let dark = theme.is_dark();
            v_flex()
                .gap_1()
                .text_sm()
                .text_color(theme.foreground)
                .children(segments.into_iter().enumerate().map(|(i, segment)| {
                    let id = ElementId::Name(format!("msg-{}-{i}", row.id.0).into());
                    match segment {
                        code_blocks::Segment::Html(html) => message_text(
                            TextView::html(id, SharedString::from(html)),
                            dark,
                            open_profile.clone(),
                        ),
                        code_blocks::Segment::Code { lang, code } => message_text(
                            TextView::markdown(
                                id,
                                SharedString::from(code_blocks::fenced(lang.as_deref(), &code)),
                            ),
                            dark,
                            open_profile.clone(),
                        ),
                    }
                }))
                .into_any_element()
        }
        Body::Text(text) => div()
            .text_sm()
            .text_color(theme.foreground)
            .child(text.clone())
            .into_any_element(),
        Body::Deleted => div()
            .text_sm()
            .italic()
            .text_color(theme.muted_foreground)
            .child("This message was deleted")
            .into_any_element(),
        Body::Event(text) => h_flex()
            .gap_1p5()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(Icon::new(IconName::Info).xsmall())
            .child(text.clone())
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
        // A pending post has no id Teams knows, so it can't be replied to yet.
        .when_some(on_thread.filter(|_| !row.pending), |el, on_thread| {
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
        .when(row.pending, |el| el.opacity(0.55))
        .child(body)
        .children(render_images(row, images, gif_toggles, cx))
        .child(footer);

    if continues {
        return div()
            .id(("row", index))
            .w_full()
            .pl(px(60.))
            .pr_4()
            .py_0p5()
            .hover(|s| s.bg(theme.muted))
            .when(row.mentions_me, |el| mentioned(el, cx))
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
        .when(row.mentions_me, |el| mentioned(el, cx))
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
                                .id(("author-link", index))
                                .cursor_pointer()
                                .text_sm()
                                .font_weight(FontWeight::BOLD)
                                .child(row.author.clone())
                                // The name opens the person's profile panel.
                                .when_some(row.author_id.clone(), |el, author_id| {
                                    let open_profile = open_profile.clone();
                                    el.on_click(move |_, window, cx| {
                                        open_profile(author_id.clone(), window, cx)
                                    })
                                }),
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
                None if self.error.is_some() => el,
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
        depth: usize,
        show_group: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let key = entry.id.0.clone();
        let active = !self.showing_unreads && self.selected.as_ref() == Some(&entry.id);
        // A DM entry opens its partner's profile panel instead of the pane.
        let panel = matches!(entry.kind, ConversationKind::Direct);
        let muted = self.is_muted(entry);
        let hidden = self.prefs.hidden.contains(&key);
        // Muted entries never shout; that is the point of muting.
        let unread = entry.unread && !muted;
        // Presence dots only make sense on a DM: a group has no one status.
        let presence_dot = if matches!(entry.kind, ConversationKind::Direct) {
            self.presences
                .get(&entry.title)
                .map(|presence| Self::presence_color(presence, theme))
        } else {
            None
        };
        let prefix = if entry.is_channel() {
            div().child("#").into_any_element()
        } else if matches!(entry.kind, snak_chat::ConversationKind::Group) {
            Icon::new(gpui_kit::assets::IconName::Users)
                .xsmall()
                .into_any_element()
        } else {
            Icon::new(IconName::User).xsmall().into_any_element()
        };
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
            .pl(px(8. + 12. * depth.saturating_sub(1) as f32))
            .pr_2()
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
            .when_some(presence_dot, |el, dot| {
                el.child(div().flex_none().size(px(7.)).rounded_full().bg(dot))
            })
            .child(div().flex_none().w(px(12.)).child(prefix))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(entry.title.clone()),
            )
            .when_some(
                entry.group().filter(|_| show_group).map(|g| g.name.clone()),
                |el, detail| {
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
                },
            )
            .when(unread && !active && Settings::get(cx).unread_dot, |el| {
                el.child(
                    div()
                        .flex_none()
                        .size(px(7.))
                        .rounded_full()
                        .bg(theme.primary),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if panel {
                    // A DM opens its partner's profile panel, not the pane.
                    this.open_profile(clicked.0.clone(), cx);
                } else {
                    this.select(clicked.clone(), window, cx);
                }
            }))
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

    /// The sidebar as display rows, shared by rendering and Option+arrow navigation.
    fn sidebar_rows(&self, query: &str, cx: &App) -> Vec<SidebarRow> {
        let settings = Settings::get(cx);
        let dm_limit = settings.dm_limit.max(1.) as usize;
        let team_names = settings.show_team_names;
        let searching = !query.is_empty();
        let prefs = &self.prefs;
        let is_selected = |c: &Conversation| self.selected.as_ref() == Some(&c.id);
        let visible = |c: &&Conversation| {
            let key = c.id.0.as_str();
            sidebar::matches(c, query)
                // Search reaches everything, including what you hid.
                && (searching || self.show_hidden || !prefs.hidden.contains(key))
                && (searching || !prefs.unreads_only || is_selected(c)
                    || (c.unread && !self.is_muted(c)))
        };
        // A collapsed group still shows what needs attention, as Slack does.
        let shown_when_collapsed =
            |c: &Conversation| is_selected(c) || (c.unread && !self.is_muted(c));
        let collapsed = |key: &str| !searching && prefs.collapsed.contains(key);
        let filtering = searching || prefs.unreads_only;

        let all: Vec<&Conversation> = self.conversations.iter().filter(visible).collect();
        let placed = |placement: Placement| {
            all.iter()
                .copied()
                .filter(move |c| prefs.placement(&c.id.0) == placement)
        };
        let team_channels = |team_id: &str| -> Vec<&Conversation> {
            let mut channels: Vec<&Conversation> = placed(Placement::Default)
                .filter(|c| c.group().is_some_and(|g| g.id == team_id))
                .collect();
            channels.sort_by_key(|c| (c.title != "general", c.title.to_lowercase()));
            channels
        };

        let mut teams: Vec<Group> = Vec::new();
        for group in self.conversations.iter().filter_map(Conversation::group) {
            if !teams.iter().any(|t| t.id == group.id) {
                teams.push(group.clone());
            }
        }
        teams.sort_by_key(|t| t.name.to_lowercase());

        let mut rows = Vec::new();
        let push_group = |rows: &mut Vec<SidebarRow>,
                          header: Header,
                          items: Vec<&Conversation>,
                          show_group: bool,
                          always_show_header: bool| {
            let is_collapsed = collapsed(&header.key);
            let depth = header.depth + 1;
            let items: Vec<SidebarRow> = items
                .into_iter()
                .filter(|c| !is_collapsed || shown_when_collapsed(c))
                .map(|c| SidebarRow::Item {
                    conversation: c.clone(),
                    depth,
                    show_group: show_group && team_names,
                })
                .collect();
            // A collapsed header normally stays so it can be reopened, but when
            // filtering, an empty group is noise whether collapsed or not.
            if items.is_empty() && !always_show_header && (!is_collapsed || filtering) {
                return;
            }
            rows.push(SidebarRow::Header(Header {
                collapsed: is_collapsed,
                ..header
            }));
            rows.extend(items);
        };

        push_group(
            &mut rows,
            Header::new("starred", "Starred", 0),
            placed(Placement::Starred).collect(),
            true,
            false,
        );

        let filing = !searching && !prefs.unreads_only;
        for (i, section) in prefs.sections.iter().enumerate() {
            let mut header = Header::new(&group_key("section", &section.name), &section.name, 0);
            header.section = Some(i);
            let loose: Vec<&Conversation> = placed(Placement::Section(i)).collect();
            // Built apart so the section header can be kept whenever anything
            // inside it shows, including a team below it.
            let mut section_rows = Vec::new();
            push_group(&mut section_rows, header.clone(), loose, true, true);
            let section_collapsed = collapsed(&header.key);
            for team in teams
                .iter()
                .filter(|t| prefs.placement(&team_key(&t.id)) == Placement::Section(i))
            {
                let channels = team_channels(&team.id);
                if section_collapsed {
                    section_rows.extend(
                        channels
                            .into_iter()
                            .filter(|c| shown_when_collapsed(c))
                            .map(|c| SidebarRow::Item {
                                conversation: c.clone(),
                                depth: 1,
                                show_group: team_names,
                            }),
                    );
                    continue;
                }
                let mut team_header = Header::new(&team_key(&team.id), &team.name, 1);
                team_header.team = Some(team.id.clone());
                push_group(&mut section_rows, team_header, channels, false, false);
            }
            let only_header = section_rows.len() == 1;
            if !only_header || filing || (section_collapsed && !filtering) {
                rows.extend(section_rows);
            }
        }

        for team in teams
            .iter()
            .filter(|t| prefs.placement(&team_key(&t.id)) == Placement::Default)
        {
            let mut header = Header::new(&team_key(&team.id), &team.name, 0);
            header.team = Some(team.id.clone());
            push_group(&mut rows, header, team_channels(&team.id), false, false);
        }

        let mut dms: Vec<&Conversation> = placed(Placement::Default)
            .filter(|c| !c.is_channel())
            .collect();
        dms.sort_by_key(|c| std::cmp::Reverse(c.last_activity));
        dms.truncate(if searching { usize::MAX } else { dm_limit });
        push_group(
            &mut rows,
            Header::new("dms", "Direct messages", 0),
            dms,
            false,
            true,
        );
        rows
    }

    fn step_conversation(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query(cx);
        let order: Vec<ConversationId> = self
            .sidebar_rows(&query, cx)
            .into_iter()
            .filter_map(|row| match row {
                SidebarRow::Item { conversation, .. } => Some(conversation.id),
                SidebarRow::Header(_) => None,
            })
            .collect();
        if order.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .filter(|_| !self.showing_unreads)
            .and_then(|s| order.iter().position(|id| id == s));
        let next = match current {
            Some(i) => (i as isize + delta).clamp(0, order.len() as isize - 1) as usize,
            None if delta > 0 => 0,
            None => order.len() - 1,
        };
        self.select(order[next].clone(), window, cx);
    }

    fn render_sidebar_header(
        &self,
        header: &Header,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let key = header.key.clone();
        let view = cx.entity();
        let team = header.team.clone();
        let section = header.section;
        let team_placement = team.as_deref().map(|t| self.prefs.placement(&team_key(t)));
        let sections: Vec<(usize, String)> = self
            .prefs
            .sections
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.name.clone()))
            .collect();

        h_flex()
            .id(("group", ix))
            .w_full()
            .gap_1()
            .pl(px(8. + 12. * header.depth as f32))
            .pr_2()
            .pt(if header.depth == 0 { px(14.) } else { px(6.) })
            .pb_1()
            .cursor_pointer()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.muted_foreground)
            .hover(|s| s.text_color(theme.sidebar_foreground))
            .child(div().min_w_0().truncate().child(header.label.clone()))
            .child(
                div().flex_none().child(
                    Icon::new(if header.collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .xsmall(),
                ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_prefs(cx, |p| p.toggle_collapsed(&key))
            }))
            .context_menu(move |menu, window, _| {
                if let Some(i) = section {
                    return menu.item(PopupMenuItem::new("Delete section").on_click(
                        window.listener_for(&view, move |this, _, _, cx| {
                            this.update_prefs(cx, |p| p.delete_section(i))
                        }),
                    ));
                }
                let Some(team) = team.clone() else {
                    return menu;
                };
                let key = team_key(&team);
                let mut menu = menu;
                for (i, name) in &sections {
                    if team_placement == Some(Placement::Section(*i)) {
                        continue;
                    }
                    let (i, key) = (*i, key.clone());
                    menu = menu.item(PopupMenuItem::new(format!("Move team to {name}")).on_click(
                        window.listener_for(&view, move |this, _, _, cx| {
                            this.update_prefs(cx, |p| p.move_to_section(&key, i))
                        }),
                    ));
                }
                if let Some(Placement::Section(_)) = team_placement {
                    let key = key.clone();
                    menu = menu.item(PopupMenuItem::new("Remove team from section").on_click(
                        window.listener_for(&view, move |this, _, _, cx| {
                            this.update_prefs(cx, |p| p.remove_from_section(&key))
                        }),
                    ));
                }
                menu.item(PopupMenuItem::new("Move team to new section…").on_click(
                    window.listener_for(&view, move |this, _, window, cx| {
                        this.new_section_dialog(Some(key.clone()), window, cx)
                    }),
                ))
            })
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.query(cx);
        let rows = self.sidebar_rows(&query, cx);
        let hidden_count = self
            .conversations
            .iter()
            .filter(|c| self.prefs.hidden.contains(&c.id.0))
            .count();

        let unread_count = self.unread_entries().len();
        let theme = cx.theme();
        let mut list = v_flex().px_2().pb_4().child(
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
                .on_click(cx.listener(|this, _, window, cx| this.show_unreads(window, cx))),
        );
        for (ix, row) in rows.iter().enumerate() {
            list = match row {
                SidebarRow::Header(header) => {
                    list.child(self.render_sidebar_header(header, ix, cx))
                }
                SidebarRow::Item {
                    conversation,
                    depth,
                    show_group,
                } => {
                    list.child(self.render_sidebar_entry(conversation, ix, *depth, *show_group, cx))
                }
            };
        }

        let theme = cx.theme();
        let unreads_only = self.prefs.unreads_only;
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
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("Conversations"),
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
        open_profile: OpenProfile,
        images: ImageCache,
        gif_toggles: GifToggles,
    ) -> impl IntoElement {
        MessageScroller::new(id, scroller.clone(), move |index, _, cx| {
            let Some(row) = rows.get(index) else {
                return div().into_any_element();
            };
            let previous = index.checked_sub(1).and_then(|i| rows.get(i));
            render_row(
                row,
                previous,
                on_thread.clone(),
                open_profile.clone(),
                index,
                &images,
                &gif_toggles,
                cx,
            )
        })
        // The scroller puts 2rem under every row, for chat bubbles; rows here
        // carry their own top padding, Slack style, so the extra gap goes.
        .with_row_style({
            let mut style = StyleRefinement::default();
            style.padding.bottom = Some(px(0.).into());
            style
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
                            .when_some(entry.group().map(|g| g.name.clone()), |el, team| {
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
        let subtitle = selected.group().map(|g| g.name.clone());
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
        let open_profile: OpenProfile = {
            let weak = cx.entity().downgrade();
            Arc::new(move |key: String, _: &mut Window, cx: &mut App| {
                weak.update(cx, |this, cx| this.open_profile(key, cx)).ok();
            }) as OpenProfile
        };

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
                open_profile.clone(),
                self.images.clone(),
                self.gif_toggles.clone(),
            )))
            .child(
                div()
                    .flex_none()
                    .px_4()
                    .pb_4()
                    .pt_1()
                    .child(Textarea::new(&self.composer)),
            )
            .when_some(self.typing_in(&selected.id), |el, user| {
                el.child(div().flex_none().pt_0().px_1().text_sm().child(format!(
                    // Unknown people still type; they just go no name.
                    "{user} is typing…"
                )))
            })
            .into_any_element()
    }

    fn render_thread(&self, open_profile: OpenProfile, cx: &mut Context<Self>) -> impl IntoElement {
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
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_thread(window, cx)),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(Self::render_timeline(
                "thread",
                &self.thread_scroller,
                self.thread_rows.clone(),
                None,
                open_profile.clone(),
                self.images.clone(),
                self.gif_toggles.clone(),
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

    /// The profile side panel: a person's own details over their DM.
    fn render_profile(
        &self,
        open_profile: OpenProfile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let Some(person) = self.profile.as_ref() else {
            return div().into_any_element();
        };
        let presence_dot = self
            .presences
            .get(&person.display_name)
            .map(|presence| Self::presence_color(presence, theme));
        let mail = person.mail.clone();
        let call_url = person
            .dm
            .as_ref()
            .and_then(|dm| self.backend.as_ref().and_then(|b| b.call_url(dm)));
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
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child(person.display_name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .when_some(person.job_title.clone(), |el, title| {
                                el.child(div().text_xs().child(title))
                            }),
                    )
                    .when_some(presence_dot, |el, dot| {
                        el.child(div().flex_none().size(px(7.)).rounded_full().bg(dot))
                    })
                    .when_some(mail.clone(), |el, mail| {
                        el.child(
                            Button::new("profile-email")
                                .small()
                                .ghost()
                                .label("Email")
                                .on_click(move |_, _, cx| {
                                    let mailto = format!("mailto:{mail}");
                                    cx.open_url(&mailto)
                                }),
                        )
                    })
                    .when_some(call_url.clone(), |el, url| {
                        el.child(
                            Button::new("profile-call")
                                .small()
                                .ghost()
                                .label("Call")
                                .on_click(move |_, _, cx| {
                                    let url = url.clone();
                                    cx.open_url(&url)
                                }),
                        )
                    })
                    .child(
                        Button::new("close-profile")
                            .small()
                            .ghost()
                            .label("Close")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_profile(window, cx)),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(Self::render_timeline(
                "profile",
                &self.profile_scroller,
                self.profile_rows.clone(),
                None,
                open_profile,
                self.images.clone(),
                self.gif_toggles.clone(),
            )))
            .into_any_element()
    }
}

impl Snak {
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let title = match self.selected_conversation() {
            Some(c) if !self.showing_unreads => match c.group() {
                Some(group) => format!("{} › #{}", group.name, c.title),
                None => c.title.clone(),
            },
            _ => "All unreads".to_string(),
        };
        TitleBar::new().child(
            h_flex()
                .w_full()
                .gap_2()
                .pr_2()
                .text_sm()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Snak"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(title),
                )
                .child(self.render_account_menu(cx)),
        )
    }

    /// Top right, where Zed (and Teams) keep the signed-in user.
    fn render_account_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self
            .signed_in_as()
            .unwrap_or_else(|| "Not signed in".into());
        let service = self.service_name();
        let view = cx.entity();
        let first_name = name.split_whitespace().next().unwrap_or(&name).to_string();
        Button::new("account")
            .ghost()
            .small()
            .tooltip(name.clone())
            .child(
                h_flex()
                    .gap_1p5()
                    .child(Avatar::new().name(name.clone()).small())
                    .child(div().text_sm().child(first_name)),
            )
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, _| {
                let item = |label: &str, run: fn(&mut Snak, &mut Window, &mut Context<Snak>)| {
                    PopupMenuItem::new(label.to_string()).on_click(
                        window
                            .listener_for(&view, move |this, _, window, cx| run(this, window, cx)),
                    )
                };
                menu.min_w(220.)
                    .label(name.clone())
                    .label(service.clone())
                    .separator()
                    .item(item("Command palette…", |this, window, cx| {
                        this.open_palette(window, cx)
                    }))
                    .item(item("Settings…", |_, _, cx| {
                        let main = cx.entity().downgrade();
                        cx.defer(move |cx| SettingsWindow::open(main, cx));
                    }))
                    .item(item("Cycle theme mode", |this, window, cx| {
                        this.cycle_appearance(window, cx)
                    }))
                    .item(item("Open log file", |_, _, cx| {
                        if let Some(path) = crate::log::path() {
                            cx.open_with_system(&path);
                        }
                    }))
                    .separator()
                    .item(item("Sign out", |this, _, cx| this.sign_out(cx)))
            })
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (dot, state) = if self.live {
            (theme.success, "Live")
        } else {
            (theme.warning, "Polling")
        };
        let mut left = h_flex()
            .gap_1p5()
            .child(div().size(px(7.)).rounded_full().bg(dot))
            .child(state)
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(self.connector.service_name().to_string()),
            );
        if let Some(error) = self.error.clone() {
            left = left.child(
                div()
                    .id("status-error")
                    .ml_3()
                    .max_w(px(640.))
                    .truncate()
                    .cursor_pointer()
                    .text_color(theme.danger)
                    .child(error)
                    .tooltip(|window, cx| Tooltip::new("Click to dismiss").build(window, cx))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.error = None;
                        cx.notify();
                    })),
            );
        }
        StatusBar::new().text_xs().left(left).right(
            Button::new("appearance")
                .xsmall()
                .ghost()
                .label(format!("Theme: {}", Settings::get(cx).appearance.label()))
                .on_click(cx.listener(|this, _, window, cx| this.cycle_appearance(window, cx))),
        )
    }
}

impl Render for Snak {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.conversations.is_empty()
            && let Some(id) = self.open_on_load.take()
        {
            cx.defer_in(window, move |this, window, cx| this.select(id, window, cx));
        }
        // Changes queued by code that had no window at hand.
        if std::mem::take(&mut self.reset_composer) {
            let composer = self.composer.clone();
            window.defer(cx, move |window, cx| {
                composer.update(cx, |state, cx| state.set_value("", window, cx))
            });
        }
        if std::mem::take(&mut self.reset_thread_composer) {
            let composer = self.thread_composer.clone();
            window.defer(cx, move |window, cx| {
                composer.update(cx, |state, cx| state.set_value("", window, cx))
            });
        }
        // Only rescue focus if it was in the thread composer that just went
        // away; anywhere else, the user is typing and it stays put.
        if std::mem::take(&mut self.refocus_main)
            && self
                .thread_composer
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        {
            let focus = self.focus.clone();
            window.defer(cx, move |window, cx| window.focus(&focus, cx));
        }
        let theme = cx.theme();
        let root = div()
            .size_full()
            .track_focus(&self.focus)
            .key_context("Snak")
            .on_action(cx.listener(Self::jump_to))
            .on_action(cx.listener(|_, _: &OpenSettings, _, cx| {
                // The settings window reads this view while rendering, which
                // GPUI forbids while this view is mid-update; open it after.
                let main = cx.entity().downgrade();
                cx.defer(move |cx| SettingsWindow::open(main, cx));
            }))
            .on_action(cx.listener(|this, _: &NextConversation, window, cx| {
                this.step_conversation(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousConversation, window, cx| {
                this.step_conversation(-1, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &CloseThread, window, cx| this.close_thread(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &CommandPalette, window, cx| this.open_palette(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ShowUnreads, window, cx| this.show_unreads(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleUnreadsOnly, _, cx| {
                this.update_prefs(cx, |p| p.unreads_only = !p.unreads_only)
            }))
            .on_action(cx.listener(|this, _: &ToggleStar, _, cx| {
                this.toggle_pref_for_selected(cx, Prefs::toggle_star)
            }))
            .on_action(cx.listener(|this, _: &ToggleMute, _, cx| {
                this.toggle_pref_for_selected(cx, Prefs::toggle_muted)
            }))
            .on_action(cx.listener(|this, _: &ToggleHide, _, cx| {
                this.toggle_pref_for_selected(cx, Prefs::toggle_hidden)
            }))
            .on_action(cx.listener(|this, _: &CollapseAll, _, cx| this.set_all_collapsed(true, cx)))
            .on_action(cx.listener(|this, _: &ExpandAll, _, cx| this.set_all_collapsed(false, cx)))
            .on_action(
                cx.listener(|this, _: &CycleTheme, window, cx| this.cycle_appearance(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SignOut, _, cx| this.sign_out(cx)))
            .bg(theme.background)
            .text_color(theme.foreground);

        let open_profile: OpenProfile = {
            let weak = cx.entity().downgrade();
            Arc::new(move |key: String, _: &mut Window, cx: &mut App| {
                weak.update(cx, |this, cx| this.open_profile(key, cx)).ok();
            }) as OpenProfile
        };

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
            Screen::Login { .. } => root
                .child(
                    v_flex()
                        .size_full()
                        .child(TitleBar::new())
                        .child(self.render_login(cx)),
                )
                .into_any_element(),
            Screen::Main => root
                .child(
                    v_flex()
                        .size_full()
                        .child(self.render_title_bar(cx))
                        .child(
                            h_flex()
                                .flex_1()
                                .min_h_0()
                                .child(self.render_sidebar(cx))
                                .child(self.render_conversation(cx))
                                .when(self.thread_root.is_some(), |el| {
                                    el.child(self.render_thread(open_profile.clone(), cx))
                                })
                                .when(self.profile.is_some(), |el| {
                                    el.child(self.render_profile(open_profile.clone(), cx))
                                }),
                        )
                        .child(self.render_status_bar(cx)),
                )
                .into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    // Not a glob: gpui's `test` attribute macro would shadow the built-in one.
    use super::{
        Body, ConversationId, ImageFormat, ImageRef, Message, MessageId, Pending, arrived,
        first_frame_png, image_format, image_size, join_drafts, keep_older_rows,
    };

    fn at(mut m: Message, seconds_ago: i64) -> Message {
        m.sent_at = Some(chrono::Utc::now() - chrono::Duration::seconds(seconds_ago));
        m
    }

    #[test]
    fn an_older_copy_of_the_same_text_is_not_the_new_message() {
        let local = Pending {
            conversation: ConversationId("c".into()),
            reply_to: None,
            message: message("pending-2", "me", Body::Text("ok".into()), true),
            seen: ["1".to_string()].into_iter().map(MessageId).collect(),
        };
        // Seconds old, but already on screen when the second "ok" was sent.
        let earlier = at(
            message("1", "me", Body::Html("<p>ok</p>".into()), false),
            10,
        );
        assert!(!arrived(&[earlier], &local));
        let just_now = at(message("2", "me", Body::Html("<p>ok</p>".into()), false), 0);
        assert!(arrived(&[just_now], &local));
    }

    #[test]
    fn two_quick_sends_of_the_same_text_each_need_their_own_copy() {
        let first = Pending {
            conversation: ConversationId("c".into()),
            reply_to: None,
            message: message("pending-1", "me", Body::Text("ok".into()), true),
            seen: Default::default(),
        };
        let second = Pending {
            message: message("pending-2", "me", Body::Text("ok".into()), true),
            ..first.clone()
        };
        let copy = at(message("9", "me", Body::Html("<p>ok</p>".into()), false), 0);
        let mut claimed = std::collections::HashSet::new();
        let id = super::arrival(std::slice::from_ref(&copy), &first, &claimed).expect("first");
        claimed.insert(id);
        assert!(super::arrival(&[copy], &second, &claimed).is_none());
    }

    #[test]
    fn a_page_shift_keeps_rows_already_shown() {
        let old: Vec<Message> = (1..=3)
            .map(|i| {
                at(
                    message(&i.to_string(), "a", Body::Text("x".into()), false),
                    100 - i,
                )
            })
            .collect();
        let new: Vec<Message> = (2..=4)
            .map(|i| {
                at(
                    message(&i.to_string(), "a", Body::Text("x".into()), false),
                    100 - i,
                )
            })
            .collect();
        let ids: Vec<String> = keep_older_rows(&old, new)
            .into_iter()
            .map(|m| m.id.0)
            .collect();
        assert_eq!(ids, ["1", "2", "3", "4"]);
    }

    #[test]
    fn failed_text_joins_what_was_typed_since() {
        assert_eq!(join_drafts("failed", ""), "failed");
        assert_eq!(join_drafts("failed", "new"), "new\n\nfailed");
        assert_eq!(join_drafts("", "new"), "new");
    }

    fn message(id: &str, author: &str, body: Body, pending: bool) -> Message {
        Message {
            id: MessageId(id.into()),
            author: author.into(),
            author_id: Some(author.into()),
            sent_at: Some(chrono::Utc::now()),
            body,
            edited: false,
            reactions: Vec::new(),
            images: Vec::new(),
            replies: Vec::new(),
            pending,
            mentions_me: false,
        }
    }

    #[test]
    fn only_web_mail_and_teams_links_open() {
        assert!(super::is_safe_link("https://example.com"));
        assert!(super::is_safe_link("HTTPS://example.com"));
        assert!(super::is_safe_link("mailto:a@b.c"));
        for bad in [
            "file:///Applications/Calculator.app",
            "smb://host/share",
            "x-apple.systempreferences:",
            "javascript:alert(1)",
            "",
        ] {
            assert!(!super::is_safe_link(bad), "{bad}");
        }
    }

    #[test]
    fn sent_message_is_matched_to_the_services_copy() {
        let local = Pending {
            conversation: ConversationId("c".into()),
            reply_to: Some(MessageId("root".into())),
            message: message("pending-1", "me", Body::Text("a & b\nnext".into()), true),
            seen: Default::default(),
        };
        let mut root = message("root", "other", Body::Text("post".into()), false);
        assert!(!arrived(std::slice::from_ref(&root), &local));
        root.replies.push(message(
            "42",
            "me",
            Body::Html("<p>a &amp; b<br>next</p>".into()),
            false,
        ));
        assert!(arrived(&[root.clone()], &local));
        root.replies[0].author_id = Some("someone else".into());
        assert!(!arrived(&[root], &local));
    }

    fn gif(frames: usize) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
            for i in 0..frames {
                let pixel = image::Rgba([(i * 80) as u8, 0, 0, 255]);
                let frame = image::Frame::new(image::RgbaImage::from_pixel(4, 3, pixel));
                encoder.encode_frame(frame).expect("encodes");
            }
        }
        out
    }

    #[test]
    fn animated_gifs_get_a_still_and_single_frames_do_not() {
        assert!(first_frame_png(&gif(3)).is_some());
        assert!(first_frame_png(&gif(1)).is_none());
        assert_eq!(image_format(&gif(1)), Some(ImageFormat::Gif));
    }

    #[test]
    fn real_size_beats_reported_size_and_keeps_aspect() {
        let reported = ImageRef {
            url: String::new(),
            width: Some(717.),
            height: Some(146.),
        };
        assert_eq!(image_size(&reported, Some((200, 100))), (200., 100.));
        let (w, h) = image_size(&reported, None);
        assert_eq!(w, 360.);
        assert!((h - 146. * 360. / 717.).abs() < 0.01);
        let (w, h) = image_size(&reported, Some((1000, 1000)));
        assert_eq!((w, h), (300., 300.));
    }
}
