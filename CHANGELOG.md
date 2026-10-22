# Changelog

## 0.1.0 - 2026-10-22

#### First public release

A Slack-style desktop client for Microsoft Teams chat on GPUI Kit: one sidebar with collapsible team groups and your own sections, channel threads in a side panel, All unreads, ⌘K and a command palette, real-time updates over Trouter, images and GIFs, @mentions, Zed themes, a settings window, and login storage in the Keychain, 1Password or a file. Calls and meetings open in Teams.

#### Show presence dots and typing

Sidebar entries for direct chats get a colored dot from Teams'
availability push, and a "{name} is typing…" label appears above the
composer while someone is typing (#11).

#### Windows and Linux release builds

The release workflow now builds and uploads `Snak-windows-x64.zip` and `Snak-linux-x64.zip` next to the macOS bundles, plain binaries for now. CI builds both (so a missing system library or link error fails there, not mid-release), keeping both compiling between releases so the next version cuts with all three platforms in it.

On Windows and Linux, settings, the token file and logs still live under a macOS-style `~/Library/...` path in the home directory for now.

#### Profile panel on user names

Clicking a username opens a side panel with who they are: photo, display
name, job title, presence, mail address with a copy button and a Call
button, over the DM with that person. It opens from message senders in
channels and threads, and from @mentions. Escape closes it.

#### Trouter frame shapes at debug level, for fast debugging

Running with `SNAK_LOG=snak_teams=debug` now logs every Trouter frame's shape: resourceTypes with their field names (message text never included), unknown resourceTypes flagged as `UNKNOWN push resource type`, acks, the registrar responses and the socket.io handshake. `cargo run -p snak-teams --example push` runs the push channel against the stored login so a debug log can be captured and shared, and `ThreadActivity/...` markers are ignored rather than landing in the message path.

#### MCP server over stdio

`snak --stdio` runs the binary as an MCP server instead of the app, sharing the already signed-in Teams session (#36). It serves `list_conversations`, `read_messages`, `search_people`, `find_dm`, `start_dm`, `get_person`, `send_message`, `mark_read`, `get_settings` and `set_setting`, so an assistant can find a colleague and chat with them through Teams without Microsoft's global MCP server, and adjust the app's appearance settings. Sign-in settings can't be changed over MCP.

#### Image viewer

Clicking an image in a message opens it large over the window, like Teams and Slack do. Arrow keys or the buttons step through the other images in the same message, and Escape, Close or a click outside the image closes it. GIFs open too and play in the viewer; the GIF badge on the thumbnail still plays and pauses it in place.

#### List chats Teams marks hidden

Teams flags chats you closed as hidden, and the conversation list dropped
them, so 1:1 DMs went missing from the sidebar and could not be searched
or opened. They now list like any other chat, sorted by last activity
under Direct messages (#33).

#### Older messages load when you scroll up

Conversations now serve messages in pages with a cursor for the page before them, so scrolling up in the timeline brings in older history instead of stopping at whatever the first fetch loaded. Chats follow the `_metadata.backwardLink` Teams returns with each page (only when it points back at the chat service), channels page through `continuationToken`, and the demo backend pages 20 at a time over seeded backlog, so the behavior is testable offline.

#### Throttled requests retried

Requests Microsoft throttles with HTTP 429 now wait for the time it asks for and retry, instead of failing the action or leaving the sidebar empty.

#### Distrusted certificates rejected

TLS certificate checks pick up rustls-webpki 0.103.16 and webpki-root-certs 1.0.10, which stop trusting the distrusted emSign intermediates (RUSTSEC-2026-0382, RUSTSEC-2026-0383).
