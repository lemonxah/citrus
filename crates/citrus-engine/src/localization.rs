//! Localization / string tables (ENGINE_FEATURE_CHECKLIST T1 #32).
//!
//! A `.loc` asset (RON) maps string KEYS to a per-language list of translations.
//! Games look strings up by key via [`Localization::tr`], which returns the current
//! language's text and falls back to the key itself when a translation is missing —
//! so a missing entry is visible in-game (the key shows) rather than blank.
//!
//! Format (`assets/strings.loc`):
//! ```ron
//! Localization(
//!     languages: ["en", "fr", "es"],
//!     entries: {
//!         "menu.play": ["Play", "Jouer", "Jugar"],
//!         "menu.quit": ["Quit", "Quitter", "Salir"],
//!     },
//! )
//! ```

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// On-disk + runtime string table. The current language is an index into
/// `languages`; `tr` reads the matching column of each entry.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Localization {
    /// Language codes in column order, e.g. `["en", "fr"]`. The first is the
    /// default / fallback language.
    pub languages: Vec<String>,
    /// key -> one translation per language (parallel to `languages`).
    pub entries: HashMap<String, Vec<String>>,
    /// Active language index (not serialized — a runtime/user setting).
    #[serde(skip)]
    current: usize,
}

impl Localization {
    /// Parse a `.loc` RON string.
    pub fn from_ron(s: &str) -> Result<Self, ron::error::SpannedError> {
        let mut loc: Self = ron::from_str(s)?;
        loc.current = 0;
        Ok(loc)
    }

    /// Load a `.loc` file from disk.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(Self::from_ron(&s)?)
    }

    /// Serialize to RON (pretty) for the editor's string-table editor / export.
    pub fn to_ron(&self) -> String {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())
            .unwrap_or_default()
    }

    /// Available language codes.
    pub fn languages(&self) -> &[String] {
        &self.languages
    }

    /// The active language code (empty if none configured).
    pub fn current_language(&self) -> &str {
        self.languages.get(self.current).map(String::as_str).unwrap_or("")
    }

    /// Index of the active language.
    pub fn current_index(&self) -> usize {
        self.current
    }

    /// Switch language by code; returns false if the code isn't configured.
    pub fn set_language(&mut self, lang: &str) -> bool {
        match self.languages.iter().position(|l| l == lang) {
            Some(i) => {
                self.current = i;
                true
            }
            None => false,
        }
    }

    /// Switch language by index (clamped to the configured set).
    pub fn set_language_index(&mut self, i: usize) {
        if i < self.languages.len() {
            self.current = i;
        }
    }

    /// Translate `key` in the active language. Falls back, in order, to the
    /// default (first) language, then to the key itself — so nothing ever renders
    /// blank and an untranslated string is obvious in-game.
    pub fn tr<'a>(&'a self, key: &'a str) -> &'a str {
        if let Some(cols) = self.entries.get(key) {
            if let Some(s) = cols.get(self.current).filter(|s| !s.is_empty()) {
                return s;
            }
            // Fall back to the default language column.
            if let Some(s) = cols.first().filter(|s| !s.is_empty()) {
                return s;
            }
        }
        key
    }

    /// Translate `key` in a specific language code, or `None` if absent.
    pub fn tr_lang(&self, key: &str, lang: &str) -> Option<&str> {
        let col = self.languages.iter().position(|l| l == lang)?;
        self.entries.get(key)?.get(col).filter(|s| !s.is_empty()).map(String::as_str)
    }

    /// Add / overwrite an entry (editor authoring). Pads/truncates the translation
    /// list to match the language count.
    pub fn set_entry(&mut self, key: impl Into<String>, mut translations: Vec<String>) {
        translations.resize(self.languages.len().max(1), String::new());
        self.entries.insert(key.into(), translations);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"Localization(
        languages: ["en", "fr", "es"],
        entries: {
            "menu.play": ["Play", "Jouer", "Jugar"],
            "menu.quit": ["Quit", "Quitter", ""],
        },
    )"#;

    #[test]
    fn parses_and_translates_current_language() {
        let mut loc = Localization::from_ron(SAMPLE).unwrap();
        assert_eq!(loc.current_language(), "en");
        assert_eq!(loc.tr("menu.play"), "Play");
        assert!(loc.set_language("fr"));
        assert_eq!(loc.tr("menu.play"), "Jouer");
        assert_eq!(loc.tr("menu.quit"), "Quitter");
    }

    #[test]
    fn falls_back_to_default_then_key() {
        let mut loc = Localization::from_ron(SAMPLE).unwrap();
        loc.set_language("es");
        // "menu.quit" has an empty Spanish cell -> falls back to the default (en).
        assert_eq!(loc.tr("menu.quit"), "Quit");
        // Unknown key returns the key itself (never blank).
        assert_eq!(loc.tr("does.not.exist"), "does.not.exist");
    }

    #[test]
    fn unknown_language_is_rejected_and_lookup_by_lang_works() {
        let mut loc = Localization::from_ron(SAMPLE).unwrap();
        assert!(!loc.set_language("de"));
        assert_eq!(loc.current_language(), "en"); // unchanged
        assert_eq!(loc.tr_lang("menu.play", "es"), Some("Jugar"));
        assert_eq!(loc.tr_lang("menu.quit", "es"), None); // empty cell
    }

    #[test]
    fn authoring_pads_translations_to_language_count() {
        let mut loc = Localization::from_ron(SAMPLE).unwrap();
        loc.set_entry("menu.options", vec!["Options".into()]);
        // Padded to 3 languages; missing ones fall back to the en cell.
        loc.set_language("fr");
        assert_eq!(loc.tr("menu.options"), "Options");
        assert_eq!(loc.entries["menu.options"].len(), 3);
    }

    #[test]
    fn ron_round_trips() {
        let loc = Localization::from_ron(SAMPLE).unwrap();
        let s = loc.to_ron();
        let back = Localization::from_ron(&s).unwrap();
        assert_eq!(back.languages, loc.languages);
        assert_eq!(back.tr_lang("menu.play", "fr"), Some("Jouer"));
    }
}
