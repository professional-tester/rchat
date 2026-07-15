use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
struct GhosttyLocations {
    xdg_config_home: PathBuf,
    macos_config_dir: Option<PathBuf>,
    bundled_theme_dirs: Vec<PathBuf>,
}

impl GhosttyLocations {
    fn from_environment() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set; cannot locate Ghostty configuration")?;
        let xdg_config_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        #[cfg(target_os = "macos")]
        let (macos_config_dir, bundled_theme_dirs) = (
            Some(home.join("Library/Application Support/com.mitchellh.ghostty")),
            vec![
                PathBuf::from("/Applications/Ghostty.app/Contents/Resources/ghostty/themes"),
                home.join("Applications/Ghostty.app/Contents/Resources/ghostty/themes"),
            ],
        );
        #[cfg(not(target_os = "macos"))]
        let (macos_config_dir, bundled_theme_dirs) = (None, Vec::new());
        Ok(Self {
            xdg_config_home,
            macos_config_dir,
            bundled_theme_dirs,
        })
    }

    fn config_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![
            self.xdg_config_home.join("ghostty/config.ghostty"),
            self.xdg_config_home.join("ghostty/config"),
        ];
        if let Some(directory) = &self.macos_config_dir {
            roots.extend([directory.join("config.ghostty"), directory.join("config")]);
        }
        roots
    }
}

#[derive(Debug, Clone)]
struct GhosttyEntry {
    key: String,
    value: String,
    source: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct GhosttyValues {
    entries: Vec<GhosttyEntry>,
}

impl GhosttyValues {
    fn last_entry(&self, key: &str) -> Option<&GhosttyEntry> {
        self.entries.iter().rev().find(|entry| entry.key == key)
    }

    fn last(&self, key: &str) -> Option<&str> {
        self.last_entry(key).map(|entry| entry.value.as_str())
    }

    fn all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> {
        self.entries
            .iter()
            .filter(move |entry| entry.key == key)
            .map(|entry| entry.value.as_str())
    }
}

#[derive(Debug)]
struct LoadedGhostty {
    values: GhosttyValues,
    source_files: Vec<PathBuf>,
}

fn load_ghostty_values(locations: &GhosttyLocations) -> Result<LoadedGhostty> {
    let mut loaded = LoadedGhostty {
        values: GhosttyValues::default(),
        source_files: Vec::new(),
    };
    let mut active = HashSet::new();
    for root in locations.config_roots() {
        if root.is_file() {
            load_ghostty_file(&root, &mut loaded, &mut active)?;
        }
    }
    if loaded.source_files.is_empty() {
        bail!("Ghostty configuration was not found in any supported location");
    }
    Ok(loaded)
}

fn load_ghostty_file(
    path: &Path,
    loaded: &mut LoadedGhostty,
    active: &mut HashSet<PathBuf>,
) -> Result<()> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve Ghostty config {}", path.display()))?;
    if !active.insert(canonical.clone()) {
        bail!("Ghostty config include cycle detected at {}", path.display());
    }
    loaded.source_files.push(canonical.clone());
    let text = fs::read_to_string(&canonical)
        .with_context(|| format!("failed to read Ghostty config {}", canonical.display()))?;
    let mut entries = Vec::new();
    let mut includes = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let Some((key, value)) = parse_ghostty_line(line).with_context(|| {
            format!(
                "malformed Ghostty config at {}:{}",
                canonical.display(),
                line_index + 1
            )
        })? else {
            continue;
        };
        if key == "config-file" {
            includes.push(value);
        } else {
            entries.push(GhosttyEntry {
                key,
                value,
                source: canonical.clone(),
            });
        }
    }
    loaded.values.entries.extend(entries);
    for include in includes {
        let (optional, include) = include
            .strip_prefix('?')
            .map_or((false, include.as_str()), |path| (true, path));
        let include = PathBuf::from(include);
        let include = if include.is_absolute() {
            include
        } else {
            canonical.parent().unwrap_or(Path::new(".")).join(include)
        };
        if optional && !include.is_file() {
            continue;
        }
        load_ghostty_file(&include, loaded, active)?;
    }
    active.remove(&canonical);
    Ok(())
}

fn parse_ghostty_line(line: &str) -> Result<Option<(String, String)>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    let separator = find_unquoted(trimmed, '=')
        .context("expected a `key = value` assignment")?;
    let key = trimmed[..separator].trim();
    if key.is_empty() {
        bail!("configuration key is empty");
    }
    let value = strip_unquoted_trailing_comment(trimmed[separator + 1..].trim());
    Ok(Some((key.to_string(), remove_outer_quotes(value).to_string())))
}

fn find_unquoted(value: &str, needle: char) -> Option<usize> {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if quote.is_none() && character == needle {
            return Some(index);
        }
    }
    None
}

fn strip_unquoted_trailing_comment(value: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    let mut seen_non_whitespace = false;
    let mut previous_whitespace = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
        } else if quote.is_none()
            && character == '#'
            && seen_non_whitespace
            && previous_whitespace
        {
            return value[..index].trim_end();
        }
        if !character.is_whitespace() {
            seen_non_whitespace = true;
        }
        previous_whitespace = character.is_whitespace();
    }
    value.trim_end()
}

fn remove_outer_quotes(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn selected_dark_theme(value: &str) -> &str {
    if value.contains(',') {
        for candidate in value.split(',').map(str::trim) {
            if let Some(dark) = candidate.strip_prefix("dark:") {
                return dark.trim();
            }
        }
    }
    value.trim()
}

fn resolve_theme_path(
    name: &str,
    loaded: &LoadedGhostty,
    locations: &GhosttyLocations,
) -> Option<PathBuf> {
    let requested = PathBuf::from(name);
    if requested.is_absolute() && requested.is_file() {
        return Some(requested);
    }
    if let Some(source) = loaded.values.last_entry("theme").map(|entry| &entry.source) {
        if let Some(directory) = source.parent() {
            let candidate = directory.join("themes").join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for source in loaded.source_files.iter().rev() {
        if let Some(directory) = source.parent() {
            let candidate = directory.join("themes").join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let xdg = locations.xdg_config_home.join("ghostty/themes").join(name);
    if xdg.is_file() {
        return Some(xdg);
    }
    locations
        .bundled_theme_dirs
        .iter()
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GhosttyImportMetadata {
    pub source_files: Vec<PathBuf>,
    pub theme: Option<String>,
}

#[derive(Debug)]
pub struct GhosttyImportResult {
    pub config_path: PathBuf,
    pub metadata: GhosttyImportMetadata,
}

#[derive(Debug, Default, Serialize)]
struct ManagedRattyConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    window: Option<ManagedWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    font: Option<ManagedFont>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<ManagedTheme>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bindings: Option<ManagedBindings>,
}

#[derive(Debug, Serialize)]
struct ManagedWindow {
    opacity: f64,
}

#[derive(Debug, Default, Serialize)]
struct ManagedFont {
    #[serde(skip_serializing_if = "Option::is_none")]
    family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    style: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<i32>,
}

#[derive(Debug, Default, Serialize)]
struct ManagedTheme {
    #[serde(skip_serializing_if = "Option::is_none")]
    foreground: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    normal: Option<ManagedPalette>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bright: Option<ManagedPalette>,
}

#[derive(Debug, Serialize)]
struct ManagedPalette {
    black: String,
    red: String,
    green: String,
    yellow: String,
    blue: String,
    magenta: String,
    cyan: String,
    white: String,
}

#[derive(Debug, Serialize)]
struct ManagedBindings {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    keys: Vec<ManagedBinding>,
}

#[derive(Debug, Serialize)]
struct ManagedBinding {
    key: String,
    with: String,
    action: String,
}

fn translated_config(values: &GhosttyValues) -> Result<ManagedRattyConfig> {
    let family = first_font_family_after_last_reset(values);
    let style = values.last("font-style").and_then(translate_font_style);
    let size = values
        .last("font-size")
        .map(|value| -> Result<i32> {
            let size: f64 = value
                .parse()
                .with_context(|| format!("invalid Ghostty font size `{value}`"))?;
            let rounded = size.round();
            if !(1.0..=96.0).contains(&rounded) {
                bail!("Ghostty font size must be between 1 and 96, got {value}");
            }
            Ok(rounded as i32)
        })
        .transpose()?;
    let font = (family.is_some() || style.is_some() || size.is_some()).then_some(ManagedFont {
        family,
        style,
        size,
    });

    let opacity = values
        .last("background-opacity")
        .map(|value| -> Result<f64> {
            let opacity: f64 = value
                .parse()
                .with_context(|| format!("invalid Ghostty background opacity `{value}`"))?;
            Ok(opacity.clamp(0.0, 1.0))
        })
        .transpose()?;
    let foreground = values
        .last("foreground")
        .map(normalize_color)
        .transpose()?;
    let background = values
        .last("background")
        .map(normalize_color)
        .transpose()?;
    let cursor = values
        .last("cursor-color")
        .map(normalize_color)
        .transpose()?;
    let (normal, bright) = translated_palettes(values)?;
    let theme = (foreground.is_some()
        || background.is_some()
        || cursor.is_some()
        || normal.is_some()
        || bright.is_some())
    .then_some(ManagedTheme {
        foreground,
        background,
        cursor,
        normal,
        bright,
    });
    let keys = values
        .all("keybind")
        .filter_map(translate_binding)
        .collect::<Vec<_>>();

    Ok(ManagedRattyConfig {
        window: opacity.map(|opacity| ManagedWindow { opacity }),
        font,
        theme,
        bindings: (!keys.is_empty()).then_some(ManagedBindings { keys }),
    })
}

fn first_font_family_after_last_reset(values: &GhosttyValues) -> Option<String> {
    let mut family = None;
    for value in values.all("font-family") {
        if value.is_empty() {
            family = None;
        } else if family.is_none() {
            family = Some(value.to_string());
        }
    }
    family
}

fn translate_font_style(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let bold = lower.contains("bold");
    let italic = lower.contains("italic");
    match (bold, italic) {
        (true, true) => Some("BoldItalic".to_string()),
        (true, false) => Some("Bold".to_string()),
        (false, true) => Some("Italic".to_string()),
        (false, false) if lower.contains("regular") || lower.contains("normal") => {
            Some("Regular".to_string())
        }
        _ => None,
    }
}

fn normalize_color(value: &str) -> Result<String> {
    let hex = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid Ghostty color `{value}`; expected #RRGGBB");
    }
    Ok(format!("#{}", hex.to_ascii_lowercase()))
}

fn translated_palettes(
    values: &GhosttyValues,
) -> Result<(Option<ManagedPalette>, Option<ManagedPalette>)> {
    const DEFAULTS: [&str; 16] = [
        "#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd",
        "#e5e5e5", "#666666", "#f14c4c", "#23d18b", "#f5f543", "#3b8eea", "#d670d6",
        "#29b8db", "#ffffff",
    ];
    let mut colors = DEFAULTS.map(str::to_string);
    let mut seen_normal = false;
    let mut seen_bright = false;
    for palette in values.all("palette") {
        let Some((index, color)) = palette.split_once('=') else {
            continue;
        };
        let Ok(index) = index.trim().parse::<usize>() else {
            continue;
        };
        if index > 15 {
            continue;
        }
        colors[index] = normalize_color(color.trim())?;
        seen_normal |= index < 8;
        seen_bright |= index >= 8;
    }
    Ok((
        seen_normal.then(|| palette_from_slice(&colors[..8])),
        seen_bright.then(|| palette_from_slice(&colors[8..])),
    ))
}

fn palette_from_slice(colors: &[String]) -> ManagedPalette {
    ManagedPalette {
        black: colors[0].clone(),
        red: colors[1].clone(),
        green: colors[2].clone(),
        yellow: colors[3].clone(),
        blue: colors[4].clone(),
        magenta: colors[5].clone(),
        cyan: colors[6].clone(),
        white: colors[7].clone(),
    }
}

fn translate_binding(value: &str) -> Option<ManagedBinding> {
    let (chord, action) = value.split_once('=')?;
    if chord.contains('>') {
        return None;
    }
    let action = match action.trim() {
        "copy_to_clipboard" => "Copy",
        "paste_from_clipboard" => "Paste",
        "reset_font_size" => "ResetFontSize",
        action
            if action == "increase_font_size" || action.starts_with("increase_font_size:") =>
        {
            "IncreaseFontSize"
        }
        action
            if action == "decrease_font_size" || action.starts_with("decrease_font_size:") =>
        {
            "DecreaseFontSize"
        }
        _ => return None,
    };
    let mut parts = chord.split('+').map(str::trim).collect::<Vec<_>>();
    let key = translate_key(parts.pop()?)?;
    let mut modifiers = Vec::new();
    for modifier in parts {
        modifiers.push(match modifier.to_ascii_lowercase().as_str() {
            "super" => "Super",
            "ctrl" => "Control",
            "alt" => "Alt",
            "shift" => "Shift",
            _ => return None,
        });
    }
    Some(ManagedBinding {
        key,
        with: modifiers.join(" | "),
        action: action.to_string(),
    })
}

fn translate_key(key: &str) -> Option<String> {
    let lower = key.to_ascii_lowercase();
    let translated = match lower.as_str() {
        "equal" => "Equal",
        "minus" => "Minus",
        "zero" => "Digit0",
        "page_up" => "PageUp",
        "page_down" => "PageDown",
        "up" => "Up",
        "down" => "Down",
        key if key.len() == 1 && key.bytes().all(|byte| byte.is_ascii_alphabetic()) => {
            return Some(key.to_ascii_uppercase());
        }
        _ => return None,
    };
    Some(translated.to_string())
}

pub fn import_from_ghostty(app_dir: &Path) -> Result<GhosttyImportResult> {
    import_from_ghostty_with_locations(app_dir, &GhosttyLocations::from_environment()?)
}

fn import_from_ghostty_with_locations(
    app_dir: &Path,
    locations: &GhosttyLocations,
) -> Result<GhosttyImportResult> {
    let mut loaded = load_ghostty_values(locations)?;
    let theme_name = loaded
        .values
        .last("theme")
        .map(selected_dark_theme)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let mut merged = GhosttyValues::default();
    if let Some(name) = &theme_name {
        let theme_path = resolve_theme_path(name, &loaded, locations)
            .with_context(|| format!("Ghostty theme `{name}` was not found"))?;
        let mut theme = LoadedGhostty {
            values: GhosttyValues::default(),
            source_files: Vec::new(),
        };
        load_ghostty_file(&theme_path, &mut theme, &mut HashSet::new())?;
        merged.values_from(&theme.values);
        loaded.source_files.extend(theme.source_files);
    }
    merged.values_from(&loaded.values);
    let config = translated_config(&merged)?;
    let text = toml::to_string_pretty(&config).context("failed to serialize managed Ratty config")?;
    toml::from_str::<toml::Value>(&text).context("generated managed Ratty config is invalid")?;

    let config_path = crate::ratty_host::managed_ratty_config_path(app_dir);
    let directory = config_path.parent().context("managed Ratty config has no parent")?;
    fs::create_dir_all(directory)
        .with_context(|| format!("failed to create managed Ratty directory {}", directory.display()))?;
    let temporary_path = directory.join("ratty.toml.tmp");
    let mut temporary = File::create(&temporary_path)
        .with_context(|| format!("failed to create {}", temporary_path.display()))?;
    temporary.write_all(text.as_bytes())?;
    temporary.flush()?;
    temporary.sync_all()?;
    fs::rename(&temporary_path, &config_path).with_context(|| {
        format!(
            "failed to replace managed Ratty config {}",
            config_path.display()
        )
    })?;

    let metadata = GhosttyImportMetadata {
        source_files: loaded.source_files,
        theme: theme_name,
    };
    let metadata_path = directory.join("import.json");
    fs::write(&metadata_path, serde_json::to_vec_pretty(&metadata)?)
        .with_context(|| format!("failed to write {}", metadata_path.display()))?;
    Ok(GhosttyImportResult {
        config_path,
        metadata,
    })
}

impl GhosttyValues {
    fn values_from(&mut self, other: &GhosttyValues) {
        self.entries.extend(other.entries.iter().cloned());
    }
}

pub fn managed_import_status(app_dir: &Path) -> Result<Option<GhosttyImportMetadata>> {
    let config_path = crate::ratty_host::managed_ratty_config_path(app_dir);
    let metadata_path = config_path.parent().unwrap_or(app_dir).join("import.json");
    if !config_path.is_file() || !metadata_path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(&metadata_path)
        .with_context(|| format!("failed to read {}", metadata_path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse {}", metadata_path.display()))
        .map(Some)
}

pub fn reset_managed_ratty_config(app_dir: &Path) -> Result<bool> {
    let config_path = crate::ratty_host::managed_ratty_config_path(app_dir);
    let directory = config_path.parent().unwrap_or(app_dir).to_path_buf();
    let paths = [
        config_path,
        directory.join("import.json"),
        directory.join("ratty.toml.tmp"),
    ];
    let mut changed = false;
    for path in paths {
        match fs::remove_file(&path) {
            Ok(()) => changed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to remove {}", path.display()));
            }
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn locations(root: &Path) -> GhosttyLocations {
        GhosttyLocations {
            xdg_config_home: root.join("xdg"),
            macos_config_dir: Some(root.join("macos")),
            bundled_theme_dirs: vec![root.join("bundled-themes")],
        }
    }

    fn write_adventure_theme(locations: &GhosttyLocations) {
        let mut theme = String::from(
            "foreground = #f8f8f2\nbackground = #101010\ncursor-color = #eeeeee\n",
        );
        for index in 0..16 {
            theme.push_str(&format!("palette = {index}=#{index:02x}{index:02x}{index:02x}\n"));
        }
        write(
            &locations
                .xdg_config_home
                .join("ghostty/themes/Adventure"),
            &theme,
        );
    }

    #[test]
    fn ghostty_import_macos_config_overrides_xdg_and_included_files_load_last() {
        let temp = tempfile::tempdir().unwrap();
        let locations = locations(temp.path());
        write(
            &locations.xdg_config_home.join("ghostty/config.ghostty"),
            "font-size = 11\n",
        );
        write(
            &locations.macos_config_dir.as_ref().unwrap().join("included"),
            "font-size = 15\n",
        );
        write(
            &locations
                .macos_config_dir
                .as_ref()
                .unwrap()
                .join("config.ghostty"),
            "font-size = 13\nconfig-file = included\n",
        );

        let loaded = load_ghostty_values(&locations).unwrap();

        assert_eq!(loaded.values.last("font-size"), Some("15"));
        assert_eq!(loaded.source_files.len(), 3);
    }

    #[test]
    fn ghostty_import_named_theme_and_user_overrides_become_ratty_theme_sections() {
        let temp = tempfile::tempdir().unwrap();
        let locations = locations(temp.path());
        write_adventure_theme(&locations);
        write(
            &locations.xdg_config_home.join("ghostty/config"),
            "theme = Adventure\ncursor-color = #abcdef\nbackground-opacity = 0.9\n",
        );

        let result = import_from_ghostty_with_locations(temp.path(), &locations).unwrap();
        let value = fs::read_to_string(&result.config_path)
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();

        assert_eq!(value["window"]["opacity"].as_float(), Some(0.9));
        assert_eq!(value["theme"]["foreground"].as_str(), Some("#f8f8f2"));
        assert_eq!(value["theme"]["cursor"].as_str(), Some("#abcdef"));
        assert_eq!(value["theme"]["normal"]["black"].as_str(), Some("#000000"));
        assert_eq!(value["theme"]["bright"]["white"].as_str(), Some("#0f0f0f"));
        assert_eq!(result.metadata.theme.as_deref(), Some("Adventure"));
        assert_eq!(managed_import_status(temp.path()).unwrap(), Some(result.metadata));
    }

    #[test]
    fn ghostty_import_current_style_imports_font_and_named_theme() {
        let temp = tempfile::tempdir().unwrap();
        let locations = locations(temp.path());
        write_adventure_theme(&locations);
        write(
            &locations.xdg_config_home.join("ghostty/config"),
            "theme = Adventure\nfont-family = \"FiraCode Nerd Font Mono\"\nfont-style = Bold Italic\nfont-size = 13.4\nwindow-padding-x = 10\nwindow-padding-y = 10\n",
        );

        let result = import_from_ghostty_with_locations(temp.path(), &locations).unwrap();
        let text = fs::read_to_string(result.config_path).unwrap();
        let value = text.parse::<toml::Value>().unwrap();

        assert_eq!(value["font"]["family"].as_str(), Some("FiraCode Nerd Font Mono"));
        assert_eq!(value["font"]["style"].as_str(), Some("BoldItalic"));
        assert_eq!(value["font"]["size"].as_integer(), Some(13));
        assert!(!text.contains("padding"));
    }

    #[test]
    fn ghostty_import_only_supported_single_chord_bindings_are_translated() {
        let temp = tempfile::tempdir().unwrap();
        let locations = locations(temp.path());
        write(
            &locations.xdg_config_home.join("ghostty/config"),
            "keybind = super+c=copy_to_clipboard\nkeybind = ctrl+shift+v=paste_from_clipboard\nkeybind = super+equal=increase_font_size:1\nkeybind = super+minus=decrease_font_size:1\nkeybind = super+zero=reset_font_size\nkeybind = super+k>c=copy_to_clipboard\nkeybind = super+n=new_window\n",
        );

        let result = import_from_ghostty_with_locations(temp.path(), &locations).unwrap();
        let value = fs::read_to_string(result.config_path)
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();
        let keys = value["bindings"]["keys"].as_array().unwrap();

        assert_eq!(keys.len(), 5);
        assert_eq!(keys[0]["key"].as_str(), Some("C"));
        assert_eq!(keys[0]["with"].as_str(), Some("Super"));
        assert_eq!(keys[0]["action"].as_str(), Some("Copy"));
        assert_eq!(keys[1]["with"].as_str(), Some("Control | Shift"));
        assert_eq!(keys[2]["key"].as_str(), Some("Equal"));
    }

    #[test]
    fn ghostty_import_reset_deletes_only_rchat_managed_files() {
        let temp = tempfile::tempdir().unwrap();
        let app_dir = temp.path().join("rchat");
        let managed = crate::ratty_host::managed_ratty_config_path(&app_dir);
        let metadata = managed.parent().unwrap().join("import.json");
        let temporary = managed.parent().unwrap().join("ratty.toml.tmp");
        let unrelated_managed = managed.parent().unwrap().join("keep.txt");
        let ghostty = temp.path().join("ghostty/config");
        let standalone_ratty = temp.path().join("ratty/ratty.toml");
        write(&managed, "[font]\nsize = 13\n");
        write(&metadata, "{}\n");
        write(&temporary, "partial");
        write(&unrelated_managed, "keep");
        write(&ghostty, "font-size = 13\n");
        write(&standalone_ratty, "[font]\nsize = 20\n");

        assert!(reset_managed_ratty_config(&app_dir).unwrap());

        assert!(!managed.exists());
        assert!(!metadata.exists());
        assert!(!temporary.exists());
        assert_eq!(fs::read_to_string(unrelated_managed).unwrap(), "keep");
        assert_eq!(fs::read_to_string(ghostty).unwrap(), "font-size = 13\n");
        assert_eq!(fs::read_to_string(standalone_ratty).unwrap(), "[font]\nsize = 20\n");
        assert!(!reset_managed_ratty_config(&app_dir).unwrap());
    }

    #[test]
    fn ghostty_import_failed_import_keeps_previous_managed_config() {
        let temp = tempfile::tempdir().unwrap();
        let locations = locations(temp.path());
        let managed = crate::ratty_host::managed_ratty_config_path(temp.path());
        write(&managed, "[font]\nsize = 17\n");
        write(
            &locations.xdg_config_home.join("ghostty/config"),
            "foreground = #not-a-color\n",
        );

        let error = import_from_ghostty_with_locations(temp.path(), &locations).unwrap_err();

        assert!(error.to_string().contains("color"));
        assert_eq!(fs::read_to_string(managed).unwrap(), "[font]\nsize = 17\n");
    }

    #[test]
    fn ghostty_import_selects_dark_theme_and_honors_font_reset() {
        let temp = tempfile::tempdir().unwrap();
        let locations = locations(temp.path());
        write_adventure_theme(&locations);
        write(
            &locations.xdg_config_home.join("ghostty/themes/Light"),
            "background = #ffffff\n",
        );
        write(
            &locations.xdg_config_home.join("ghostty/config"),
            "theme = light:Light,dark:Adventure\nfont-family = First\nfont-family = Second\nfont-family = \"\"\nfont-family = Final\nfont-family = Ignored\nbackground-opacity = 2.5\n",
        );

        let result = import_from_ghostty_with_locations(temp.path(), &locations).unwrap();
        let value = fs::read_to_string(result.config_path)
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();

        assert_eq!(value["theme"]["background"].as_str(), Some("#101010"));
        assert_eq!(value["font"]["family"].as_str(), Some("Final"));
        assert_eq!(value["window"]["opacity"].as_float(), Some(1.0));
        assert_eq!(result.metadata.theme.as_deref(), Some("Adventure"));
    }

    #[test]
    fn ghostty_import_optional_includes_are_skipped_and_cycles_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let locations = locations(temp.path());
        let config = locations.xdg_config_home.join("ghostty/config");
        write(&config, "font-size = 12\nconfig-file = ?missing\n");
        assert_eq!(
            load_ghostty_values(&locations)
                .unwrap()
                .values
                .last("font-size"),
            Some("12")
        );

        write(&config, "config-file = nested\n");
        write(&config.parent().unwrap().join("nested"), "config-file = config\n");
        let error = load_ghostty_values(&locations).unwrap_err();
        assert!(error.to_string().contains("cycle"));
    }

    #[test]
    fn ghostty_import_malformed_lines_report_source_and_line() {
        let temp = tempfile::tempdir().unwrap();
        let locations = locations(temp.path());
        let config = locations.xdg_config_home.join("ghostty/config");
        write(&config, "font-size = 12\nthis is malformed\n");

        let error = load_ghostty_values(&locations).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains(&config.display().to_string()));
        assert!(message.contains(":2"));
    }
}
