//! Хранилище загруженных файлов.
//!
//! Файлы лежат в памяти: чат живёт, пока запущен сервер, и заводить ради
//! картинок работу с диском незачем. Место ограничено, самые старые файлы
//! вытесняются — иначе за вечер переписки сервер съест всю память.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::Mutex,
};

use axum::body::Bytes;
use common::{Attachment, AttachmentKind, validate};
use uuid::Uuid;

/// Сколько всего байт вложений держим.
pub const DEFAULT_CAPACITY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct StoredFile {
    pub mime: &'static str,
    pub bytes: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
    TooLarge { size: usize, limit: usize },
    UnsupportedType,
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::TooLarge { size, limit } => write!(
                f,
                "файл слишком большой: {} КБ при потолке {} КБ",
                size / 1024,
                limit / 1024
            ),
            MediaError::UnsupportedType => write!(
                f,
                "поддерживаются картинки (jpeg, png, gif, webp) и звук (webm, ogg, mp4)"
            ),
        }
    }
}

impl std::error::Error for MediaError {}

struct Entry {
    attachment: Attachment,
    file: StoredFile,
}

#[derive(Default)]
struct Files {
    by_id: HashMap<Uuid, Entry>,
    /// Порядок загрузки — по нему вытесняем самые старые файлы.
    order: VecDeque<Uuid>,
    bytes: usize,
}

pub struct MediaStore {
    capacity: usize,
    files: Mutex<Files>,
}

impl Default for MediaStore {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY_BYTES)
    }
}

impl MediaStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            files: Mutex::new(Files::default()),
        }
    }

    /// Принимает файл и возвращает его описание.
    ///
    /// Тип берётся из содержимого, а не из заголовка запроса: клиент может
    /// назвать байты как угодно, а браузеру потом это исполнять.
    pub fn put(&self, name: &str, bytes: Bytes) -> Result<Attachment, MediaError> {
        if bytes.len() > validate::MAX_UPLOAD_BYTES {
            return Err(MediaError::TooLarge {
                size: bytes.len(),
                limit: validate::MAX_UPLOAD_BYTES,
            });
        }
        let (mime, kind) = sniff(&bytes).ok_or(MediaError::UnsupportedType)?;

        let attachment = Attachment {
            id: Uuid::new_v4(),
            kind,
            name: validate::clean_file_name(name),
            size: bytes.len() as u64,
            mime: mime.to_string(),
        };

        let mut files = self.files.lock().expect("mutex отравлен");
        files.bytes += bytes.len();
        files.order.push_back(attachment.id);
        files.by_id.insert(
            attachment.id,
            Entry {
                attachment: attachment.clone(),
                file: StoredFile { mime, bytes },
            },
        );

        while files.bytes > self.capacity {
            let Some(oldest) = files.order.pop_front() else {
                break;
            };
            if let Some(entry) = files.by_id.remove(&oldest) {
                files.bytes -= entry.file.bytes.len();
            }
        }

        Ok(attachment)
    }

    pub fn get(&self, id: Uuid) -> Option<StoredFile> {
        let files = self.files.lock().expect("mutex отравлен");
        files.by_id.get(&id).map(|entry| entry.file.clone())
    }

    /// Описание файла по идентификатору.
    ///
    /// Через него проходит каждое сообщение с вложением: клиент присылает лишь
    /// id, а имя, размер и тип берутся отсюда — подделать подпись под чужой
    /// картинкой нельзя.
    pub fn describe(&self, id: Uuid) -> Option<Attachment> {
        let files = self.files.lock().expect("mutex отравлен");
        files.by_id.get(&id).map(|entry| entry.attachment.clone())
    }

    pub fn len(&self) -> usize {
        self.files.lock().expect("mutex отравлен").by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Определяет тип по сигнатуре в начале файла.
///
/// SVG в списке намеренно нет: это XML, умеющий исполнять скрипты, а раздаём
/// мы файлы со своего же адреса — картинка от чужого человека получила бы
/// доступ к странице чата.
fn sniff(bytes: &[u8]) -> Option<(&'static str, AttachmentKind)> {
    let image = |mime| Some((mime, AttachmentKind::Image));
    let audio = |mime| Some((mime, AttachmentKind::Audio));

    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        image("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        image("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        image("image/gif")
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        image("image/webp")
    // Дальше — то, во что браузеры пишут с микрофона: WebM в Chrome,
    // Ogg в Firefox, MP4 в Safari.
    } else if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        audio("audio/webm")
    } else if bytes.starts_with(b"OggS") {
        audio("audio/ogg")
    } else if bytes.len() > 12 && &bytes[4..8] == b"ftyp" {
        audio("audio/mp4")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(size: usize) -> Bytes {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(size.max(8), 0);
        Bytes::from(bytes)
    }

    #[test]
    fn stores_and_returns_a_file() {
        let store = MediaStore::default();

        let attachment = store.put("кот.png", png(64)).unwrap();

        assert_eq!(attachment.mime, "image/png");
        assert_eq!(attachment.name, "кот.png");
        assert_eq!(attachment.size, 64);
        assert_eq!(store.get(attachment.id).unwrap().bytes.len(), 64);
        assert_eq!(store.describe(attachment.id).unwrap(), attachment);
    }

    #[test]
    fn type_comes_from_content_not_from_the_name() {
        let store = MediaStore::default();

        // Имя врёт, содержимое — png.
        let attachment = store.put("документ.jpg", png(32)).unwrap();

        assert_eq!(attachment.mime, "image/png");
    }

    #[test]
    fn rejects_anything_that_is_not_an_image() {
        let store = MediaStore::default();

        // Скрипт, притворяющийся картинкой, не должен попасть в хранилище:
        // отдавали бы мы его со своего же адреса.
        let script = Bytes::from_static(b"<svg onload=alert(1)></svg>");
        assert_eq!(
            store.put("картинка.svg", script),
            Err(MediaError::UnsupportedType)
        );
        assert!(store.is_empty());
    }

    #[test]
    fn recognizes_what_browsers_record_from_the_microphone() {
        let store = MediaStore::default();

        // Chrome пишет в WebM, Firefox в Ogg, Safari в MP4.
        let webm = Bytes::from_static(&[0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0]);
        let ogg = Bytes::from_static(b"OggS       ");
        let mp4 = Bytes::from_static(b"    ftypisom   ");

        for (bytes, expected) in [(webm, "audio/webm"), (ogg, "audio/ogg"), (mp4, "audio/mp4")] {
            let attachment = store.put("голосовое", bytes).unwrap();
            assert_eq!(attachment.mime, expected);
            assert_eq!(attachment.kind, AttachmentKind::Audio);
        }
    }

    #[test]
    fn rejects_oversized_files() {
        let store = MediaStore::default();

        let huge = png(validate::MAX_UPLOAD_BYTES + 1);

        assert!(matches!(
            store.put("огромный.png", huge),
            Err(MediaError::TooLarge { .. })
        ));
    }

    #[test]
    fn oldest_files_are_evicted_when_the_space_runs_out() {
        let store = MediaStore::new(1000);

        let first = store.put("первый.png", png(600)).unwrap();
        let second = store.put("второй.png", png(600)).unwrap();

        // Иначе за вечер переписки сервер съел бы всю память.
        assert!(store.get(first.id).is_none(), "старый файл не вытеснен");
        assert!(store.get(second.id).is_some());
    }

    #[test]
    fn unknown_id_is_not_described() {
        let store = MediaStore::default();

        assert!(store.describe(Uuid::new_v4()).is_none());
    }
}
