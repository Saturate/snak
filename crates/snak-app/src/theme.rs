//! Zed themes in Snak. Zed theme files are converted to GPUI Kit's format at
//! startup: the bundled themes (all MIT, see assets/themes/LICENSES.md), plus
//! whatever the user has in Zed's theme folders. Snak follows the theme names
//! picked in Zed's settings when it can find them.

use std::path::{Path, PathBuf};

use gpui_kit::component::{Theme, ThemeRegistry};
use gpui_kit::{App, Global};
use serde_json::{Map, Value, json};

const BUNDLED: [&str; 9] = [
    include_str!("../assets/themes/one.json"),
    include_str!("../assets/themes/ayu.json"),
    include_str!("../assets/themes/gruvbox.json"),
    include_str!("../assets/themes/catppuccin.json"),
    include_str!("../assets/themes/rose-pine.json"),
    include_str!("../assets/themes/rose-pine-moon.json"),
    include_str!("../assets/themes/rose-pine-dawn.json"),
    include_str!("../assets/themes/dracula.json"),
    include_str!("../assets/themes/tokyo-night.json"),
];
const DEFAULT_DARK: &str = "One Dark";
const DEFAULT_LIGHT: &str = "One Light";

/// Themes found at startup, for the settings window.
#[derive(Default)]
pub struct InstalledThemes {
    /// (name, is dark), sorted by name.
    pub themes: Vec<(String, bool)>,
    /// What Zed's own settings pick, used when Snak has no choice saved.
    pub zed_dark: String,
    pub zed_light: String,
}

impl Global for InstalledThemes {}

pub fn install(cx: &mut App) {
    let mut families: Vec<String> = BUNDLED.iter().map(|f| f.to_string()).collect();
    for dir in user_theme_dirs() {
        families.extend(read_json_files(&dir));
    }

    let mut names = Vec::new();
    for family in &families {
        let Ok(family) = serde_json::from_str::<Value>(family) else {
            continue;
        };
        for theme in family["themes"].as_array().into_iter().flatten() {
            let Some(name) = theme["name"].as_str() else {
                continue;
            };
            if names.iter().any(|(n, _)| n == name) {
                continue;
            }
            if load(&convert(theme, true), cx) || load(&convert(theme, false), cx) {
                names.push((name.to_string(), theme["appearance"] != "light"));
            }
        }
    }
    names.sort();

    let (dark, light) = zed_selection();
    cx.set_global(InstalledThemes {
        themes: names,
        zed_dark: dark.unwrap_or_else(|| DEFAULT_DARK.to_string()),
        zed_light: light.unwrap_or_else(|| DEFAULT_LIGHT.to_string()),
    });
}

/// Makes these the themes used for dark and light mode; unknown names are ignored.
/// Unknown names (a theme that was uninstalled) fall back to Zed's choice,
/// then to the bundled One themes, never to GPUI Kit's own default.
pub fn select(dark: &str, light: &str, cx: &mut App) {
    let themes = ThemeRegistry::global(cx).themes().clone();
    let (zed_dark, zed_light) = cx
        .try_global::<InstalledThemes>()
        .map(|i| (i.zed_dark.clone(), i.zed_light.clone()))
        .unwrap_or_default();
    let find = |names: [&str; 3]| names.into_iter().find_map(|n| themes.get(n).cloned());
    let theme = Theme::global_mut(cx);
    if let Some(found) = find([dark, &zed_dark, DEFAULT_DARK]) {
        theme.dark_theme = found;
    }
    if let Some(found) = find([light, &zed_light, DEFAULT_LIGHT]) {
        theme.light_theme = found;
    }
}

fn load(theme_set: &Value, cx: &mut App) -> bool {
    ThemeRegistry::global_mut(cx)
        .load_themes_from_str(&theme_set.to_string())
        .is_ok()
}

fn home() -> Option<PathBuf> {
    std::env::home_dir()
}

fn user_theme_dirs() -> Vec<PathBuf> {
    let Some(home) = home() else {
        return Vec::new();
    };
    let mut dirs = vec![home.join(".config/zed/themes")];
    let extensions = home.join("Library/Application Support/Zed/extensions/installed");
    if let Ok(entries) = std::fs::read_dir(extensions) {
        dirs.extend(entries.flatten().map(|e| e.path().join("themes")));
    }
    dirs
}

fn read_json_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect()
}

/// The dark and light theme names from Zed's settings, if set.
fn zed_selection() -> (Option<String>, Option<String>) {
    let Some(settings) = home()
        .and_then(|h| std::fs::read_to_string(h.join(".config/zed/settings.json")).ok())
        .and_then(|text| serde_json::from_str::<Value>(&strip_jsonc(&text)).ok())
    else {
        return (None, None);
    };
    match &settings["theme"] {
        // A single name means Zed uses it in both modes.
        Value::String(name) => (Some(name.clone()), Some(name.clone())),
        Value::Object(theme) => (
            theme
                .get("dark")
                .and_then(Value::as_str)
                .map(str::to_string),
            theme
                .get("light")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        _ => (None, None),
    }
}

/// Zed's settings file is JSON with comments and trailing commas.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut previous = ' ';
                for c in chars.by_ref() {
                    if previous == '*' && c == '/' {
                        break;
                    }
                    previous = c;
                }
            }
            _ => out.push(c),
        }
    }
    // Commas go in a second pass over comment-free text, so a comment between
    // a trailing comma and the closing bracket can't hide it.
    drop_trailing_commas(&out)
}

fn drop_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ',' => {
                let after = chars.clone().find(|c| !c.is_whitespace());
                if !matches!(after, Some('}') | Some(']')) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// One Zed theme (`{name, appearance, style}`) as a GPUI Kit theme set.
fn convert(theme: &Value, with_syntax: bool) -> Value {
    let style = &theme["style"];
    let get = |key: &str| style.get(key).and_then(Value::as_str).map(str::to_string);
    let pick = |keys: &[&str]| keys.iter().find_map(|k| get(k));
    let cursor = style["players"][0]["cursor"].as_str().map(str::to_string);
    let selection = style["players"][0]["selection"]
        .as_str()
        .map(str::to_string);

    let mut colors = Map::new();
    let mut set = |target: &str, value: Option<String>| {
        if let Some(value) = value {
            colors.insert(target.to_string(), Value::String(value));
        }
    };
    let bg = pick(&["editor.background", "background"]);
    let text = pick(&["text", "editor.foreground"]);
    let accent = pick(&["text.accent", "icon.accent", "info"]);
    let panel = pick(&["panel.background", "surface.background"]);
    let subtle_border = pick(&["border.variant", "border"]);
    let hover = pick(&["ghost_element.hover", "element.hover"]);
    let selected = pick(&["ghost_element.selected", "element.selected"]);

    set("background", bg.clone());
    set("foreground", text.clone());
    set("border", subtle_border.clone());
    set("window.border", get("border"));
    set("muted.background", hover.clone());
    set("muted.foreground", get("text.muted"));
    set("accent.background", hover.clone());
    set("accent.foreground", text.clone());
    for key in [
        "primary.background",
        "primary.hover.background",
        "primary.active.background",
    ] {
        set(key, accent.clone());
    }
    set("primary.foreground", bg.clone());
    set("secondary.background", get("element.background"));
    set("secondary.hover.background", get("element.hover"));
    set("secondary.active.background", get("element.active"));
    set("secondary.foreground", text.clone());
    set("input.border", get("border"));
    set("ring", get("border.focused"));
    set("caret", cursor);
    set("selection.background", selection);
    for key in ["link", "link.hover", "link.active"] {
        set(key, accent.clone());
    }
    set("list.background", panel.clone());
    set("list.hover.background", hover.clone());
    set("list.active.background", selected.clone());
    set("list.active.border", get("border.focused"));
    set("popover.background", get("elevated_surface.background"));
    set("popover.foreground", text.clone());
    set("sidebar.background", panel.clone());
    set("sidebar.foreground", text.clone());
    set("sidebar.border", subtle_border.clone());
    set("sidebar.accent.background", hover);
    set("sidebar.accent.foreground", text.clone());
    // Zed marks the selected row with a quiet fill, not a solid accent bar.
    set("sidebar.primary.background", selected);
    set("sidebar.primary.foreground", text.clone());
    set("title_bar.background", get("title_bar.background"));
    set("title_bar.border", subtle_border.clone());
    set("status_bar.background", get("status_bar.background"));
    set("status_bar.border", subtle_border);
    set("tab_bar.background", get("tab_bar.background"));
    set("tab.background", get("tab.inactive_background"));
    set("tab.active.background", get("tab.active_background"));
    set("tab.active.foreground", text);
    set("tab.foreground", get("text.muted"));
    set("scrollbar.background", get("scrollbar.track.background"));
    set(
        "scrollbar.thumb.background",
        get("scrollbar.thumb.background"),
    );
    set(
        "scrollbar.thumb.hover.background",
        get("scrollbar.thumb.hover_background"),
    );
    set("danger.background", get("error"));
    set("danger.foreground", bg);
    set("success.background", get("success"));
    set("warning.background", get("warning"));
    set("info.background", get("info"));
    set("drop_target.background", get("drop_target.background"));
    set("skeleton.background", get("element.background"));
    for (target, ansi, fallback) in [
        ("base.red", "terminal.ansi.red", "error"),
        ("base.green", "terminal.ansi.green", "success"),
        ("base.yellow", "terminal.ansi.yellow", "warning"),
        ("base.blue", "terminal.ansi.blue", "info"),
        ("base.magenta", "terminal.ansi.magenta", "info"),
        ("base.cyan", "terminal.ansi.cyan", "info"),
    ] {
        set(target, pick(&[ansi, fallback]));
    }

    let mut config = json!({
        "name": theme["name"],
        "mode": if theme["appearance"] == "light" { "light" } else { "dark" },
        // Zed's UI is denser than GPUI Kit's defaults.
        "font.size": 14,
        "mono_font.size": 13,
        "radius": 4,
        "radius.lg": 6,
        "shadow": false,
        "colors": colors,
    });
    // GPUI Kit reads Zed's `style` section directly for code highlighting.
    if with_syntax {
        config["highlight"] = style.clone();
    }
    json!({ "name": theme["name"], "themes": [config] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_comments_and_trailing_commas_are_removed() {
        let text = r#"{
            // comment
            "url": "https://x//y", /* block */
            "theme": { "dark": "One Dark", },
            "list": [1, 2,],
        }"#;
        let parsed: Value = serde_json::from_str(&strip_jsonc(text)).expect("valid json");
        assert_eq!(parsed["url"], "https://x//y");
        assert_eq!(parsed["theme"]["dark"], "One Dark");
        assert_eq!(parsed["list"], json!([1, 2]));
    }

    #[test]
    fn trailing_comma_before_a_comment_is_dropped() {
        let text = "{\n  \"dark\": \"Gruvbox Dark\", // favourite\n  /* x */\n}";
        let parsed: Value = serde_json::from_str(&strip_jsonc(text)).expect("valid json");
        assert_eq!(parsed["dark"], "Gruvbox Dark");
    }

    #[test]
    fn bundled_one_themes_convert() {
        let family: Value = serde_json::from_str(BUNDLED[0]).expect("bundled theme parses");
        let themes = family["themes"].as_array().expect("themes");
        let names: Vec<&str> = themes.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, [DEFAULT_DARK, DEFAULT_LIGHT]);
        let dark = convert(&themes[0], true);
        let colors = &dark["themes"][0]["colors"];
        assert_eq!(colors["background"], "#282c33ff");
        assert_eq!(colors["sidebar.background"], "#2f343eff");
        assert_eq!(dark["themes"][0]["mode"], "dark");
    }

    #[test]
    fn every_bundled_theme_converts_with_syntax_colors() {
        let mut count = 0;
        for file in BUNDLED {
            let family: Value = serde_json::from_str(file).expect("bundled theme parses");
            for theme in family["themes"].as_array().expect("themes") {
                let name = theme["name"].as_str().unwrap_or("?").to_string();
                let parsed: Result<gpui_kit::component::ThemeSet, _> =
                    serde_json::from_value(convert(theme, true));
                let set = parsed.unwrap_or_else(|e| panic!("{name}: {e}"));
                assert!(set.themes[0].highlight.is_some(), "{name}: syntax colors");
                assert!(
                    set.themes[0].colors.background.is_some(),
                    "{name}: background"
                );
                count += 1;
            }
        }
        assert_eq!(count, 25);
    }
}
