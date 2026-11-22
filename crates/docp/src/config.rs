//! Configuration: optional TOML file plus command-line overrides.
//!
//! Lookup order (first existing wins):
//!   1. `--config PATH`
//!   2. `$DOCP_CONFIG`
//!   3. `$XDG_CONFIG_HOME/docp/config.toml`
//!   4. `~/.config/docp/config.toml`
//!
//! ```toml
//! [theme]
//! name = "terminal"          # terminal | dark | light
//!
//! [fonts]
//! sans = "Inter"             # family for headings / pixel text
//! mono = "JetBrains Mono"    # family for code and diagram labels
//! sans_file = ""             # optional font file, overrides `sans`
//! mono_file = ""
//!
//! [layout]
//! max_width = 110            # clamp of the text column width in cells
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub theme: ThemeSection,
    pub fonts: FontsSection,
    pub layout: LayoutSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeSection {
    /// `terminal`, `dark` or `light`.
    pub name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontsSection {
    pub sans: Option<String>,
    pub mono: Option<String>,
    pub emoji: Option<String>,
    pub sans_file: Option<String>,
    pub mono_file: Option<String>,
    pub emoji_file: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutSection {
    pub max_width: usize,
}

impl Default for LayoutSection {
    fn default() -> Self {
        Self { max_width: 110 }
    }
}

impl Config {
    /// Load from `explicit` (or the standard locations). A missing file is not
    /// an error; a malformed one is, so typos do not silently do nothing.
    pub fn load(explicit: Option<&Path>) -> Result<Config> {
        let Some(path) = explicit.map(Path::to_path_buf).or_else(default_path) else {
            return Ok(Config::default());
        };
        if !path.exists() {
            if explicit.is_some() {
                anyhow::bail!("config file not found: {}", path.display());
            }
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))
    }

    /// The file the loader would use and whether it exists, for `--info`.
    pub fn source(explicit: Option<&Path>) -> Option<(PathBuf, bool)> {
        let path = explicit.map(Path::to_path_buf).or_else(default_path)?;
        let exists = path.exists();
        Some((path, exists))
    }

    /// Apply command-line overrides on top of the file.
    pub fn apply_cli(&mut self, cli: &CliOverrides) {
        if let Some(t) = &cli.theme {
            self.theme.name = Some(t.clone());
        }
        if let Some(f) = &cli.font_sans {
            self.fonts.sans = Some(f.clone());
            self.fonts.sans_file = None;
        }
        if let Some(f) = &cli.font_mono {
            self.fonts.mono = Some(f.clone());
            self.fonts.mono_file = None;
        }
        if let Some(p) = &cli.font_sans_file {
            self.fonts.sans_file = Some(p.clone());
        }
        if let Some(p) = &cli.font_mono_file {
            self.fonts.mono_file = Some(p.clone());
        }
        if let Some(f) = &cli.font_emoji {
            self.fonts.emoji = Some(f.clone());
            self.fonts.emoji_file = None;
        }
        if let Some(p) = &cli.font_emoji_file {
            self.fonts.emoji_file = Some(p.clone());
        }
        if let Some(w) = cli.max_width {
            self.layout.max_width = w;
        }
    }

    pub fn font_spec(&self) -> render_md::fonts::FontSpec {
        render_md::fonts::FontSpec {
            sans: self.fonts.sans.clone(),
            mono: self.fonts.mono.clone(),
            sans_file: self.fonts.sans_file.clone().filter(|s| !s.is_empty()),
            mono_file: self.fonts.mono_file.clone().filter(|s| !s.is_empty()),
            emoji: self.fonts.emoji.clone(),
            emoji_file: self.fonts.emoji_file.clone().filter(|s| !s.is_empty()),
        }
    }
}

/// Command-line values that override the file (kept separate so `Config`
/// stays a pure description of the file).
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub theme: Option<String>,
    pub font_sans: Option<String>,
    pub font_mono: Option<String>,
    pub font_sans_file: Option<String>,
    pub font_mono_file: Option<String>,
    pub font_emoji: Option<String>,
    pub font_emoji_file: Option<String>,
    pub max_width: Option<usize>,
}

fn default_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DOCP_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("docp").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let c: Config = toml::from_str(
            r#"
[theme]
name = "light"

[fonts]
sans = "SF Pro Text"
mono = "Menlo"
sans_file = "/tmp/x.ttf"

[layout]
max_width = 90
"#,
        )
        .unwrap();
        assert_eq!(c.theme.name.as_deref(), Some("light"));
        assert_eq!(c.fonts.sans.as_deref(), Some("SF Pro Text"));
        assert_eq!(c.fonts.sans_file.as_deref(), Some("/tmp/x.ttf"));
        assert_eq!(c.layout.max_width, 90);
    }

    #[test]
    fn empty_config_uses_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.layout.max_width, 110);
        assert!(c.theme.name.is_none());
    }

    #[test]
    fn unknown_keys_are_errors() {
        // A typo must be loud, not silently ignored.
        assert!(toml::from_str::<Config>("[fonts]\nsnas = \"x\"\n").is_err());
    }

    #[test]
    fn cli_overrides_file() {
        let mut c: Config = toml::from_str("[fonts]\nsans = \"Inter\"\n").unwrap();
        c.apply_cli(&CliOverrides {
            font_sans: Some("Menlo".into()),
            ..Default::default()
        });
        assert_eq!(c.fonts.sans.as_deref(), Some("Menlo"));
        // choosing a family clears a file that would otherwise win
        c.apply_cli(&CliOverrides {
            font_sans: Some("Atkinson".into()),
            ..Default::default()
        });
        assert!(c.fonts.sans_file.is_none());
    }
}
