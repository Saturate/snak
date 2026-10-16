//! Sidebar organisation that only exists in Snak: stars, sections, hide and mute.
//! Teams has no server-side place for these, so they live in a local file.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub starred: Vec<String>,
    pub sections: Vec<Section>,
    pub hidden: HashSet<String>,
    pub muted: HashSet<String>,
    pub unreads_only: bool,
    /// Keys of collapsed sidebar groups (see [`group_key`]).
    pub collapsed: HashSet<String>,
}

/// Sidebar groups and whole teams share one key space with conversations,
/// so a team can be filed into a section the same way a channel can.
pub fn team_key(team_id: &str) -> String {
    format!("team:{team_id}")
}

pub fn group_key(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub name: String,
    pub members: Vec<String>,
}

/// Where an entry is filed in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Starred,
    Section(usize),
    Default,
}

impl Prefs {
    /// One file per backend, so the demo workspace never touches real settings.
    fn path(profile: &str) -> Option<PathBuf> {
        let home = std::env::home_dir()?;
        let slug: String = profile
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        Some(home.join(format!(
            "Library/Application Support/dev.snak.app/prefs-{slug}.json"
        )))
    }

    pub fn load(profile: &str) -> Self {
        Self::path(profile)
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, profile: &str) -> std::io::Result<()> {
        let Some(path) = Self::path(profile) else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        // Write then rename so a crash mid-write can't leave a truncated file.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(tmp, path)
    }

    pub fn placement(&self, key: &str) -> Placement {
        if self.starred.iter().any(|k| k == key) {
            return Placement::Starred;
        }
        self.sections
            .iter()
            .position(|s| s.members.iter().any(|k| k == key))
            .map_or(Placement::Default, Placement::Section)
    }

    /// Takes the entry out of Starred and every section.
    fn unfile(&mut self, key: &str) {
        self.starred.retain(|k| k != key);
        for section in &mut self.sections {
            section.members.retain(|k| k != key);
        }
    }

    pub fn toggle_star(&mut self, key: &str) {
        let was_starred = self.placement(key) == Placement::Starred;
        self.unfile(key);
        if !was_starred {
            self.starred.push(key.to_string());
        }
    }

    pub fn move_to_section(&mut self, key: &str, section: usize) {
        if section >= self.sections.len() {
            return;
        }
        self.unfile(key);
        self.sections[section].members.push(key.to_string());
    }

    pub fn remove_from_section(&mut self, key: &str) {
        self.unfile(key);
    }

    pub fn add_section(&mut self, name: &str, first_member: Option<&str>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        self.sections.push(Section {
            name: name.to_string(),
            members: Vec::new(),
        });
        if let Some(key) = first_member {
            self.move_to_section(key, self.sections.len() - 1);
        }
    }

    pub fn delete_section(&mut self, section: usize) {
        if section < self.sections.len() {
            self.sections.remove(section);
        }
    }

    pub fn toggle_hidden(&mut self, key: &str) {
        if !self.hidden.remove(key) {
            self.hidden.insert(key.to_string());
            // Hiding something you also filed would make it reappear there.
            self.unfile(key);
        }
    }

    pub fn toggle_collapsed(&mut self, key: &str) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.to_string());
        }
    }

    pub fn toggle_muted(&mut self, key: &str) {
        if !self.muted.remove(key) {
            self.muted.insert(key.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starring_moves_out_of_a_section_and_back_to_default() {
        let mut prefs = Prefs::default();
        prefs.add_section("Customer X", Some("c1"));
        assert_eq!(prefs.placement("c1"), Placement::Section(0));
        prefs.toggle_star("c1");
        assert_eq!(prefs.placement("c1"), Placement::Starred);
        assert!(prefs.sections[0].members.is_empty());
        prefs.toggle_star("c1");
        assert_eq!(prefs.placement("c1"), Placement::Default);
    }

    #[test]
    fn hiding_unfiles_the_entry() {
        let mut prefs = Prefs::default();
        prefs.toggle_star("c1");
        prefs.toggle_hidden("c1");
        assert!(prefs.hidden.contains("c1"));
        assert_eq!(prefs.placement("c1"), Placement::Default);
        prefs.toggle_hidden("c1");
        assert!(prefs.hidden.is_empty());
    }

    #[test]
    fn teams_file_into_sections_like_conversations() {
        let mut prefs = Prefs::default();
        let team = team_key("t1");
        prefs.add_section("Customers", Some(&team));
        assert_eq!(prefs.placement(&team), Placement::Section(0));
        prefs.remove_from_section(&team);
        assert_eq!(prefs.placement(&team), Placement::Default);
    }

    #[test]
    fn blank_section_names_are_ignored() {
        let mut prefs = Prefs::default();
        prefs.add_section("   ", Some("c1"));
        assert!(prefs.sections.is_empty());
    }

    #[test]
    fn old_files_missing_fields_still_load() {
        let prefs: Prefs = serde_json::from_str(r#"{"starred":["a"]}"#).expect("loads");
        assert_eq!(prefs.starred, ["a"]);
        assert!(!prefs.unreads_only);
    }
}
