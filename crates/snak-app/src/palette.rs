//! The command palette (⌘⇧P), Zed-style: one fuzzy list of commands,
//! conversations and themes.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, IndexPath, WindowExt as _,
    command::{Command, CommandGroup, CommandItem},
    h_flex,
};
use gpui_kit::*;
use snak_chat::ConversationId;

use crate::app::Snak;
use crate::settings::{Appearance, Settings};
use crate::theme::InstalledThemes;
use crate::*;

#[derive(Clone)]
enum Entry {
    Action(Rc<dyn Fn() -> Box<dyn Action>>),
    Conversation(ConversationId),
    Theme { name: String, dark: bool },
}

fn command(label: &str, keywords: &[&str], action: impl Action + Clone) -> (CommandItem, Entry) {
    let item = CommandItem::new()
        .label(label.to_string())
        .keywords(keywords.iter().map(|k| k.to_string()))
        .action(action.boxed_clone());
    (item, Entry::Action(Rc::new(move || action.boxed_clone())))
}

fn commands() -> Vec<(CommandItem, Entry)> {
    vec![
        command("Jump to conversation", &["go", "switch", "search"], JumpTo),
        command("Show all unreads", &["inbox"], ShowUnreads),
        command("Next conversation", &["down"], NextConversation),
        command("Previous conversation", &["up"], PreviousConversation),
        command("Close thread", &["panel"], CloseThread),
        command("Toggle unreads only", &["filter"], ToggleUnreadsOnly),
        command(
            "Star or unstar conversation",
            &["favorite", "pin"],
            ToggleStar,
        ),
        command("Mute or unmute conversation", &["silence"], ToggleMute),
        command("Hide or show conversation", &["remove"], ToggleHide),
        command("Collapse all groups", &["fold"], CollapseAll),
        command("Expand all groups", &["unfold"], ExpandAll),
        command(
            "Cycle theme mode",
            &["dark", "light", "appearance"],
            CycleTheme,
        ),
        command("Open settings", &["preferences", "config"], OpenSettings),
        command("Sign out", &["logout"], SignOut),
        command("Quit Snak", &["exit"], Quit),
    ]
}

impl Snak {
    pub fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut groups: Vec<(&str, Vec<(CommandItem, Entry)>)> = vec![("Commands", commands())];

        let conversations = self
            .conversations()
            .iter()
            .map(|c| {
                let label = match c.group() {
                    Some(group) => format!("#{}  ·  {}", c.title, group.name),
                    None => c.title.clone(),
                };
                let mut keywords = vec![c.title.clone()];
                keywords.extend(c.group().map(|g| g.name.clone()));
                (
                    CommandItem::new().label(label).keywords(keywords),
                    Entry::Conversation(c.id.clone()),
                )
            })
            .collect();
        groups.push(("Conversations", conversations));

        let themes = cx
            .global::<InstalledThemes>()
            .themes
            .iter()
            .map(|(name, dark)| {
                (
                    CommandItem::new()
                        .label(format!("Theme: {name}"))
                        .keywords(["theme".to_string(), name.clone()]),
                    Entry::Theme {
                        name: name.clone(),
                        dark: *dark,
                    },
                )
            })
            .collect();
        groups.push(("Themes", themes));

        let mut sections: Vec<(String, Vec<CommandItem>)> = Vec::new();
        let mut entries: Vec<Vec<Entry>> = Vec::new();
        for (label, items) in groups {
            let (items, section): (Vec<CommandItem>, Vec<Entry>) = items.into_iter().unzip();
            sections.push((label.to_string(), items));
            entries.push(section);
        }
        let (sections, entries) = (Rc::new(sections), Rc::new(entries));

        let palette = self.palette.clone();
        palette.update(cx, |state, cx| state.set_query("", window, cx));
        let owner = cx.weak_entity();
        let focus_on_mount = Rc::new(Cell::new(true));
        window.open_dialog(cx, move |dialog, _, _| {
            let palette = palette.clone();
            let owner = owner.clone();
            let entries = entries.clone();
            let sections = sections.clone();
            let focus_on_mount = focus_on_mount.clone();
            dialog
                .close_button(false)
                .p_0()
                .w(px(560.))
                .content(move |content, window, cx| {
                    if focus_on_mount.replace(false) {
                        let palette = palette.clone();
                        window.defer(cx, move |window, cx| {
                            palette.read(cx).focus_handle(cx).focus(window, cx);
                        });
                    }
                    // The dialog re-renders its content, so the list is rebuilt each time.
                    let command =
                        sections
                            .iter()
                            .fold(Command::new(&palette), |command, (label, items)| {
                                command.group(
                                    CommandGroup::new()
                                        .label(label.clone())
                                        .items(items.clone()),
                                )
                            });
                    let owner = owner.clone();
                    let entries = entries.clone();
                    let rebuilt = command
                        .bordered(false)
                        .max_h(px(420.))
                        .placeholder("Type a command, conversation or theme…")
                        .on_confirm(move |index: IndexPath, window, cx| {
                            let Some(entry) = entries
                                .get(index.section)
                                .and_then(|section| section.get(index.row))
                                .cloned()
                            else {
                                return;
                            };
                            window.close_dialog(cx);
                            owner
                                .update(cx, |snak, cx| snak.run_palette_entry(entry, window, cx))
                                .ok();
                        })
                        .footer(|state, _, cx| {
                            h_flex()
                                .px_3()
                                .py_1p5()
                                .border_t_1()
                                .border_color(cx.theme().border)
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("{} results", state.matched_count()))
                        });
                    content.child(rebuilt)
                })
        });
    }

    fn run_palette_entry(&mut self, entry: Entry, window: &mut Window, cx: &mut Context<Self>) {
        match entry {
            // The palette dispatches from its dialog, which sits beside this
            // view rather than inside it, so view actions are re-sent from here.
            Entry::Action(action) => {
                self.focus_main(window, cx);
                window.dispatch_action(action(), cx);
            }
            Entry::Conversation(id) => self.select(id, window, cx),
            Entry::Theme { name, dark } => {
                let current_is_dark = cx.theme().is_dark();
                Settings::update(cx, |s| {
                    if dark {
                        s.dark_theme = Some(name);
                    } else {
                        s.light_theme = Some(name);
                    }
                    // Picking a theme of the other mode should show it, not file it away.
                    if dark != current_is_dark {
                        s.appearance = if dark {
                            Appearance::Dark
                        } else {
                            Appearance::Light
                        };
                    }
                });
            }
        }
    }
}
