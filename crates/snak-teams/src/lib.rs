// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Teams client core for Snak: login, Teams' internal chat APIs, push events and models.

pub mod auth;
mod backend;
mod client;
mod error;
pub mod html;
pub mod models;
mod trouter;

pub use backend::{TeamsBackend, TeamsConnector};
pub use client::TeamsClient;
pub use error::{CoreError, Result};
pub use trouter::{Presence, PushEvent, PushSession};
