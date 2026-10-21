// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. A copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Runs the push channel against a real account using the stored login,
//! for issue #19: verifying the Trouter payload guesses against live
//! traffic. Prints event counts, never message text, names or tokens;
//! set SNAK_LOG=snak_teams=debug for raw frame shapes.
//!
//! cargo run -p snak-teams --example push

mod common;

use std::sync::Arc;
use std::time::Instant;

use common::redact;
use snak_chat::{ChatBackend, Connector, Event};
use snak_teams::TeamsConnector;

const SNAK_LOG: &str = "SNAK_LOG";
const SNAK_STORE: &str = "SNAK_STORE";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env(SNAK_LOG)
                .unwrap_or_else(|_| "snak_teams=debug,info".into()),
        )
        .with_target(true)
        .init();

    let connector = TeamsConnector::new();
    // Use the same place the app keeps the login: SNAK_STORE=file|keychain.
    // Without moving: a probe must never relocate the app's login.
    if std::env::var(SNAK_STORE).as_deref() == Ok("file") {
        snak_teams::store::use_storage_without_moving(snak_teams::store::Storage::File);
    }
    let backend: Arc<dyn ChatBackend> = match connector.restore().await {
        Ok(Some(backend)) => backend,
        Ok(None) => return println!("no stored login; sign in through the app first"),
        Err(e) => return println!("connect failed: {}", redact(&e.to_string())),
    };
    let mut events = Arc::clone(&backend).subscribe();

    let started = Instant::now();
    let mut total = 0usize;
    while let Some(event) = events.recv().await {
        let elapsed = started.elapsed();
        match event {
            Event::Connected => println!("{elapsed:?}: connected"),
            Event::Reconnecting => println!("{elapsed:?}: reconnecting"),
            Event::SessionExpired => return println!("{elapsed:?}: session expired"),
            Event::ConversationsChanged => println!("{elapsed:?}: conversations changed"),
            Event::MessagesChanged(id) => println!("{elapsed:?}: messages changed: {}", id.0),
            // Names and presence words identify people; the kind is enough.
            Event::Typing { stopped, .. } => println!("{elapsed:?}: typing stopped={stopped}"),
            Event::PresenceChanged { .. } => println!("{elapsed:?}: presence changed"),
        }
        total += 1;
    }
    println!("channel closed after {total} events")
}
