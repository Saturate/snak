//! `snak --stdio`: an MCP server that drives the app's own Teams backend,
//! so an assistant can read and write chat without Microsoft's global
//! MCP server, and can inspect and edit the app's settings.

use rmcp::{
    Json, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;

/// Reads and writes chat through the Teams backend the app itself uses.
struct Mcp {
    backend: Arc<dyn snak_chat::ChatBackend>,
    /// The concrete Teams backend, for person lookups the trait does not know.
    teams: Arc<snak_teams::TeamsBackend>,
    settings_path: std::path::PathBuf,
}

#[tool_handler]
impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            concat!(
                "Chat through Microsoft Teams as the signed-in user. Conversations come",
                " from list_conversations; their `id` is what the other tools take. To",
                " message someone, find them with search_people, then find_dm gives",
                " the conversation to send to, or start_dm when there is none yet.",
                " send_message posts as the user right",
                " away, so confirm the text and recipient first. Nothing is signed in",
                " until snak itself has completed a login at least once; ask the user",
                " to run snak if a tool reports that."
            ),
        )
    }
}

// Optional fields are left out of the output rather than sent as null, and
// their schema names one type: `["string", "null"]` is valid JSON Schema but
// some clients (Gemini's function declarations) reject a type array.

#[derive(Deserialize, JsonSchema)]
struct ConversationsParams {
    /// Only conversations with unread messages.
    #[serde(default)]
    unread_only: bool,
    /// How many to return, most recently active first.
    #[serde(default = "default_conversation_limit")]
    #[schemars(range(min = 1, max = 500))]
    limit: usize,
}

fn default_conversation_limit() -> usize {
    50
}

#[derive(Serialize, JsonSchema)]
struct ConversationsOut {
    conversations: Vec<ConversationOut>,
}

#[derive(Serialize, JsonSchema)]
struct ConversationOut {
    id: String,
    title: String,
    /// "dm", "group" or "channel".
    kind: String,
    /// The team a channel belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    team: Option<String>,
    unread: bool,
    muted: bool,
    /// RFC 3339 UTC time of the latest message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    last_activity: Option<String>,
    /// "Author: text" of the latest message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    preview: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct MessagesParams {
    conversation_id: String,
    /// How many of the newest messages (channel posts, with their replies) to return.
    #[serde(default = "default_message_limit")]
    #[schemars(range(min = 1, max = 200))]
    limit: usize,
}

fn default_message_limit() -> usize {
    30
}

#[derive(Serialize, JsonSchema)]
struct MessagesOut {
    messages: Vec<MessageOut>,
}

#[derive(Serialize, JsonSchema)]
struct MessageOut {
    id: String,
    author: String,
    /// The author's object id, for get_person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    author_id: Option<String>,
    /// RFC 3339 UTC time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    sent_at: Option<String>,
    /// The message as plain text; system events included.
    text: String,
    edited: bool,
    /// Images attached; their URLs need the user's Teams token, so only the count.
    image_count: usize,
    replies: Vec<MessageOut>,
}

#[derive(Serialize, JsonSchema)]
struct DoneOut {
    done: bool,
}

#[derive(Deserialize, JsonSchema)]
struct PersonParams {
    /// An object id (from search_people or a message's author_id), a DM id or a mention link.
    key: String,
}

#[derive(Serialize, JsonSchema)]
struct PersonOut {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    display_name: Option<String>,
    /// Graph's `mail`, or the UPN for guests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    mail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    job_title: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct SearchPeopleParams {
    /// Start of a name or mail address, like "anders" or "anders.h@".
    query: String,
    #[serde(default = "default_people_limit")]
    #[schemars(range(min = 1, max = 50))]
    limit: usize,
}

fn default_people_limit() -> usize {
    10
}

#[derive(Serialize, JsonSchema)]
struct PeopleOut {
    people: Vec<PersonHitOut>,
}

#[derive(Serialize, JsonSchema)]
struct PersonHitOut {
    /// Object id, for find_dm, start_dm and get_person.
    id: String,
    #[serde(flatten)]
    person: PersonOut,
}

#[derive(Deserialize, JsonSchema)]
struct FindDmParams {
    /// The person's object id, from search_people or a message's author_id.
    person_id: String,
}

#[derive(Serialize, JsonSchema)]
struct DmOut {
    conversation_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct SendParams {
    conversation_id: String,
    /// Plain text; Markdown is sent as-is, not rendered.
    text: String,
    /// Reply to a channel post by its id.
    #[serde(default)]
    #[schemars(with = "String")]
    reply_to: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct MarkReadParams {
    conversation_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct SetSettingParams {
    name: String,
    /// The value to store, like `true` or `"Dark"` or `16`.
    #[schemars(schema_with = "setting_value_schema")]
    value: Value,
}

fn setting_value_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [
            { "type": "boolean" },
            { "type": "number" },
            { "type": "string" },
            { "type": "null" }
        ]
    })
}

#[derive(Clone, Copy)]
enum SettingKind {
    Bool,
    Number,
    /// A theme name, or null to follow Zed's choice.
    ThemeName,
    Appearance,
}

/// Settings an assistant may change. Sign-in storage is left out: moving the
/// refresh token is the user's decision. A value of the wrong type would make
/// the app set the whole file aside on its next launch, so types are checked.
const EDITABLE_SETTINGS: &[(&str, SettingKind)] = &[
    ("appearance", SettingKind::Appearance),
    ("dark_theme", SettingKind::ThemeName),
    ("light_theme", SettingKind::ThemeName),
    ("ui_font_size", SettingKind::Number),
    ("dm_limit", SettingKind::Number),
    ("show_team_names", SettingKind::Bool),
    ("unread_dot", SettingKind::Bool),
    ("autoplay_gifs", SettingKind::Bool),
];

fn check_setting(name: &str, value: &Value) -> Result<(), String> {
    let Some((_, kind)) = EDITABLE_SETTINGS.iter().find(|(n, _)| *n == name) else {
        let names: Vec<&str> = EDITABLE_SETTINGS.iter().map(|(n, _)| *n).collect();
        return Err(format!(
            "{name} can't be set here; settable: {}",
            names.join(", ")
        ));
    };
    let (ok, expected) = match kind {
        SettingKind::Bool => (value.is_boolean(), "true or false"),
        SettingKind::Number => (value.is_number(), "a number"),
        SettingKind::ThemeName => (value.is_string() || value.is_null(), "a theme name or null"),
        SettingKind::Appearance => (
            matches!(value.as_str(), Some("System" | "Light" | "Dark")),
            "\"System\", \"Light\" or \"Dark\"",
        ),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{name} takes {expected}"))
    }
}

#[tool_router]
impl Mcp {
    fn new(
        backend: Arc<dyn snak_chat::ChatBackend>,
        teams: Arc<snak_teams::TeamsBackend>,
        settings_path: std::path::PathBuf,
    ) -> Self {
        Self {
            backend,
            teams,
            settings_path,
        }
    }

    /// The object id behind whatever key a tool got, the same way get_person
    /// reads it, so an MRI or a mention link works everywhere.
    fn person_id(&self, key: &str) -> Result<String, String> {
        self.teams
            .person_id(key)
            .ok_or_else(|| "not a person's object id, MRI, DM id or mention link".into())
    }

    #[tool(
        description = "The signed-in user's chats and channels, most recently active first.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_conversations(
        &self,
        params: Parameters<ConversationsParams>,
    ) -> Result<Json<ConversationsOut>, String> {
        let conversations = match self.backend.conversations().await {
            Ok(conversations) => conversations,
            Err(e) => return Err(e.to_string()),
        };
        let mut conversations: Vec<_> = conversations
            .into_iter()
            .filter(|c| c.unread || !params.0.unread_only)
            .collect();
        // None sorts last: a conversation with no messages is the least recent.
        conversations.sort_by_key(|c| std::cmp::Reverse(c.last_activity));
        let out = conversations
            .iter()
            .take(params.0.limit.clamp(1, 500))
            .map(conversation_out)
            .collect();
        Ok(Json(ConversationsOut { conversations: out }))
    }

    #[tool(
        description = "One conversation's newest messages, oldest first, replies under their posts.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn read_messages(
        &self,
        params: Parameters<MessagesParams>,
    ) -> Result<Json<MessagesOut>, String> {
        let limit = params.0.limit.clamp(1, 200);
        let id = snak_chat::ConversationId(params.0.conversation_id.clone());
        let page = self
            .backend
            .messages(&id)
            .await
            .map_err(|e| e.to_string())?;
        let mut rows = page.messages;
        let mut older = page.older;
        // Channel pages are short; follow a few back to reach the limit, but
        // never walk a whole history for one call.
        for _ in 0..4 {
            let Some(cursor) = older.take().filter(|_| rows.len() < limit) else {
                break;
            };
            let page = self
                .backend
                .older_messages(&id, &cursor)
                .await
                .map_err(|e| e.to_string())?;
            let seen: std::collections::HashSet<&str> =
                rows.iter().map(|m| m.id.0.as_str()).collect();
            let earlier: Vec<_> = page
                .messages
                .into_iter()
                .filter(|m| !seen.contains(m.id.0.as_str()))
                .collect();
            if earlier.is_empty() {
                break;
            }
            rows.splice(0..0, earlier);
            older = page.older;
        }
        let skip = rows.len().saturating_sub(limit);
        let out = rows[skip..].iter().map(message_out).collect();
        Ok(Json(MessagesOut { messages: out }))
    }

    #[tool(
        description = "One person's display name, mail and job title, by object id, DM id or mention link.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn get_person(
        &self,
        params: Parameters<PersonParams>,
    ) -> Result<Json<PersonOut>, String> {
        match self.teams.lookup_person(&params.0.key).await {
            Ok(Some(person)) => Ok(Json(person_out(person))),
            Ok(None) => Err("nobody by that key".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    #[tool(
        description = "People in the organisation's directory whose name or mail starts with the query.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn search_people(
        &self,
        params: Parameters<SearchPeopleParams>,
    ) -> Result<Json<PeopleOut>, String> {
        let query = params.0.query.trim();
        // The client drops quotes and backslashes, which would leave nothing.
        if !query.chars().any(|c| !matches!(c, '"' | '\\')) {
            return Err("query is empty".into());
        }
        let hits = self
            .teams
            .search_people(query, params.0.limit.clamp(1, 50))
            .await
            .map_err(|e| e.to_string())?;
        let people = hits
            .into_iter()
            .map(|(id, person)| PersonHitOut {
                id,
                person: person_out(person),
            })
            .collect();
        Ok(Json(PeopleOut { people }))
    }

    #[tool(
        description = "The existing one-on-one chat with a person, by their object id. Fails if there is none yet; start_dm creates one.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn find_dm(&self, params: Parameters<FindDmParams>) -> Result<Json<DmOut>, String> {
        let person = self.person_id(&params.0.person_id)?;
        match self.teams.direct_chat_with(&person).await {
            Ok(Some(id)) => Ok(Json(DmOut {
                conversation_id: id.0,
            })),
            Ok(None) => Err("no one-on-one chat with that person yet".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    #[tool(
        description = "The one-on-one chat with a person by their object id, created if there is none yet. Creating it sends nothing; the person sees it once a message arrives.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn start_dm(&self, params: Parameters<FindDmParams>) -> Result<Json<DmOut>, String> {
        let person = self.person_id(&params.0.person_id)?;
        if person.eq_ignore_ascii_case(&self.backend.me().id) {
            return Err("that's the signed-in user".into());
        }
        match self.teams.open_direct_chat(&person).await {
            Ok(id) => Ok(Json(DmOut {
                conversation_id: id.0,
            })),
            Err(e) => Err(e.to_string()),
        }
    }

    #[tool(
        description = "Send a plain-text message as the signed-in user, posted immediately. reply_to threads a channel post.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    async fn send_message(&self, params: Parameters<SendParams>) -> Result<Json<DoneOut>, String> {
        if params.0.text.trim().is_empty() {
            return Err("text is empty".into());
        }
        let reply_to = params.0.reply_to.clone().map(snak_chat::MessageId);
        self.backend
            .send(
                &snak_chat::ConversationId(params.0.conversation_id.clone()),
                reply_to.as_ref(),
                &params.0.text,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(Json(DoneOut { done: true }))
    }

    #[tool(
        description = "Mark a conversation read, clearing its unread dot.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn mark_read(&self, params: Parameters<MarkReadParams>) -> Result<Json<DoneOut>, String> {
        self.backend
            .mark_read(&snak_chat::ConversationId(params.0.conversation_id.clone()))
            .await
            .map_err(|e| e.to_string())?;
        Ok(Json(DoneOut { done: true }))
    }

    #[tool(
        description = "The app's settings as JSON text, as the settings file has them.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_settings(&self) -> Result<String, String> {
        read_settings(&self.settings_path)
            .await
            .map(|value| serde_json::to_string_pretty(&value).unwrap_or_default())
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Set one appearance or display setting by name, for the app's next launch. Use it while snak is closed: a running snak rewrites the file from memory on its next settings change. Sign-in settings can't be changed here.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_setting(&self, params: Parameters<SetSettingParams>) -> Result<String, String> {
        check_setting(&params.0.name, &params.0.value)?;
        let mut settings = match read_settings(&self.settings_path).await {
            Ok(settings) => settings,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Value::Object(serde_json::Map::default())
            }
            Err(e) => return Err(e.to_string()),
        };
        let Some(map) = settings.as_object_mut() else {
            return Err("the settings file isn't a JSON object".into());
        };
        map.insert(params.0.name, params.0.value);
        write_settings(&self.settings_path, &settings)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string_pretty(&settings).unwrap_or_default())
    }
}

fn conversation_out(c: &snak_chat::Conversation) -> ConversationOut {
    use snak_chat::ConversationKind;
    let kind = match c.kind {
        ConversationKind::Channel { .. } => "channel",
        ConversationKind::Direct => "dm",
        ConversationKind::Group => "group",
    };
    ConversationOut {
        id: c.id.0.clone(),
        title: c.title.clone(),
        kind: kind.into(),
        team: c.group().map(|g| g.name.clone()),
        unread: c.unread,
        muted: c.muted,
        last_activity: c.last_activity.map(|t| t.to_rfc3339()),
        preview: c.preview.clone(),
    }
}

fn person_out(person: snak_teams::models::Person) -> PersonOut {
    PersonOut {
        display_name: person.display_name,
        mail: person.mail,
        job_title: person.job_title,
    }
}

fn message_out(message: &snak_chat::Message) -> MessageOut {
    MessageOut {
        id: message.id.0.clone(),
        author: message.author.clone(),
        author_id: message.author_id.clone(),
        sent_at: message.sent_at.map(|t| t.to_rfc3339()),
        text: body_text(&message.body),
        edited: message.edited,
        image_count: message.images.len(),
        replies: message.replies.iter().map(message_out).collect(),
    }
}

fn body_text(body: &snak_chat::Body) -> String {
    use snak_chat::Body;
    match body {
        Body::Html(html) => snak_teams::html::to_text(html),
        Body::Text(text) => text.clone(),
        Body::Deleted => "[deleted]".into(),
        Body::Event(event) => event.clone(),
    }
}

fn read_settings(
    path: &std::path::Path,
) -> Pin<Box<dyn Future<Output = std::io::Result<Value>> + Send>> {
    let path = path.to_owned();
    Box::pin(async move {
        match tokio::task::spawn_blocking(move || std::fs::read(&path)).await {
            Ok(Ok(bytes)) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(std::io::Error::other(e)),
        }
    })
}

fn write_settings(
    path: &std::path::Path,
    value: &Value,
) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>> {
    let Ok(bytes) = serde_json::to_vec_pretty(value) else {
        return Box::pin(async { Err(std::io::Error::other("settings serialization failed")) });
    };
    let path = path.to_owned();
    let tmp = path.with_extension("json~");
    Box::pin(async move {
        match tokio::task::spawn_blocking(move || {
            if let Some(dir) = tmp.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, path)
        })
        .await
        {
            Ok(result) => result,
            Err(e) => Err(std::io::Error::other(e)),
        }
    })
}

fn settings_path() -> Option<std::path::PathBuf> {
    // home_dir has not been deprecated since Rust 1.86; the MSRV is newer.
    Some(
        std::env::home_dir()? // skipcq: RS-W1128
            .join("Library/Application Support/dev.snak.app/settings.json"),
    )
}

/// Where the app keeps its refresh token, read from the same settings.json.
fn storage() -> snak_teams::store::Storage {
    let value = settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_default();
    storage_from(&value)
}

/// The mapping in app settings' `credential_storage()`, including the
/// `insecure_credentials` key `login_storage` replaced.
fn storage_from(value: &Value) -> snak_teams::store::Storage {
    if value.get("insecure_credentials").and_then(Value::as_bool) == Some(true) {
        return snak_teams::store::Storage::File;
    }
    let storage = value.get("login_storage").and_then(|s| s.as_str());
    let account = value
        .get("onepassword_account")
        .and_then(|a| a.as_str().map(String::from));
    match (storage, account) {
        (Some("file") | Some("File"), _) => snak_teams::store::Storage::File,
        (Some("onepassword") | Some("OnePassword"), Some(account)) => {
            snak_teams::store::Storage::OnePassword { account }
        }
        _ => snak_teams::store::Storage::default(),
    }
}

/// Connects to Teams with the stored session and serves MCP over stdio.
/// Returns when the client closes the connection or stdio ends.
pub fn run() -> snak_chat::Result<()> {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => return Err(snak_chat::ChatError::Other(e.to_string())),
    };
    runtime.block_on(run_async())
}

async fn run_async() -> snak_chat::Result<()> {
    // The token only gets read here, never moved, so no exclusive lock is needed.
    snak_teams::store::use_storage_without_moving(storage());
    let login = match snak_teams::store::load().await {
        Ok(Some(login)) => login,
        Ok(None) => {
            eprintln!("snak: no stored Teams session; run snak and sign in first");
            return Ok(());
        }
        Err(e) => return Err(snak_chat::ChatError::Other(e.to_string())),
    };
    let http = reqwest::Client::new();
    let client = snak_teams::TeamsClient::connect(http, login).await?;
    let teams = Arc::new(snak_teams::TeamsBackend::new(client));
    // Directory addresses only fill in as conversations() walks the chats.
    let _ = snak_chat::ChatBackend::conversations(teams.as_ref()).await;
    let backend: Arc<dyn snak_chat::ChatBackend> = teams.clone();
    let settings_path = settings_path().unwrap_or_else(|| "settings.json".into());
    let server = Mcp::new(backend, teams, settings_path);
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| snak_chat::ChatError::Other(e.to_string()))?;
    running
        .waiting()
        .await
        .map_err(|e| snak_chat::ChatError::Other(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_text_flattens_every_kind() {
        use snak_chat::Body;
        assert_eq!(body_text(&Body::Text("hi".into())), "hi");
        assert_eq!(body_text(&Body::Deleted), "[deleted]");
        assert_eq!(
            body_text(&Body::Event("left the team".into())),
            "left the team"
        );
        assert_eq!(body_text(&Body::Html("<p>a<br/>b</p>".into())), "a\nb");
    }

    #[test]
    fn sign_in_settings_are_not_settable() {
        assert!(check_setting("login_storage", &serde_json::json!("File")).is_err());
        assert!(check_setting("onepassword_account", &serde_json::json!("acme")).is_err());
    }

    #[test]
    fn settings_must_have_the_type_the_app_reads() {
        use serde_json::json;
        assert!(check_setting("unread_dot", &json!(false)).is_ok());
        assert!(check_setting("unread_dot", &json!("false")).is_err());
        assert!(check_setting("ui_font_size", &json!(16)).is_ok());
        assert!(check_setting("dark_theme", &Value::Null).is_ok());
        assert!(check_setting("appearance", &json!("Dark")).is_ok());
        assert!(check_setting("appearance", &json!("dark")).is_err());
    }

    #[test]
    fn storage_maps_the_settings_file() {
        use serde_json::json;
        use snak_teams::store::Storage;
        let one = json!({ "login_storage": "OnePassword", "onepassword_account": "acme" });
        assert!(
            matches!(storage_from(&one), Storage::OnePassword { account } if account == "acme")
        );
        assert!(matches!(
            storage_from(&json!({ "login_storage": "File" })),
            Storage::File
        ));
        assert!(matches!(
            storage_from(&json!({ "insecure_credentials": true })),
            Storage::File
        ));
        // 1Password without an account falls back like the app does.
        let bare = storage_from(&json!({ "login_storage": "OnePassword" }));
        assert!(matches!(bare, Storage::Keychain));
    }
}
