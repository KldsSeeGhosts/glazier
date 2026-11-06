//! User settings stored in `%APPDATA%\Glazier\config.toml`.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub hotkeys: Vec<String>,
    pub recording_hotkey: String,
    pub recording_directory: String,
    pub recording_fps: u32,
    pub save_directory: String,
    pub filename_format: String,
    pub default_tool: String,
    pub default_color: String,
    pub default_size: f32,
    pub show_magnifier: bool,
    pub show_tool_options: bool,
    pub toolbar_theme: String,
    pub theme_file: String,
    pub show_thumbnail: bool,
    pub thumbnail_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkeys: vec!["PrintScreen".into(), "Win+Shift+S".into()],
            recording_hotkey: "Win+Shift+R".into(),
            recording_directory: String::new(),
            recording_fps: 30,
            save_directory: String::new(),
            filename_format: String::new(),
            default_tool: "pencil".into(),
            default_color: "#FF4444".into(),
            default_size: 3.0,
            show_magnifier: true,
            show_tool_options: true,
            toolbar_theme: "auto".into(),
            theme_file: String::new(),
            show_thumbnail: true,
            thumbnail_seconds: 6,
        }
    }
}

const TEMPLATE: &str = r##"# Glazier settings. Choose "Reload settings" from the tray menu after editing.

# Shortcuts that freeze all monitors and open the region picker.
# Modifiers: Ctrl, Alt, Shift, Win. Keys: A-Z, 0-9, F1-F24, PrintScreen, Insert,
# Delete, Home, End, PageUp, PageDown, Pause, ScrollLock, Space.
# "Win+Shift+S" and "PrintScreen" take over the Windows Snipping Tool shortcuts.
hotkeys = ["PrintScreen", "Win+Shift+S"]

# Separate recording picker. Press again while recording to stop.
recording_hotkey = "Win+Shift+R"
# Empty uses Videos\Screen Recordings. Environment variables are expanded.
recording_directory = ""
# Frames per second, from 1 to 60.
recording_fps = 30

# Folder for saved annotations. Empty uses Pictures\Screenshots, then Pictures.
save_directory = ""

# File name without extension. %Y %m %d %H %M %S expand to the current time.
# Empty uses annotate-%Y-%m-%d_%H-%M-%S.
filename_format = ""

# pencil, highlighter, line, arrow, rect, circle, text, blur, step or ruler.
default_tool = "pencil"
default_color = "#FF4444"
default_size = 3

# Show the zoom control on the selection and the color/size button in the toolbar.
show_magnifier = true
show_tool_options = true

# Toolbar appearance: "auto" follows the Windows app theme, or "dark" / "light".
toolbar_theme = "auto"

# Optional absolute path to a versioned theme-token JSON file.
theme_file = ""

# Floating exported-image previews. Hover pauses dismissal; 0 stays until closed.
show_thumbnail = true
thumbnail_seconds = 6
"##;

pub fn dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("Glazier")
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

/// Loads settings, creating the commented template on first run.
/// Returns the defaults and an error message when the file is invalid.
pub fn load() -> (Config, Option<String>) {
    let path = path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(format!("config.toml: {}", e.message()))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::create_dir_all(dir());
            let _ = std::fs::write(&path, TEMPLATE);
            (Config::default(), None)
        }
        Err(e) => (Config::default(), Some(format!("Could not read config.toml: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_matches_defaults() {
        let cfg: Config = toml::from_str(TEMPLATE).unwrap();
        let def = Config::default();
        assert_eq!(cfg.hotkeys, def.hotkeys);
        assert_eq!(cfg.recording_hotkey, def.recording_hotkey);
        assert_eq!(cfg.recording_fps, def.recording_fps);
        assert_eq!(cfg.default_tool, def.default_tool);
        assert_eq!(cfg.default_size, def.default_size);
        assert_eq!(cfg.toolbar_theme, def.toolbar_theme);
        assert_eq!(cfg.theme_file, def.theme_file);
        assert_eq!(cfg.show_thumbnail, def.show_thumbnail);
        assert_eq!(cfg.thumbnail_seconds, def.thumbnail_seconds);
    }
}
