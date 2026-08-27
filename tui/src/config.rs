//! Настройки клиента: адрес сервера, последний ник и цвета собеседников.
//!
//! Файл кладём туда, где такие файлы принято искать: `%APPDATA%` на Windows,
//! `$XDG_CONFIG_HOME` или `~/.config` на остальных. Формат — TOML, потому что
//! цвета человек правит руками, а не через интерфейс.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

const APP_DIR: &str = "tuichat";
const FILE: &str = "config.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: String,
    /// Последний ник. Подставляется в поле входа при следующем запуске.
    pub nickname: Option<String>,
    pub room: String,
    /// Каталог, в котором последний раз выбирали файл для отправки.
    pub last_dir: Option<String>,
    /// Показывать ли картинки прямо в переписке. `None` — решает клиент по
    /// возможностям терминала: в sixel лента от них заметно тормозит.
    pub inline_images: Option<bool>,
    /// Перезапускать ли клиент в нормальном терминале: `auto` — только после
    /// двойного клика, `always` — из любого несовременного окна, `never` —
    /// никогда. Подробности — в `launcher`.
    pub terminal: String,
    /// В каком терминале открываться: `wezterm`, `wt` или пусто — «выбирай
    /// сам». Названный здесь терминал единственный, который годится: раз его
    /// выбрали, уходить в другой молча нельзя.
    pub terminal_program: String,
    /// Цвета ников: ключ — ник в нижнем регистре, значение — `#rrggbb`
    /// или название вроде `cyan`.
    pub colors: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: crate::net::DEFAULT_SERVER.to_string(),
            nickname: None,
            room: "general".to_string(),
            last_dir: None,
            inline_images: None,
            terminal: "auto".to_string(),
            terminal_program: String::new(),
            colors: BTreeMap::new(),
        }
    }
}

/// Каталог с настройками и журналом падений.
pub fn dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    };
    Some(base?.join(APP_DIR))
}

pub fn path() -> Option<PathBuf> {
    Some(dir()?.join(FILE))
}

impl Config {
    /// Читает настройки. Сломанный или отсутствующий файл — не повод падать:
    /// берём значения по умолчанию и работаем дальше.
    pub fn load() -> Self {
        let Some(path) = path() else {
            return Self::default();
        };
        let Ok(text) = fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|err| std::io::Error::other(format!("не удалось собрать настройки: {err}")))?;
        write_atomically(&path, text.as_bytes())
    }

    /// Цвет ника, заданный человеком.
    pub fn color_of(&self, nickname: &str) -> Option<Color> {
        self.colors
            .get(&nickname.to_lowercase())
            .and_then(|value| parse_color(value))
    }

    pub fn set_color(&mut self, nickname: &str, color: &str) {
        self.colors
            .insert(nickname.to_lowercase(), color.to_string());
    }

    pub fn clear_color(&mut self, nickname: &str) {
        self.colors.remove(&nickname.to_lowercase());
    }
}

/// Пишет файл через временный: прерванная запись не должна оставлять
/// обрезанный конфиг вместо рабочего.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

/// Разбирает цвет: `#rrggbb` или привычное название.
pub fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim().to_lowercase();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 {
            return None;
        }
        let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
        return Some(Color::Rgb(channel(0)?, channel(2)?, channel(4)?));
    }

    // Названия отдаём как RGB, а не как именованные цвета терминала: те
    // перекрашиваются темой, и «синий» на разных машинах разный.
    let named = match value.as_str() {
        "red" | "красный" => (226, 108, 108),
        "green" | "зелёный" | "зеленый" => (126, 186, 128),
        "yellow" | "жёлтый" | "желтый" => (214, 173, 96),
        "blue" | "синий" => (114, 159, 207),
        "magenta" | "purple" | "фиолетовый" => (178, 148, 214),
        "cyan" | "бирюзовый" => (94, 186, 176),
        "orange" | "оранжевый" => (217, 119, 87),
        "white" | "белый" => (230, 230, 234),
        "gray" | "grey" | "серый" => (128, 132, 138),
        _ => return None,
    };
    Some(Color::Rgb(named.0, named.1, named.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors_are_parsed() {
        assert_eq!(parse_color("#d97757"), Some(Color::Rgb(217, 119, 87)));
        assert_eq!(parse_color("  #FFFFFF "), Some(Color::Rgb(255, 255, 255)));
    }

    #[test]
    fn names_are_parsed_in_both_languages() {
        assert_eq!(parse_color("cyan"), parse_color("бирюзовый"));
        assert!(parse_color("зелёный").is_some());
    }

    #[test]
    fn nonsense_colors_are_rejected() {
        for value in ["#fff", "#gggggg", "розовый в крапинку", ""] {
            assert!(parse_color(value).is_none(), "принят цвет {value:?}");
        }
    }

    #[test]
    fn colors_are_looked_up_regardless_of_case() {
        let mut config = Config::default();
        config.set_color("Alice", "#d97757");

        assert_eq!(config.color_of("alice"), Some(Color::Rgb(217, 119, 87)));
        assert_eq!(config.color_of("ALICE"), Some(Color::Rgb(217, 119, 87)));
    }

    #[test]
    fn broken_file_falls_back_to_defaults() {
        // Правленный руками файл может оказаться сломанным — это не повод
        // отказываться запускаться.
        let broken: Config = toml::from_str("это не тоml =").unwrap_or_default();

        assert_eq!(broken.room, Config::default().room);
    }

    #[test]
    fn config_survives_a_round_trip() {
        let mut config = Config {
            nickname: Some("alice".into()),
            room: "rust".into(),
            ..Config::default()
        };
        config.set_color("bob", "cyan");

        let text = toml::to_string_pretty(&config).unwrap();
        let restored: Config = toml::from_str(&text).unwrap();

        assert_eq!(restored.nickname.as_deref(), Some("alice"));
        assert_eq!(restored.room, "rust");
        assert_eq!(restored.color_of("bob"), parse_color("cyan"));
    }

    #[test]
    fn missing_fields_keep_defaults() {
        // Старый файл без новых полей должен читаться, а не отбрасываться.
        let config: Config = toml::from_str("room = \"rust\"").unwrap();

        assert_eq!(config.room, "rust");
        assert_eq!(config.server, Config::default().server);
    }
}
