//! App-wide settings, edited in the settings window and saved as JSON.
//! Sidebar layout (stars, sections, collapse) stays in `prefs`, per backend.

use std::path::PathBuf;

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, px};
use serde::{Deserialize, Serialize};

use crate::theme::{self, InstalledThemes};
use snak_chat::CredentialStorage;

/// 1Password accounts the `op` CLI knows, as (sign-in address, email).
/// Empty when the CLI isn't installed.
pub fn onepassword_accounts() -> Vec<(String, String)> {
    let Ok(out) = std::process::Command::new("op")
        .args(["account", "list", "--format", "json"])
        .output()
    else {
        return Vec::new();
    };
    let Ok(accounts) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Vec::new();
    };
    accounts
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some((
                a["url"].as_str()?.to_string(),
                a["email"].as_str()?.to_string(),
            ))
        })
        .collect()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

    pub fn next(self) -> Self {
        match self {
            Appearance::System => Appearance::Light,
            Appearance::Light => Appearance::Dark,
            Appearance::Dark => Appearance::System,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }

    pub fn from_label(label: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|a| a.label() == label)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    /// None follows whatever Zed's settings pick.
    pub dark_theme: Option<String>,
    pub light_theme: Option<String>,
    pub ui_font_size: f64,
    pub dm_limit: f64,
    pub show_team_names: bool,
    pub login_storage: LoginStorage,
    /// Sign-in address of the 1Password account to use, e.g. my.1password.eu.
    pub onepassword_account: Option<String>,
    /// Replaced by `login_storage`; read once so an earlier choice carries over.
    #[serde(skip_serializing)]
    insecure_credentials: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoginStorage {
    #[default]
    Keychain,
    File,
    OnePassword,
}

impl LoginStorage {
    pub const ALL: [LoginStorage; 3] = [
        LoginStorage::Keychain,
        LoginStorage::OnePassword,
        LoginStorage::File,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LoginStorage::Keychain => "macOS Keychain",
            LoginStorage::OnePassword => "1Password",
            LoginStorage::File => "File (less secure)",
        }
    }

    pub fn from_label(label: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|s| s.label() == label)
            .unwrap_or_default()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            dark_theme: None,
            light_theme: None,
            ui_font_size: 14.,
            dm_limit: 40.,
            show_team_names: true,
            login_storage: LoginStorage::Keychain,
            onepassword_account: None,
            insecure_credentials: false,
        }
    }
}

impl Global for Settings {}

impl Settings {
    pub fn get(cx: &App) -> &Settings {
        cx.global::<Settings>()
    }

    fn path() -> Option<PathBuf> {
        Some(std::env::home_dir()?.join("Library/Application Support/dev.snak.app/settings.json"))
    }

    pub fn load() -> Self {
        let mut settings: Settings = Self::path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        if settings.insecure_credentials {
            settings.login_storage = LoginStorage::File;
        }
        settings
    }

    /// The backend-neutral form; 1Password without an account falls back to the Keychain.
    pub fn credential_storage(&self) -> CredentialStorage {
        match (self.login_storage, &self.onepassword_account) {
            (LoginStorage::File, _) => CredentialStorage::File,
            (LoginStorage::OnePassword, Some(account)) => CredentialStorage::OnePassword {
                account: account.clone(),
            },
            _ => CredentialStorage::Keychain,
        }
    }

    fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        let Ok(json) = serde_json::to_vec_pretty(self) else {
            return;
        };
        if let Some(dir) = path.parent()
            && std::fs::create_dir_all(dir).is_ok()
        {
            // Write then rename so a crash mid-write can't leave a truncated file.
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        }
    }

    /// Changes a setting, saves it, and re-applies everything to all windows.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
        change(cx.global_mut::<Settings>());
        Settings::get(cx).save();
        Settings::apply(cx);
    }

    pub fn dark_theme_name(&self, cx: &App) -> String {
        self.dark_theme
            .clone()
            .unwrap_or_else(|| cx.global::<InstalledThemes>().zed_dark.clone())
    }

    pub fn light_theme_name(&self, cx: &App) -> String {
        self.light_theme
            .clone()
            .unwrap_or_else(|| cx.global::<InstalledThemes>().zed_light.clone())
    }

    pub fn apply(cx: &mut App) {
        let settings = Settings::get(cx).clone();
        theme::select(
            &settings.dark_theme_name(cx),
            &settings.light_theme_name(cx),
            cx,
        );
        match settings.appearance {
            Appearance::System => Theme::sync_system_appearance(None, cx),
            Appearance::Light => Theme::change(ThemeMode::Light, None, cx),
            Appearance::Dark => Theme::change(ThemeMode::Dark, None, cx),
        }
        // Theme changes reset the font size to the theme's, so this goes last.
        Theme::global_mut(cx).font_size = px(settings.ui_font_size as f32);
        cx.refresh_windows();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_files_missing_fields_still_load() {
        let settings: Settings = serde_json::from_str(r#"{"appearance":"Dark"}"#).expect("loads");
        assert_eq!(settings.appearance, Appearance::Dark);
        assert_eq!(settings.dm_limit, 40.);
        assert!(settings.dark_theme.is_none());
    }

    #[test]
    fn earlier_file_storage_choice_carries_over() {
        let mut settings: Settings =
            serde_json::from_str(r#"{"insecure_credentials":true}"#).expect("loads");
        if settings.insecure_credentials {
            settings.login_storage = LoginStorage::File;
        }
        assert_eq!(settings.credential_storage(), CredentialStorage::File);
        let saved = serde_json::to_string(&settings).expect("saves");
        assert!(!saved.contains("insecure_credentials"));
    }

    #[test]
    fn onepassword_needs_an_account() {
        let mut settings = Settings {
            login_storage: LoginStorage::OnePassword,
            ..Settings::default()
        };
        assert_eq!(settings.credential_storage(), CredentialStorage::Keychain);
        settings.onepassword_account = Some("my.1password.eu".into());
        assert_eq!(
            settings.credential_storage(),
            CredentialStorage::OnePassword {
                account: "my.1password.eu".into()
            }
        );
    }

    #[test]
    fn appearance_labels_round_trip() {
        for appearance in Appearance::ALL {
            assert_eq!(Appearance::from_label(appearance.label()), appearance);
        }
    }
}
