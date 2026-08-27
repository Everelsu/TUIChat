//! Обзор файлов для отправки.
//!
//! Вводить путь руками — ровно тот костыль, из-за которого «отправить картинку»
//! в терминале ощущается наказанием. Здесь список каталога, из которого файл
//! выбирается стрелками.
//!
//! Чтение каталога живёт отдельно от состояния: на сетевом диске оно способно
//! думать секундами, и делать это в цикле отрисовки нельзя.

use std::path::{Path, PathBuf};

/// Что сервер умеет принимать. Остальное показываем приглушённым: прятать
/// файлы хуже — человек начинает искать, куда делся его файл.
const SUPPORTED: [&str; 11] = [
    "jpg", "jpeg", "png", "gif", "webp", "webm", "ogg", "oga", "mp4", "m4a", "wav",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    /// Картинка или звук — то, что чат покажет прямо в ленте. Остальное
    /// отправляется тоже, просто приходит строкой с именем и размером.
    pub media: bool,
}

pub fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| SUPPORTED.contains(&extension.to_lowercase().as_str()))
}

/// Читает каталог. Блокирующая работа — вызывать из отдельного потока.
pub fn read_dir(path: &Path) -> Result<Vec<FileEntry>, String> {
    let listing = std::fs::read_dir(path).map_err(|err| format!("{}: {err}", path.display()))?;

    let mut entries: Vec<FileEntry> = Vec::new();
    // Подъём наверх делаем строкой списка, а не только клавишей: так его
    // видно, и не приходится угадывать.
    if let Some(parent) = path.parent() {
        entries.push(FileEntry {
            name: "..".to_string(),
            path: parent.to_path_buf(),
            is_dir: true,
            size: 0,
            media: false,
        });
    }

    let mut found: Vec<FileEntry> = listing
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            // Скрытые файлы прячем: в домашнем каталоге их больше, чем нужных.
            if name.starts_with('.') {
                return None;
            }
            let meta = entry.metadata().ok()?;
            Some(FileEntry {
                media: !meta.is_dir() && is_media(&path),
                name,
                path,
                is_dir: meta.is_dir(),
                size: meta.len(),
            })
        })
        .collect();

    // Каталоги первыми, дальше по алфавиту без учёта регистра: так список
    // читается сверху вниз, а не прыгает между «Фото» и «архив».
    found.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    entries.extend(found);
    Ok(entries)
}

/// С какого каталога открывать обзор.
pub fn start_dir(remembered: Option<&str>) -> PathBuf {
    if let Some(path) = remembered.map(PathBuf::from)
        && path.is_dir()
    {
        return path;
    }

    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    match home.map(PathBuf::from) {
        // «Загрузки» — то место, куда попадает всё, что потом пересылают.
        Some(home) if home.join("Downloads").is_dir() => home.join("Downloads"),
        Some(home) => home,
        None => PathBuf::from("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Временный каталог с несколькими файлами.
    fn sample() -> PathBuf {
        let root = std::env::temp_dir().join(format!("chat-files-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("вложенный")).unwrap();
        std::fs::write(root.join("кот.png"), [0u8; 4]).unwrap();
        std::fs::write(root.join("заметки.txt"), [0u8; 4]).unwrap();
        std::fs::write(root.join(".скрытый"), [0u8; 4]).unwrap();
        root
    }

    #[test]
    fn known_types_are_recognized() {
        assert!(is_media(Path::new("кот.PNG")));
        assert!(is_media(Path::new("голосовое.webm")));
        assert!(!is_media(Path::new("заметки.txt")));
        assert!(!is_media(Path::new("без_расширения")));
    }

    #[test]
    fn directories_come_first_and_hidden_files_stay_hidden() {
        let root = sample();

        let entries = read_dir(&root).unwrap();

        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names[0], "..", "подъём наверх должен быть виден");
        assert_eq!(names[1], "вложенный");
        assert!(!names.contains(&".скрытый"), "{names:?}");
        assert!(names.contains(&"кот.png"), "{names:?}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn plain_files_are_listed_and_marked_as_not_media() {
        let root = sample();

        let entries = read_dir(&root).unwrap();

        let notes = entries.iter().find(|entry| entry.name == "заметки.txt");
        // Файл видно, но помечен: прятать его хуже — человек пойдёт искать,
        // куда тот делся.
        assert!(!notes.unwrap().media);
        let picture = entries.iter().find(|entry| entry.name == "кот.png");
        assert!(picture.unwrap().media);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_directory_is_reported() {
        let error = read_dir(Path::new("нет-такого-каталога")).unwrap_err();

        assert!(error.contains("нет-такого-каталога"), "{error}");
    }

    #[test]
    fn remembered_directory_is_used_when_it_exists() {
        let root = sample();

        let chosen = start_dir(Some(&root.to_string_lossy()));

        assert_eq!(chosen, root);
        // Исчезнувший каталог не должен ронять обзор — берём запасной.
        assert_ne!(start_dir(Some("нет-такого")), PathBuf::from("нет-такого"));

        std::fs::remove_dir_all(&root).ok();
    }
}
