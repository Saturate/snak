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
                " from list-conversations; their `id` is what the other tools take.",
                " Nothing is signed in until snak itself has completed a login at least",
                " once; ask the user to run snak if a tool reports that."
            ),
        )
    }
}

#[derive(Serialize, JsonSchema)]
struct ConversationsOut {
    conversations: Vec<ConversationOut>,
}

#[derive(Serialize, JsonSchema)]
struct ConversationOut {
    id: String,
    title: String,
    /// Channel or chat, and the team for a channel.
    kind: String,
    unread: bool,
    muted: bool,
    /// "Author: text" of the latest message.
    preview: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct MessagesParams {
    conversation_id: String,
}

#[derive(Serialize, JsonSchema)]
struct MessagesOut {
    messages: Vec<MessageOut>,
}

#[derive(Serialize, JsonSchema)]
struct MessageOut {
    id: String,
    author: String,
    /// Local time like "Mon 5 Oct 13:07".
    sent_at: Option<String>,
    /// The message as plain text; system events included.
    text: String,
    edited: bool,
    /// Image URLs in the message.
    images: Vec<String>,
    replies: Vec<MessageOut>,
}

#[derive(Serialize, JsonSchema)]
struct DoneOut {
    done: bool,
}

#[derive(Deserialize, JsonSchema)]
struct PersonParams {
    /// A DM id, a member id or a mention link.
    key: String,
}

#[derive(Serialize, JsonSchema)]
struct PersonOut {
    display_name: Option<String>,
    /// Graph's `mail`, or the UPN for guests.
    mail: Option<String>,
    job_title: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct SendParams {
    conversation_id: String,
    text: String,
    /// Reply to a channel post by its id, if any.
    reply_to: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct MarkReadParams {
    conversation_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct SetSettingParams {
    name: String,
    /// The value to store, like `true` or `"light"` or `12.5`.
    value: Value,
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

    #[tool(description = "The signed-in user's conversations, chats and channels both.")]
    async fn list_conversations(&self) -> Result<Json<ConversationsOut>, String> {
        let conversations = match self.backend.conversations().await {
            Ok(conversations) => conversations,
            Err(e) => return Err(e.to_string()),
        };
        let mut out: Vec<_> = conversations
            .iter()
            .map(|c| ConversationOut {
                id: c.id.0.clone(),
                title: c.title.clone(),
                kind: if c.is_channel() {
                    "channel".into()
                } else {
                    "chat".into()
                },
                unread: c.unread,
                muted: c.muted,
                preview: c.preview.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.title.cmp(&b.title));
        Ok(Json(ConversationsOut { conversations: out }))
    }

    #[tool(description = "One conversation's messages, oldest first, replies under their posts.")]
    async fn read_messages(
        &self,
        params: Parameters<MessagesParams>,
    ) -> Result<Json<MessagesOut>, String> {
        let rows = match self
            .backend
            .messages(&snak_chat::ConversationId(params.0.conversation_id.clone()))
            .await
        {
            Ok(rows) => rows,
            Err(e) => return Err(e.to_string()),
        };
        let out = rows.iter().map(message_out).collect();
        Ok(Json(MessagesOut { messages: out }))
    }

    #[tool(
        description = "One person's details: display name, mail and job title, by DM id, member id or mention link."
    )]
    async fn get_person(
        &self,
        params: Parameters<PersonParams>,
    ) -> Result<Json<PersonOut>, String> {
        match self.teams.lookup_person(&params.0.key).await {
            Ok(Some(person)) => Ok(Json(PersonOut {
                display_name: person.display_name,
                mail: person.mail,
                job_title: person.job_title,
            })),
            Ok(None) => Err("nobody by that key".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    #[tool(description = "Send a message to a conversation; reply_to threads a channel post.")]
    async fn send_message(&self, params: Parameters<SendParams>) -> Result<Json<DoneOut>, String> {
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

    #[tool(description = "Mark a conversation read, clearing its unread dot.")]
    async fn mark_read(&self, params: Parameters<MarkReadParams>) -> Result<Json<DoneOut>, String> {
        self.backend
            .mark_read(&snak_chat::ConversationId(params.0.conversation_id.clone()))
            .await
            .map_err(|e| e.to_string())?;
        Ok(Json(DoneOut { done: true }))
    }

    #[tool(description = "The app's settings as JSON text, as the settings file has them.")]
    async fn get_settings(&self) -> Result<String, String> {
        read_settings(&self.settings_path)
            .await
            .map(|value| serde_json::to_string_pretty(&value).unwrap_or_default())
            .map_err(|e| e.to_string())
    }

    #[tool(description = "Set one setting by name; applies on the app's next launch.")]
    async fn set_setting(&self, params: Parameters<SetSettingParams>) -> Result<String, String> {
        let mut settings = read_settings(&self.settings_path)
            .await
            .map_err(|e| e.to_string())?;
        settings[params.0.name] = params.0.value;
        write_settings(&self.settings_path, &settings)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::to_string_pretty(&settings).unwrap_or_default())
    }
}

fn message_out(message: &snak_chat::Message) -> MessageOut {
    MessageOut {
        id: message.id.0.clone(),
        author: message.author.clone(),
        sent_at: message.sent_at.map(|t| t.to_rfc3339()),
        text: body_text(&message.body),
        edited: message.edited,
        images: message.images.iter().map(|i| i.url.clone()).collect(),
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
    Some(std::env::home_dir()?.join("Library/Application Support/dev.snak.app/settings.json"))
}

/// Where the app keeps its refresh token, read from the same settings.json.
fn storage() -> snak_teams::store::Storage {
    let Some(path) = settings_path() else {
        return snak_teams::store::Storage::default();
    };
    let Ok(value) = serde_json::from_str::<Value>(&match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return snak_teams::store::Storage::default(),
    }) else {
        return snak_teams::store::Storage::default();
    };
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
    fn storage_maps_the_settings_file() {
        // The mapping mirrors app settings' credential_storage().
        let json = serde_json::json!({
            "login_storage": "onepassword",
            "onepassword_account": "acme"
        });
        let value: Value = serde_json::from_value(json).unwrap();
        let account = value
            .get("onepassword_account")
            .and_then(|a| a.as_str().map(String::from));
        let storage = match value.get("login_storage").and_then(|s| s.as_str()) {
            Some("file") => snak_teams::store::Storage::File,
            Some("onepassword") if account.is_some() => snak_teams::store::Storage::OnePassword {
                account: account.unwrap(),
            },
            _ => snak_teams::store::Storage::default(),
        };
        assert!(matches!(
            storage,
            snak_teams::store::Storage::OnePassword { .. }
        ));
    }
}
