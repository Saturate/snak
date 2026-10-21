// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Endpoints derived from Squads (https://github.com/IanTerzo/Squads, src/api.rs, MPL-2.0).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::auth::{self, AccessToken, scope};
use crate::error::{CoreError, Result};
use crate::models::{ChannelPage, Me, MessagePage, Person, ReplyChain, UserDetails};
use crate::store::StoredLogin;

const TEAMS: &str = "https://teams.microsoft.com";
const GRAPH_BATCH_LIMIT: usize = 20;
// Skype tokens share the access token cache; this key can never collide with a scope URL.
const SKYPE_TOKEN_KEY: &str = "skypetoken";

pub struct TeamsClient {
    http: reqwest::Client,
    refresh_token: Mutex<String>,
    tokens: Mutex<HashMap<&'static str, AccessToken>>,
    region: String,
    me: Me,
    /// Sign-out epoch this client started under; see `store::epoch`.
    store_epoch: u64,
}

impl TeamsClient {
    /// Builds a client from a stored refresh token: finds the tenant's region and the signed-in user.
    pub async fn connect(http: reqwest::Client, login: StoredLogin) -> Result<Self> {
        let mut client = Self {
            http,
            refresh_token: Mutex::new(login.token),
            tokens: Mutex::new(HashMap::new()),
            region: String::new(),
            store_epoch: login.epoch,
            me: Me {
                id: String::new(),
                display_name: None,
                mail: None,
                user_principal_name: None,
            },
        };
        client.region = client.discover_region().await?;
        client.me = client
            .get_json(scope::GRAPH, "https://graph.microsoft.com/v1.0/me")
            .await?;
        Ok(client)
    }

    pub fn me(&self) -> &Me {
        &self.me
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub(crate) async fn token(&self, scope: &'static str) -> Result<String> {
        if let Some(token) = self.tokens.lock().await.get(scope)
            && token.expires_at > Instant::now()
        {
            return Ok(token.value.clone());
        }

        let mut refresh = self.refresh_token.lock().await;
        // Someone may have refreshed this scope while we waited for the lock.
        if let Some(token) = self.tokens.lock().await.get(scope)
            && token.expires_at > Instant::now()
        {
            return Ok(token.value.clone());
        }
        let (token, rotated) = auth::access_token(&self.http, &refresh, scope)
            .await
            .inspect_err(|e| tracing::warn!(scope, error = %e, "token refresh failed"))?;
        tracing::debug!(scope, "access token refreshed");
        if let Some(rotated) = rotated {
            // The new access token is good either way; a failed save (say, a
            // denied Keychain prompt) only means the next launch uses an older,
            // still valid refresh token.
            if let Err(e) = crate::store::save(rotated.clone(), false, self.store_epoch).await {
                tracing::warn!(error = %e, "could not save the rotated refresh token");
            }
            *refresh = rotated;
        }
        let value = token.value.clone();
        self.tokens.lock().await.insert(scope, token);
        Ok(value)
    }

    /// Skype token for services that predate Entra auth (Trouter, the registrar).
    /// Derived from Squads (src/api.rs, `gen_skype_token`).
    pub(crate) async fn skype_token(&self) -> Result<String> {
        if let Some(token) = self.tokens.lock().await.get(SKYPE_TOKEN_KEY)
            && token.expires_at > Instant::now()
        {
            return Ok(token.value.clone());
        }
        let (body, url) = self.authz().await?;
        self.cache_skype_token(&body)
            .await
            .ok_or(CoreError::MissingField {
                url,
                field: "tokens.skypeToken",
            })
    }

    /// Drops a cached Skype token the server has rejected before its expiry.
    pub(crate) async fn forget_skype_token(&self) {
        self.tokens.lock().await.remove(SKYPE_TOKEN_KEY);
    }

    /// Drops the Skype token only if it is still the one that was rejected,
    /// so parallel 401s don't each throw away a token another just fetched.
    async fn forget_skype_token_if(&self, rejected: &str) {
        let mut tokens = self.tokens.lock().await;
        if tokens
            .get(SKYPE_TOKEN_KEY)
            .is_some_and(|t| t.value == rejected)
        {
            tokens.remove(SKYPE_TOKEN_KEY);
        }
    }

    // The same authz response carries both the region and a Skype token, so
    // one call at connect time fills both.
    async fn authz(&self) -> Result<(Value, String)> {
        let url = format!("{TEAMS}/api/authsvc/v1.0/authz");
        let body = self
            .call(scope::SKYPE, &url, |http, token| {
                http.post(&url)
                    .bearer_auth(token)
                    .header("Content-Length", "0")
            })
            .await?;
        Ok((body, url))
    }

    async fn cache_skype_token(&self, authz: &Value) -> Option<String> {
        let tokens = authz.get("tokens")?;
        let value = tokens.get("skypeToken")?.as_str()?.to_string();
        let lifetime = auth::expires_in_secs(tokens.get("expiresIn")).saturating_sub(60);
        self.tokens.lock().await.insert(
            SKYPE_TOKEN_KEY,
            AccessToken {
                value: value.clone(),
                expires_at: Instant::now() + Duration::from_secs(lifetime),
            },
        );
        Some(value)
    }

    // Squads hardcodes `emea`. The authz response says which regional chat
    // service this tenant lives on, e.g. https://emea.ng.msg.teams.microsoft.com.
    async fn discover_region(&self) -> Result<String> {
        let (body, _) = self.authz().await?;
        self.cache_skype_token(&body).await;

        if let Some(region) = body.get("region").and_then(Value::as_str) {
            return Ok(region.to_string());
        }
        let from_chat_service = body
            .pointer("/regionGtms/chatService")
            .and_then(Value::as_str)
            .and_then(|url| url.strip_prefix("https://"))
            .and_then(|host| host.split('.').next())
            .map(str::to_string);
        Ok(from_chat_service.unwrap_or_else(|| "emea".to_string()))
    }

    async fn get_json<T: DeserializeOwned>(&self, scope: &'static str, url: &str) -> Result<T> {
        self.call(scope, url, |http, token| http.get(url).bearer_auth(token))
            .await
    }

    /// Sends with a token for `scope`. A 401 means the token was revoked or
    /// its claims changed (we advertise CP1, so Entra may issue long-lived
    /// tokens and revoke them mid-life): drop it and retry once with a new one.
    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        scope: &'static str,
        url: &str,
        build: impl Fn(&reqwest::Client, String) -> reqwest::RequestBuilder,
    ) -> Result<T> {
        let res = self.call_response(scope, build).await?;
        decode(res, url).await
    }

    /// [`Self::call`] without decoding, for callers that need the headers.
    async fn call_response(
        &self,
        scope: &'static str,
        build: impl Fn(&reqwest::Client, String) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let token = self.token(scope).await?;
        let res = send_throttled(|| build(&self.http, token.clone())).await?;
        if res.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(res);
        }
        tracing::info!(scope, "access token rejected; getting a new one");
        {
            // Only drop the token that was rejected: if a parallel request
            // already replaced it, removing that one would refresh again.
            let mut tokens = self.tokens.lock().await;
            if tokens.get(scope).is_some_and(|t| t.value == token) {
                tokens.remove(scope);
            }
        }
        let token = self.token(scope).await?;
        send_throttled(|| build(&self.http, token.clone())).await
    }

    /// All joined teams with their channels, plus all chats.
    pub async fn teams_and_chats(&self) -> Result<UserDetails> {
        let url = format!(
            "{TEAMS}/api/csa/{}/api/v2/teams/users/me?isPrefetch=false&enableMembershipSummary=true&enableRC2Fetch=false",
            self.region
        );
        self.get_json(scope::CHAT_AGGREGATOR, &url).await
    }

    /// Raw JSON for diagnosing schema drift in the undocumented endpoints.
    #[doc(hidden)]
    pub async fn raw_teams_and_chats(&self) -> Result<serde_json::Value> {
        let url = format!(
            "{TEAMS}/api/csa/{}/api/v2/teams/users/me?isPrefetch=false&enableMembershipSummary=true&enableRC2Fetch=false",
            self.region
        );
        self.get_json(scope::CHAT_AGGREGATOR, &url).await
    }

    #[doc(hidden)]
    pub async fn raw_channel_posts(
        &self,
        team_id: &str,
        channel_id: &str,
    ) -> Result<serde_json::Value> {
        let url = format!(
            "{TEAMS}/api/csa/{}/api/v2/teams/{}/channels/{}",
            self.region,
            urlencode(team_id),
            urlencode(channel_id)
        );
        self.get_json(scope::CHAT_AGGREGATOR, &url).await
    }

    /// Newest-first page of a chat's messages.
    pub async fn chat_messages(&self, conversation_id: &str) -> Result<MessagePage> {
        let url = format!(
            "{TEAMS}/api/chatsvc/{}/v1/users/ME/conversations/{}/messages?pageSize=200",
            self.region,
            urlencode(conversation_id)
        );
        self.get_json(scope::CHAT, &url).await
    }

    /// Recent channel posts, each with its replies.
    pub async fn channel_posts(&self, team_id: &str, channel_id: &str) -> Result<Vec<ReplyChain>> {
        let url = format!(
            "{TEAMS}/api/csa/{}/api/v2/teams/{}/channels/{}",
            self.region,
            urlencode(team_id),
            urlencode(channel_id)
        );
        let page: ChannelPage = self.get_json(scope::CHAT_AGGREGATOR, &url).await?;
        Ok(page.reply_chains)
    }

    /// Bytes of an image embedded in a message. Only Microsoft's own media
    /// hosts get the Skype token: message HTML is written by other people.
    pub async fn image(&self, url: &str) -> Result<Vec<u8>> {
        const MAX_BYTES: usize = 25 * 1024 * 1024;
        let Some((url, host)) = crate::backend::classify_parsed_url(url) else {
            return Err(CoreError::Push(
                "refusing to fetch an image from that host".into(),
            ));
        };
        let request = |token: Option<String>| match token {
            Some(token) => self
                .http
                .get(url.clone())
                .header("Authorization", format!("skype_token {token}")),
            // Public CDN: deliberately no credentials.
            None => self.http.get(url.clone()),
        };
        let token = match host {
            crate::backend::MediaHost::Teams => Some(self.skype_token().await?),
            crate::backend::MediaHost::Giphy => None,
        };
        let mut res = request(token.clone()).send().await?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED
            && let Some(rejected) = &token
        {
            self.forget_skype_token_if(rejected).await;
            res = request(Some(self.skype_token().await?)).send().await?;
        }
        let status = res.status();
        if !status.is_success() {
            return Err(CoreError::Http {
                status: status.as_u16(),
                url: url.path().to_string(),
                body: String::new(),
            });
        }
        // Capped while streaming, not after: the server picks the body size.
        if res.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
            return Err(CoreError::Push("image too large".into()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = res.chunk().await? {
            if bytes.len() + chunk.len() > MAX_BYTES {
                return Err(CoreError::Push("image too large".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    /// Creates the one-on-one chat with a user by object id and returns its
    /// conversation id. A unique-roster thread is idempotent: when the chat
    /// already exists, Teams hands back the existing one.
    pub async fn create_direct_chat(&self, person_id: &str) -> Result<String> {
        let url = format!("{TEAMS}/api/chatsvc/{}/v1/threads", self.region);
        let body = json!({
            "members": [
                { "id": format!("8:orgid:{}", self.me.id), "role": "Admin" },
                { "id": format!("8:orgid:{person_id}"), "role": "Admin" },
            ],
            "properties": {
                "threadType": "chat",
                "fixedRoster": "true",
                "uniquerosterthread": "true",
            },
        });
        let res = self
            .call_response(scope::CHAT, |http, token| {
                http.post(&url).bearer_auth(token).json(&body)
            })
            .await?;
        let location = res
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|l| l.to_str().ok())
            .and_then(|l| l.rsplit('/').next())
            .map(str::to_string);
        let _: Value = decode(res, &url).await?;
        location.ok_or(CoreError::MissingField {
            url,
            field: "Location",
        })
    }

    /// Moves the read marker to now, which clears unread in every Teams client.
    pub async fn mark_read(&self, conversation_id: &str) -> Result<()> {
        let url = format!(
            "{TEAMS}/api/chatsvc/{}/v1/users/ME/conversations/{}/properties?name=consumptionhorizon",
            self.region,
            urlencode(conversation_id)
        );
        // Squads sends the current time in all three slots (message id;
        // timestamp; client message id) and Teams accepts it.
        let now = chrono::Utc::now().timestamp_millis();
        let body = json!({ "consumptionhorizon": format!("{now};{now};{now}") });
        let _: Value = self
            .call(scope::CHAT, &url, |http, token| {
                http.put(&url).bearer_auth(token).json(&body)
            })
            .await?;
        Ok(())
    }

    /// Sends to a chat or channel. Pass `reply_to` to reply in a channel thread.
    pub async fn send_message(
        &self,
        conversation_id: &str,
        reply_to: Option<&str>,
        text: &str,
    ) -> Result<()> {
        let target = match reply_to {
            Some(root) => format!("{conversation_id};messageid={root}"),
            None => conversation_id.to_string(),
        };
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let client_message_id = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
            .to_string();
        let body = json!({
            "id": "-1",
            "type": "Message",
            "conversationid": target,
            "conversationLink": format!("blah/{target}"),
            "from": format!("8:orgid:{}", self.me.id),
            "composetime": now,
            "originalarrivaltime": now,
            "content": crate::html::from_text(text),
            "messagetype": "RichText/Html",
            "contenttype": "Text",
            "imdisplayname": self.me.display_name,
            "clientmessageid": client_message_id,
            "callId": "",
            "state": 0,
            "version": "0",
            "amsreferences": [],
            "properties": {
                "importance": "",
                "subject": null,
                "title": "",
                "cards": "[]",
                "links": "[]",
                "mentions": "[]",
                "files": "[]",
                "formatVariant": "TEAMS",
            },
            "postType": "Standard",
            "crossPostChannels": [],
        });

        let url = format!(
            "{TEAMS}/api/chatsvc/{}/v1/users/ME/conversations/{}/messages",
            self.region,
            urlencode(&target)
        );
        let _: Value = self
            .call(scope::CHAT, &url, |http, token| {
                http.post(&url).bearer_auth(token).json(&body)
            })
            .await?;
        Ok(())
    }

    /// Directory users whose name or mail matches `query`, keyed by object id.
    pub async fn search_people(&self, query: &str, limit: usize) -> Result<Vec<(String, Person)>> {
        // Graph's $search grammar quotes each clause; a stray quote or
        // backslash would end the clause early and fail the whole request.
        let query: String = query.chars().filter(|c| !matches!(c, '"' | '\\')).collect();
        let search = format!("\"displayName:{query}\" OR \"mail:{query}\"");
        let top = limit.to_string();
        let url = "https://graph.microsoft.com/v1.0/users";
        let body: Value = self
            .call(scope::GRAPH, url, |http, token| {
                http.get(url)
                    .query(&[
                        ("$search", search.as_str()),
                        ("$select", "id,displayName,userPrincipalName,mail,jobTitle"),
                        ("$top", top.as_str()),
                    ])
                    // $search on directory objects is refused without it.
                    .header("ConsistencyLevel", "eventual")
                    .bearer_auth(token)
            })
            .await?;
        Ok(body["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|user| Some((user["id"].as_str()?.to_string(), graph_person(user))))
            .collect())
    }

    /// Names and UPNs for user object IDs, via Graph batch requests.
    pub async fn people(&self, ids: &[String]) -> Result<HashMap<String, Person>> {
        let mut people = HashMap::new();
        let url = "https://graph.microsoft.com/v1.0/$batch";
        for chunk in ids.chunks(GRAPH_BATCH_LIMIT) {
            let requests: Vec<Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    json!({
                        "id": i.to_string(),
                        "method": "GET",
                        "url": format!(
                            "/users/{id}?$select=id,displayName,userPrincipalName,mail,jobTitle"
                        ),
                    })
                })
                .collect();
            let batch = json!({ "requests": requests });
            let body: Value = self
                .call(scope::GRAPH, url, |http, token| {
                    http.post(url).bearer_auth(token).json(&batch)
                })
                .await?;
            for response in body["responses"].as_array().into_iter().flatten() {
                let user = &response["body"];
                if let Some(id) = user["id"].as_str() {
                    people.insert(id.to_string(), graph_person(user));
                }
            }
        }
        Ok(people)
    }
}

fn graph_person(user: &Value) -> Person {
    Person {
        display_name: user["displayName"].as_str().map(str::to_string),
        // Graph's `mail` is missing for guests; the UPN still works.
        mail: user["mail"]
            .as_str()
            .or(user["userPrincipalName"].as_str())
            .map(str::to_string),
        job_title: user["jobTitle"].as_str().map(str::to_string),
        upn: user["userPrincipalName"].as_str().map(str::to_string),
    }
}

// Two retries ride out Graph's short per-app bursts; a longer block (minutes)
// should surface as an error rather than freeze the UI or a tool call.
const THROTTLE_RETRIES: u32 = 2;
const MAX_THROTTLE_WAIT: Duration = Duration::from_secs(30);

/// Sends, waiting out a 429 the way Microsoft asks with Retry-After. Only
/// 429: it means the request was not processed, whereas a 503 on a send
/// may have posted the message, and retrying it would post it twice.
async fn send_throttled(build: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
    for attempt in 0..THROTTLE_RETRIES {
        let res = build().send().await?;
        if res.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(res);
        }
        let wait = retry_after(&res)
            .unwrap_or(Duration::from_secs(2 << attempt))
            .min(MAX_THROTTLE_WAIT);
        let path = res.url().path().to_string();
        tracing::info!(
            url = path,
            wait_secs = wait.as_secs(),
            "throttled; retrying"
        );
        tokio::time::sleep(wait).await;
    }
    Ok(build().send().await?)
}

/// Retry-After in its seconds form; the HTTP-date form falls back to backoff.
fn retry_after(res: &reqwest::Response) -> Option<Duration> {
    let secs = res
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs))
}

pub(crate) async fn decode<T: DeserializeOwned>(res: reqwest::Response, url: &str) -> Result<T> {
    let status = res.status();
    let body = res.text().await?;
    let path = url.split(['?', '#']).next().unwrap_or(url);
    if !status.is_success() {
        // Error bodies are Microsoft's error JSON, never message content.
        // Bounded and without the query, since this text reaches the log
        // and the status bar.
        let snippet: String = body.chars().take(300).collect();
        tracing::warn!(status = status.as_u16(), url = path, body = %snippet, "request failed");
        return Err(CoreError::Http {
            status: status.as_u16(),
            url: path.to_string(),
            body: snippet,
        });
    }
    // Some write endpoints answer 201 with an empty body.
    let body = if body.trim().is_empty() {
        "null"
    } else {
        &body
    };
    serde_json::from_str(body).map_err(|source| CoreError::Decode {
        url: path.to_string(),
        source,
    })
}

// Conversation IDs look like `19:abc@thread.v2`; Teams expects them unescaped
// apart from characters that would break the path.
fn urlencode(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b':'
            | b'@'
            | b';'
            | b'='
            | b',' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// Answers each connection with the next canned response, then stops.
    fn serve(responses: &'static [&'static str]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        url
    }

    #[tokio::test]
    async fn a_429_is_retried_after_the_wait_it_asks_for() {
        let url = serve(&[
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        ]);
        let http = reqwest::Client::new();
        let res = send_throttled(|| http.get(&url)).await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
    }

    #[tokio::test]
    async fn a_503_is_not_retried() {
        let url = serve(&[
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ]);
        let http = reqwest::Client::new();
        let res = send_throttled(|| http.get(&url)).await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    }
}
