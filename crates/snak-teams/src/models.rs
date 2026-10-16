// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Shapes derived from Squads (https://github.com/IanTerzo/Squads, src/api_types.rs, MPL-2.0).
// Every field the server might omit is optional: these are undocumented APIs.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct UserDetails {
    #[serde(default)]
    pub teams: Vec<Team>,
    #[serde(default)]
    pub chats: Vec<Chat>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Team {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub channels: Vec<Channel>,
    pub is_archived: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub id: String,
    pub display_name: String,
    pub is_general: Option<bool>,
    pub is_favorite: Option<bool>,
    pub is_member: Option<bool>,
    pub last_message: Option<Message>,
    pub consumption_horizon: Option<ConsumptionHorizon>,
    pub is_message_read: Option<bool>,
    pub is_muted: Option<bool>,
    #[serde(rename = "lastL2MessageIdNFS")]
    pub last_l2_message_id: Option<i64>,
}

impl Channel {
    /// The channel list sends `lastMessage` with every field null, so the
    /// newest message ID stands in for its time. Teams message IDs are
    /// arrival times in epoch milliseconds; anything outside a sane range is
    /// treated as not a timestamp.
    pub fn last_activity(&self) -> Option<DateTime<Utc>> {
        const EARLIEST_MS: i64 = 1_420_070_400_000; // 2015-01-01, before Teams existed
        if let Some(t) = self.last_message.as_ref().and_then(Message::sent_at) {
            return Some(t);
        }
        let ms = self.last_l2_message_id?;
        let latest_ms = Utc::now().timestamp_millis() + 86_400_000;
        (EARLIEST_MS..latest_ms)
            .contains(&ms)
            .then(|| DateTime::from_timestamp_millis(ms))
            .flatten()
    }

    pub fn is_unread(&self) -> bool {
        if let Some(read) = self.is_message_read {
            return !read;
        }
        let last = self.last_activity().map(|t| t.timestamp_millis());
        let read_up_to = self
            .consumption_horizon
            .as_ref()
            .and_then(|h| h.original_arrival_time);
        matches!((last, read_up_to), (Some(last), Some(read)) if last > read)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumptionHorizon {
    pub original_arrival_time: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMember {
    pub mri: String,
    pub object_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    #[serde(default)]
    pub members: Vec<ChatMember>,
    pub title: Option<String>,
    pub is_read: Option<bool>,
    pub is_one_on_one: Option<bool>,
    pub hidden: Option<bool>,
    pub chat_type: Option<String>,
    pub last_message: Option<Message>,
    pub is_conversation_deleted: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: Option<String>,
    pub content: Option<String>,
    #[serde(alias = "messageType", alias = "messagetype")]
    pub message_type: Option<String>,
    #[serde(alias = "imDisplayName", alias = "imdisplayname")]
    pub display_name: Option<String>,
    pub from: Option<String>,
    #[serde(alias = "originalArrivalTime", alias = "originalarrivaltime")]
    pub arrival_time: Option<String>,
    #[serde(alias = "composeTime", alias = "composetime")]
    pub compose_time: Option<String>,
    #[serde(alias = "clientMessageId", alias = "clientmessageid")]
    pub client_message_id: Option<String>,
    // Values here come back as strings, numbers or JSON-in-strings depending
    // on the endpoint, so they're read through the helpers below.
    pub properties: Option<Value>,
}

impl Message {
    pub fn sent_at(&self) -> Option<DateTime<Utc>> {
        let raw = self
            .arrival_time
            .as_deref()
            .or(self.compose_time.as_deref())?;
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }

    pub fn is_deleted(&self) -> bool {
        self.property_i64("deletetime") > 0
    }

    pub fn is_edited(&self) -> bool {
        self.property_i64("edittime") > 0
    }

    /// Sender object ID, from MRIs like `8:orgid:<guid>` or a contacts URL ending in one.
    pub fn sender_id(&self) -> Option<&str> {
        let from = self.from.as_deref()?;
        let mri = from.rsplit('/').next().unwrap_or(from);
        mri.strip_prefix("8:orgid:")
    }

    pub fn subject(&self) -> Option<String> {
        let value = self.properties.as_ref()?.get("subject")?.as_str()?;
        (!value.is_empty()).then(|| value.to_string())
    }

    pub fn reactions(&self) -> Vec<Reaction> {
        let Some(raw) = self.properties.as_ref().and_then(|p| p.get("emotions")) else {
            return Vec::new();
        };
        let parsed: Option<Vec<RawEmotion>> = match raw {
            Value::String(s) => serde_json::from_str(s).ok(),
            other => serde_json::from_value(other.clone()).ok(),
        };
        parsed
            .unwrap_or_default()
            .into_iter()
            .filter(|e| !e.users.is_empty())
            .map(|e| Reaction {
                key: e.key,
                count: e.users.len(),
            })
            .collect()
    }

    fn property_i64(&self, key: &str) -> i64 {
        match self.properties.as_ref().and_then(|p| p.get(key)) {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.parse().unwrap_or(0),
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawEmotion {
    key: String,
    #[serde(default)]
    users: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct Reaction {
    pub key: String,
    pub count: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessagePage {
    #[serde(default)]
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplyChain {
    pub id: String,
    #[serde(default)]
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelPage {
    #[serde(default)]
    pub reply_chains: Vec<ReplyChain>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    pub id: String,
    pub display_name: Option<String>,
    pub mail: Option<String>,
    pub user_principal_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Person {
    pub display_name: Option<String>,
    pub upn: Option<String>,
}
