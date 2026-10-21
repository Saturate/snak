// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Tokens and signatures arrive in URLs after `?`, `#` or `&`.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for sep in ['?', '#', '&'] {
        if let Some(i) = out.find(sep) {
            out.truncate(i);
        }
    }
    out
}
