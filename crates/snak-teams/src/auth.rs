// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Login flow derived from Squads (https://github.com/IanTerzo/Squads, src/api.rs, MPL-2.0).

use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer};

use crate::error::{CoreError, Result, is_expired_session};

// The Teams first-party client ID. Using it means no app registration or admin
// consent, at the cost of relying on Teams' own permissions and internal APIs.
pub(crate) const CLIENT_ID: &str = "1fec8e78-bce4-4aaf-ab1b-5451cc387264";
const TENANT: &str = "organizations";

pub mod scope {
    pub const CHAT: &str = "https://ic3.teams.office.com/.default";
    pub const CHAT_AGGREGATOR: &str = "https://chatsvcagg.teams.microsoft.com/.default";
    pub const SKYPE: &str = "https://api.spaces.skype.com/Authorization.ReadWrite";
    pub const GRAPH: &str = "https://graph.microsoft.com/.default";
    pub const PRESENCE: &str = "https://presence.teams.microsoft.com/.default";
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub user_code: String,
    pub device_code: String,
    pub verification_url: String,
    #[serde(deserialize_with = "number_or_string")]
    pub expires_in: u64,
    #[serde(deserialize_with = "number_or_string")]
    pub interval: u64,
    pub message: String,
}

// The v1 device code endpoint returns numbers as strings.
fn number_or_string<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(u64),
        Text(String),
    }
    match Raw::deserialize(deserializer)? {
        Raw::Number(n) => Ok(n),
        Raw::Text(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct TokenError {
    error: String,
}

pub(crate) struct AccessToken {
    pub value: String,
    pub expires_at: Instant,
}

pub(crate) fn expires_in_secs(value: Option<&serde_json::Value>) -> u64 {
    match value {
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

pub async fn start_device_login(http: &reqwest::Client) -> Result<DeviceCode> {
    let url = format!("https://login.microsoftonline.com/{TENANT}/oauth2/devicecode");
    let res = http
        .post(&url)
        .form(&[
            ("client_id", CLIENT_ID),
            ("resource", "https://api.spaces.skype.com"),
        ])
        .send()
        .await?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        return Err(CoreError::Http {
            status: status.as_u16(),
            url,
            body,
        });
    }
    serde_json::from_str(&body).map_err(|source| CoreError::Decode { url, source })
}

/// Polls until the user finishes the browser step, then stores the refresh token.
pub async fn complete_device_login(http: &reqwest::Client, code: &DeviceCode) -> Result<String> {
    let url = format!("https://login.microsoftonline.com/{TENANT}/oauth2/token");
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    let interval = Duration::from_secs(code.interval.max(1));

    while Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        let res = http
            .post(&url)
            .form(&[
                ("client_id", CLIENT_ID),
                ("code", code.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;
        let status = res.status();
        let body = res.text().await?;

        if status.is_success() {
            let token: TokenResponse =
                serde_json::from_str(&body).map_err(|source| CoreError::Decode {
                    url: url.clone(),
                    source,
                })?;
            let refresh = token
                .refresh_token
                .ok_or_else(|| CoreError::SessionExpired("no refresh token in response".into()))?;
            crate::store::save(refresh.clone(), true).await?;
            return Ok(refresh);
        }

        let pending = serde_json::from_str::<TokenError>(&body)
            .map(|e| e.error == "authorization_pending" || e.error == "slow_down")
            .unwrap_or(false);
        if !pending {
            return Err(CoreError::Http {
                status: status.as_u16(),
                url,
                body,
            });
        }
    }
    Err(CoreError::LoginTimedOut)
}

/// Exchanges the refresh token for an access token for one scope. Entra rotates
/// refresh tokens, so the new one replaces the stored one.
pub(crate) async fn access_token(
    http: &reqwest::Client,
    refresh_token: &str,
    scope: &str,
) -> Result<(AccessToken, Option<String>)> {
    let url = format!("https://login.microsoftonline.com/{TENANT}/oauth2/v2.0/token");
    let full_scope = format!("{scope} openid profile offline_access");
    let res = http
        .post(&url)
        // Teams' client is registered as a SPA, so token redemption needs its origin.
        .header("Origin", "https://teams.microsoft.com")
        .form(&[
            ("client_id", CLIENT_ID),
            ("scope", full_scope.as_str()),
            ("grant_type", "refresh_token"),
            ("client_info", "1"),
            ("refresh_token", refresh_token),
            // CP1 tells Entra this client handles claims challenges (CAE).
            (
                "claims",
                r#"{"access_token":{"xms_cc":{"values":["CP1"]}}}"#,
            ),
        ])
        .send()
        .await?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        if is_expired_session(&body) {
            return Err(CoreError::SessionExpired(body));
        }
        return Err(CoreError::Http {
            status: status.as_u16(),
            url,
            body,
        });
    }
    let token: TokenResponse =
        serde_json::from_str(&body).map_err(|source| CoreError::Decode { url, source })?;
    let value = token
        .access_token
        .ok_or_else(|| CoreError::SessionExpired("no access token in response".into()))?;
    // Renew a minute early so a request never goes out with a token about to lapse.
    let lifetime = expires_in_secs(token.expires_in.as_ref()).saturating_sub(60);
    Ok((
        AccessToken {
            value,
            expires_at: Instant::now() + Duration::from_secs(lifetime),
        },
        token.refresh_token,
    ))
}
