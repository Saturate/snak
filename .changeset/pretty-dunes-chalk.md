---
default: minor
---

#### MCP server over stdio

`snak --stdio` runs the binary as an MCP server instead of the app, sharing the already signed-in Teams session (#36). It serves `list_conversations`, `read_messages`, `search_people`, `find_dm`, `start_dm`, `get_person`, `send_message`, `mark_read`, `get_settings` and `set_setting`, so an assistant can find a colleague and chat with them through Teams without Microsoft's global MCP server, and adjust the app's appearance settings. Sign-in settings can't be changed over MCP.
