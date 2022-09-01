mod folder_view;
mod modcards56_view;
mod navicust_view;

use fluent_templates::Loader;

use crate::{game, gui, i18n, rom, save};

#[derive(PartialEq, Clone)]
enum Tab {
    Navicust,
    Folder,
    Modcards,
}

pub struct State {
    tab: Option<Tab>,
    navicust_view: navicust_view::State,
    folder_view: folder_view::State,
    modcards56_view: modcards56_view::State,
    texture_cache:
        std::collections::HashMap<(gui::save_view::CachedAssetType, usize), egui::TextureHandle>,
}

impl State {
    pub fn new() -> Self {
        Self {
            tab: None,
            navicust_view: navicust_view::State::new(),
            folder_view: folder_view::State::new(),
            modcards56_view: modcards56_view::State::new(),
            texture_cache: std::collections::HashMap::new(),
        }
    }
}

pub struct SaveView {
    navicust_view: navicust_view::NavicustView,
    folder_view: folder_view::FolderView,
    modcards56_view: modcards56_view::Modcards56View,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CachedAssetType {
    ChipIcon,
    ElementIcon,
}

impl SaveView {
    pub fn new() -> Self {
        Self {
            navicust_view: navicust_view::NavicustView::new(),
            folder_view: folder_view::FolderView::new(),
            modcards56_view: modcards56_view::Modcards56View::new(),
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        clipboard: &mut arboard::Clipboard,
        font_families: &gui::FontFamilies,
        lang: &unic_langid::LanguageIdentifier,
        game: &'static (dyn game::Game + Send + Sync),
        save: &Box<dyn save::Save + Send + Sync>,
        assets: &Box<dyn rom::Assets + Send + Sync>,
        state: &mut State,
    ) {
        let navicust_view = save.view_navicust();
        let chips_view = save.view_chips();
        let modcards56_view = save.view_modcards56();

        let mut available_tabs = vec![];
        if navicust_view.is_some() {
            available_tabs.push(Tab::Navicust);
        }
        if chips_view.is_some() {
            available_tabs.push(Tab::Folder);
        }
        if modcards56_view.is_some() {
            available_tabs.push(Tab::Modcards);
        }

        ui.horizontal(|ui| {
            for tab in available_tabs.iter() {
                if ui
                    .selectable_label(
                        state.tab.as_ref() == Some(tab),
                        i18n::LOCALES
                            .lookup(
                                lang,
                                match tab {
                                    Tab::Navicust => "save.navicust",
                                    Tab::Folder => "save.folder",
                                    Tab::Modcards => "save.modcards",
                                },
                            )
                            .unwrap(),
                    )
                    .clicked()
                {
                    state.tab = Some(tab.clone());
                }
            }
        });

        if state.tab.is_none() {
            state.tab = available_tabs.first().cloned();
        }

        match state.tab {
            Some(Tab::Navicust) => {
                if let Some(navicust_view) = navicust_view {
                    self.navicust_view.show(
                        ui,
                        clipboard,
                        font_families,
                        lang,
                        game,
                        &navicust_view,
                        assets,
                        &mut state.navicust_view,
                    );
                }
            }
            Some(Tab::Folder) => {
                if let Some(chips_view) = chips_view {
                    self.folder_view.show(
                        ui,
                        clipboard,
                        font_families,
                        lang,
                        game,
                        &chips_view,
                        assets,
                        &mut state.texture_cache,
                        &mut state.folder_view,
                    );
                }
            }
            Some(Tab::Modcards) => {
                if let Some(modcards56_view) = modcards56_view {
                    self.modcards56_view.show(
                        ui,
                        clipboard,
                        font_families,
                        lang,
                        game,
                        &modcards56_view,
                        assets,
                        &mut state.modcards56_view,
                    );
                }
            }
            None => {}
        }
    }
}
