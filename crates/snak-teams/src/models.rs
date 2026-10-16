// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Shapes derived from Squads (https://github.com/IanTerzo/Squads, src/api_types.rs, MPL-2.0).
// Every field the server might omit is optional: these are undocumented APIs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
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
    #[serde(
        rename = "lastL2MessageIdNFS",
        default,
        deserialize_with = "lenient_i64"
    )]
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
    #[serde(default, deserialize_with = "lenient_i64")]
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

#[derive(Debug, Clone)]
pub struct Message {
    pub id: Option<String>,
    pub content: Option<String>,
    pub message_type: Option<String>,
    pub display_name: Option<String>,
    pub from: Option<String>,
    pub arrival_time: Option<String>,
    pub compose_time: Option<String>,
    pub client_message_id: Option<String>,
    // Values here come back as strings, numbers or JSON-in-strings depending
    // on the endpoint, so they're read through the helpers below.
    pub properties: Option<Value>,
}

// The chat service spells fields in lowercase, the aggregator in camelCase,
// and a payload carrying both would fail serde's aliases as a duplicate
// field, losing the whole page. So pick by hand: first spelling present wins.
impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom("a message must be a JSON object"));
        }
        let text = |keys: &[&str]| {
            keys.iter().find_map(|key| match value.get(*key)? {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
        };
        Ok(Message {
            id: text(&["id"]),
            content: text(&["content"]),
            message_type: text(&["messagetype", "messageType"]),
            display_name: text(&["imdisplayname", "imDisplayName"]),
            from: text(&["from"]),
            arrival_time: text(&["originalarrivaltime", "originalArrivalTime"]),
            compose_time: text(&["composetime", "composeTime"]),
            client_message_id: text(&["clientmessageid", "clientMessageId"]),
            properties: value.get("properties").cloned(),
        })
    }
}

/// Accepts a number or a numeric string; anything else is treated as absent
/// rather than failing the whole list it sits in.
fn lenient_i64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    })
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

    /// `(itemid, mri)` for each @mention; the itemid ties it to its spans in
    /// the HTML. Teams sends the list as a JSON string or, sometimes, inline.
    pub fn mentions(&self) -> Vec<(String, String)> {
        let Some(raw) = self.properties.as_ref().and_then(|p| p.get("mentions")) else {
            return Vec::new();
        };
        let list: Option<Vec<Value>> = match raw {
            Value::String(s) => serde_json::from_str(s).ok(),
            other => serde_json::from_value(other.clone()).ok(),
        };
        list.unwrap_or_default()
            .into_iter()
            .filter_map(|m| {
                let id = match &m["itemid"] {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => return None,
                };
                Some((id, m["mri"].as_str()?.to_string()))
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_spellings_of_a_field_still_parse() {
        let message: Message = serde_json::from_value(serde_json::json!({
            "id": 42, "messagetype": "Text", "messageType": "RichText/Html", "content": "x"
        }))
        .expect("parses");
        assert_eq!(message.message_type.as_deref(), Some("Text"));
        assert_eq!(message.id.as_deref(), Some("42"));
    }

    #[test]
    fn numbers_sent_as_strings_do_not_fail_the_channel() {
        let channel: Channel = serde_json::from_value(serde_json::json!({
            "id": "c", "displayName": "a",
            "lastL2MessageIdNFS": "1760000000000",
            "consumptionHorizon": { "originalArrivalTime": "oops" }
        }))
        .expect("parses");
        assert_eq!(channel.last_l2_message_id, Some(1_760_000_000_000));
        assert_eq!(
            channel
                .consumption_horizon
                .and_then(|h| h.original_arrival_time),
            None
        );
    }
}
