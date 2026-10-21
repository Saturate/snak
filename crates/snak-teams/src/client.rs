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
use crate::models::{ChannelPage, Me, MessagePage, Person, UserDetails};
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
        let token = self.token(scope).await?;
        let res = build(&self.http, token.clone()).send().await?;
        if res.status() != reqwest::StatusCode::UNAUTHORIZED {
            return decode(res, url).await;
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
        let res = build(&self.http, token).send().await?;
        decode(res, url).await
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

    /// The page of a chat's messages behind `backward_link`, the link an
    /// earlier [`Self::chat_messages`] or `chat_messages_before` page gave.
    pub async fn chat_messages_before(&self, backward_link: &str) -> Result<MessagePage> {
        // The link comes out of a response body; the bearer token only goes
        // where chat_messages itself would send it.
        let Some(link) = chat_link(backward_link, &self.region) else {
            return Err(CoreError::UntrustedLink);
        };
        self.get_json(scope::CHAT, link.as_str()).await
    }

    /// Recent channel posts, each with its replies, with where the next,
    /// older page keeps going.
    pub async fn channel_posts(&self, team_id: &str, channel_id: &str) -> Result<ChannelPage> {
        let url = format!(
            "{TEAMS}/api/csa/{}/api/v2/teams/{}/channels/{}",
            self.region,
            urlencode(team_id),
            urlencode(channel_id)
        );
        self.get_json(scope::CHAT_AGGREGATOR, &url).await
    }

    /// The channel posts older than `continuation_token`, a cursor from an
    /// earlier [`Self::channel_posts`] or `channel_posts_before` response.
    pub async fn channel_posts_before(
        &self,
        team_id: &str,
        channel_id: &str,
        continuation_token: &str,
    ) -> Result<ChannelPage> {
        let url = format!(
            "{TEAMS}/api/csa/{}/api/v2/teams/{}/channels/{}?continuationToken={}",
            self.region,
            urlencode(team_id),
            urlencode(channel_id),
            urlencode(continuation_token)
        );
        self.get_json(scope::CHAT_AGGREGATOR, &url).await
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
                        "url": format!("/users/{id}?$select=id,displayName,userPrincipalName"),
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
                    people.insert(
                        id.to_string(),
                        Person {
                            display_name: user["displayName"].as_str().map(str::to_string),
                            upn: user["userPrincipalName"].as_str().map(str::to_string),
                        },
                    );
                }
            }
        }
        Ok(people)
    }
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

/// The link as it will be requested, if that's under this region's chatsvc
/// conversations. The check runs on the parsed form because the parser
/// drops tabs and newlines and resolves `..`, `%2e` and `\` before the
/// request goes out, so the raw text can look inside and land outside.
fn chat_link(link: &str, region: &str) -> Option<url::Url> {
    if region.is_empty() || link.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let url = url::Url::parse(link).ok()?;
    let prefix = format!("{TEAMS}/api/chatsvc/{region}/v1/users/ME/conversations/");
    url.as_str().starts_with(&prefix).then_some(url)
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
    use super::chat_link;

    fn is_chat_link(link: &str, region: &str) -> bool {
        chat_link(link, region).is_some()
    }

    #[test]
    fn only_chatsvc_links_in_our_region_get_the_token() {
        let ok = "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations/19:a@thread.v2/messages?startTime=1&syncState=x&pageSize=200";
        assert!(is_chat_link(ok, "emea"));
        assert!(!is_chat_link(ok, "amer"));
        assert!(!is_chat_link(ok, ""));
        // Parsing only tidies it; it still lands in the same place.
        assert!(is_chat_link(
            "HTTPS://TEAMS.MICROSOFT.COM/api/chatsvc/emea/v1/users/ME/conversations/c/messages",
            "emea"
        ));
        for bad in [
            "https://evil.example/api/chatsvc/emea/v1/users/ME/conversations/c/messages",
            "https://teams.microsoft.com.evil.example/api/chatsvc/emea/v1/users/ME/conversations/c",
            "http://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations/c/messages",
            "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/properties",
            "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations/../../../x",
            "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations/c\\..\\..\\x",
            "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations/.\t./.\t./x",
            "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations/.\n./.\r./x",
            "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations/c /messages",
            "https://teams.microsoft.com:444/api/chatsvc/emea/v1/users/ME/conversations/c",
            "https://user@teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations/c",
            "HTTPS://TEAMS.MICROSOFT.COM/api/chatsvc/emea/v1/users/ME/conversations/%2e%2e/%2e%2e/x",
            "https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations/%2E%2e/%2e./x",
            "",
        ] {
            assert!(!is_chat_link(bad, "emea"), "{bad}");
        }
    }
}
