//! Проверка и нормализация пользовательского ввода.
//!
//! Всё это обязано выполняться **на сервере** — клиент может врать. Клиенты
//! используют те же функции, чтобы не гонять заведомо мусорные сообщения по сети
//! и показывать ошибку сразу.

use std::fmt;

pub const MIN_NICKNAME_CHARS: usize = 1;
pub const MAX_NICKNAME_CHARS: usize = 20;
pub const MAX_ROOM_CHARS: usize = 32;
pub const MAX_TEXT_CHARS: usize = 1000;

/// Потолок на размер загружаемого файла.
///
/// Отдельно от лимита кадра: файлы идут не по сокету, а обычным POST-запросом.
///
/// Это потолок по умолчанию, а не жёсткий предел: сервер поднимает или
/// опускает его переменной `CHAT_MAX_UPLOAD_MB` и сообщает клиенту свой при
/// входе в комнату. Совсем без потолка нельзя — вложения лежат в памяти
/// сервера, и один файл не должен класть комнату.
pub const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

pub const MAX_FILE_NAME_CHARS: usize = 80;

/// Жёсткий лимит на размер одного WebSocket-фрейма (см. `max_message_size`
/// у tokio-tungstenite). Считается в байтах и перекрывает все лимиты выше.
pub const MAX_FRAME_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    Empty {
        field: &'static str,
    },
    TooLong {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    IllegalChar {
        field: &'static str,
        ch: char,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::Empty { field } => write!(f, "{field}: пустое значение"),
            ValidationError::TooLong { field, max, actual } => {
                write!(
                    f,
                    "{field}: слишком длинное ({actual} симв., максимум {max})"
                )
            }
            ValidationError::IllegalChar { field, ch } => {
                write!(f, "{field}: недопустимый символ {ch:?}")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

/// Невидимые символы, которыми можно испортить рендер терминала или подделать ник
/// (zero-width, bidi-override, BOM). `char::is_control` их не ловит.
fn is_invisible(ch: char) -> bool {
    matches!(ch,
        '\u{061c}' | '\u{180e}' | '\u{feff}'
        | '\u{200b}'..='\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2066}'..='\u{2069}'
    )
}

fn guard_frame(field: &'static str, raw: &str) -> Result<(), ValidationError> {
    if raw.len() > MAX_FRAME_BYTES {
        return Err(ValidationError::TooLong {
            field,
            max: MAX_FRAME_BYTES,
            actual: raw.len(),
        });
    }
    Ok(())
}

/// Ник в том виде, в каком его увидят остальные.
///
/// Пробелы по краям срезаются, внутри пробелов и управляющих символов быть не должно.
pub fn clean_nickname(raw: &str) -> Result<String, ValidationError> {
    const FIELD: &str = "ник";
    guard_frame(FIELD, raw)?;

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::Empty { field: FIELD });
    }
    for ch in trimmed.chars() {
        if ch.is_control() || ch.is_whitespace() || is_invisible(ch) {
            return Err(ValidationError::IllegalChar { field: FIELD, ch });
        }
    }
    let len = trimmed.chars().count();
    if len < MIN_NICKNAME_CHARS {
        return Err(ValidationError::Empty { field: FIELD });
    }
    if len > MAX_NICKNAME_CHARS {
        return Err(ValidationError::TooLong {
            field: FIELD,
            max: MAX_NICKNAME_CHARS,
            actual: len,
        });
    }
    Ok(trimmed.to_string())
}

/// Ключ уникальности ника внутри комнаты.
///
/// Сравнение регистронезависимое: `Alice` и `alice` — один и тот же занятый ник,
/// иначе такими парами слишком легко выдавать себя за другого.
pub fn nickname_key(nickname: &str) -> String {
    nickname.to_lowercase()
}

/// Имя комнаты. Приводится к нижнему регистру, разрешены `a-z 0-9 _ -`.
pub fn clean_room(raw: &str) -> Result<String, ValidationError> {
    const FIELD: &str = "комната";
    guard_frame(FIELD, raw)?;

    let trimmed = raw.trim().to_lowercase();
    if trimmed.is_empty() {
        return Err(ValidationError::Empty { field: FIELD });
    }
    for ch in trimmed.chars() {
        if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
            return Err(ValidationError::IllegalChar { field: FIELD, ch });
        }
    }
    let len = trimmed.chars().count();
    if len > MAX_ROOM_CHARS {
        return Err(ValidationError::TooLong {
            field: FIELD,
            max: MAX_ROOM_CHARS,
            actual: len,
        });
    }
    Ok(trimmed)
}

/// Текст сообщения.
///
/// Сообщения однострочные: перевод строки и таб схлопываются в пробел, прочие
/// управляющие и невидимые символы выбрасываются. Так TUI не приходится думать
/// про многострочный рендер, а `\r` не портит терминал.
pub fn clean_text(raw: &str) -> Result<String, ValidationError> {
    const FIELD: &str = "сообщение";
    guard_frame(FIELD, raw)?;

    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if is_invisible(ch) {
            continue;
        }
        // Не-пробельные управляющие символы (ESC из ANSI-последовательностей и т.п.)
        // выбрасываем молча: пробел на их месте только мусорил бы текст.
        if ch.is_control() && !ch.is_whitespace() {
            continue;
        }
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }

    if out.is_empty() {
        return Err(ValidationError::Empty { field: FIELD });
    }
    let len = out.chars().count();
    if len > MAX_TEXT_CHARS {
        return Err(ValidationError::TooLong {
            field: FIELD,
            max: MAX_TEXT_CHARS,
            actual: len,
        });
    }
    Ok(out)
}

/// Приводит имя файла к безопасному виду.
///
/// Ошибку не возвращает: из-за странного имени картинки терять картинку глупо.
/// Разделители пути вырезаются — имя приходит от клиента и не должно уметь
/// указывать куда-то за пределы «просто подписи под файлом».
pub fn clean_file_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' => '_',
            ch if ch.is_control() || is_invisible(ch) => '_',
            ch => ch,
        })
        .take(MAX_FILE_NAME_CHARS)
        .collect();

    // Имя нигде не используется как путь: файл лежит под своим uuid, а это
    // просто подпись. Поэтому чистим ровно то, что мешает показу.
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "файл".to_string()
    } else {
        cleaned.to_string()
    }
}
