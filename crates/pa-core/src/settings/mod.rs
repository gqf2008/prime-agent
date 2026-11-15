//! Settings subsystem: typed settings documents, global/project storage,
//! legacy migrations, and the manager.

pub(crate) mod interactive_settings;
pub(crate) mod load;
pub(crate) mod manager;
pub(crate) mod merge;
pub(crate) mod storage;
pub(crate) mod types;

#[cfg(test)]
mod persistence_tests;

pub use manager::{IdleEviction, SettingsError, SettingsManager, DEFAULT_IDLE_EVICTION_MINUTES};
pub use storage::{
    FileSettingsStorage, InMemorySettingsStorage, SettingsScope, SettingsStorage, CONFIG_DIR_NAME,
};
pub use types::{
    AutoRefineSettings, AutonomousSettings, CompactionSettings, QueueModeSetting, Settings,
    ThinkingLevelSetting, TransportSetting, UpdateChannel, WebSearchProvider,
};

/// The default theme name when settings carry none: a light terminal
/// background (`COLORFGBG`) selects the light theme, everything else gets
/// Prime. Shared by [`SettingsManager::theme_name`] (the composition root's
/// TUI theme resolution) and the session HTML exporter so both surfaces agree
/// on the default — a dark palette on a light terminal paints the editor
/// surface and the echoed user block as dark bands on a light page.
#[must_use]
pub(crate) fn default_theme_name() -> String {
    if detect_light_terminal_background(std::env::var("COLORFGBG").ok().as_deref()) {
        "light".to_string()
    } else {
        "prime".to_string()
    }
}

/// `COLORFGBG` detection: the variable is `fg;bg` or the three-field
/// `fg;default;bg` form (urxvt), so the background slot is the last field.
/// Slots 0-6 and 8 (bright black, a dark gray) are dark backgrounds; slot 7
/// (white) and 9-15 (the bright colors) are light.
fn detect_light_terminal_background(value: Option<&str>) -> bool {
    let Some(value) = value else { return false };
    let Some(background) = value
        .trim_end_matches(';')
        .rsplit_once(';')
        .and_then(|(_, background)| background.parse::<u64>().ok())
    else {
        return false;
    };
    background == 7 || background >= 9
}

#[cfg(test)]
mod default_theme_tests {
    use super::*;

    #[test]
    fn a_light_terminal_background_selects_the_light_theme() {
        assert!(!detect_light_terminal_background(Some("15;0")));
        assert!(detect_light_terminal_background(Some("0;15")));
        // The three-field urxvt form keeps the background last.
        assert!(detect_light_terminal_background(Some("0;default;15")));
        assert!(!detect_light_terminal_background(Some("0;default;8")));
        // Slot 7 is white (light); slot 8 is bright black (a dark gray).
        assert!(detect_light_terminal_background(Some("15;7")));
        assert!(!detect_light_terminal_background(Some("15;8")));
        assert!(!detect_light_terminal_background(None));
        assert!(!detect_light_terminal_background(Some("bogus")));
    }
}
