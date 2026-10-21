// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Error bodies can echo request details, and tokens and signatures arrive
/// in URLs after `?`, `#` or `&`: keep the first line, up to the first of
/// those, and at most 300 characters.
pub fn redact(error: &str) -> String {
    let line = error.lines().next().unwrap_or("");
    let end = line.find(['?', '#', '&']).unwrap_or(line.len());
    line[..end].chars().take(300).collect()
}
