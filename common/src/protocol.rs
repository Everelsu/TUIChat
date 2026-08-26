//! Типы сообщений, которыми обмениваются клиент и сервер.
//!
//! Формат — JSON с внешним тегом `type`, чтобы веб-клиенту на JS было удобно:
//! `{"type":"chat","text":"привет"}`. Имена полей и вариантов — snake_case,
//! менять их = ломать совместимость со всеми клиентами сразу.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Внутренний идентификатор пользователя. Ник может меняться, id — нет.
pub type UserId = Uuid;

/// Пользователь в том виде, в каком его показывают другим.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: UserId,
    pub nickname: String,
}

/// Вложение к сообщению.
///
/// Сами байты по WebSocket не ходят: кадр ограничен `MAX_FRAME_BYTES`, а
/// base64 раздувает данные ещё на треть. Файл заливается отдельным запросом
/// `POST /upload`, а в сообщении едет только описание.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Uuid,
    pub kind: AttachmentKind,
    pub name: String,
    pub size: u64,
    /// Тип определяет сервер по содержимому, а не по словам клиента.
    pub mime: String,
}

/// Как показывать вложение. Тип содержимого лежит рядом в `mime`, а это —
/// подсказка клиенту, каким виджетом его рисовать.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    Audio,
}

/// Кусочек сообщения, на которое отвечают.
///
/// Собирает его сервер: клиент присылает только идентификатор, иначе цитату
/// можно было бы приписать человеку, который ничего такого не писал. Текст
/// хранится прямо здесь, чтобы цитата рисовалась и у того, кто исходного
/// сообщения уже не застал.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyPreview {
    pub id: Uuid,
    pub nickname: String,
    pub excerpt: String,
}

/// Сколько символов исходного сообщения показывать в цитате.
pub const REPLY_EXCERPT_CHARS: usize = 60;

/// Одна реплика в комнате.
///
/// Отдельный тип, потому что живёт в двух местах: приходит по одной штуке в
/// `Chat` и пачкой в `Welcome` как история комнаты.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// id самого сообщения: по нему клиент отбрасывает дубли, когда после
    /// переподключения история накладывается на уже показанное.
    pub id: Uuid,
    pub from: UserInfo,
    pub text: String,
    /// Unix-время в миллисекундах, UTC. Форматирует в местное время клиент.
    pub ts: i64,
    /// Поле необязательное: старые клиенты, ничего не знающие о вложениях,
    /// продолжают читать такие сообщения как обычный текст.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<Attachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<ReplyPreview>,
}

/// Клиент -> сервер.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Первое сообщение в соединении. Всё остальное до него — ошибка `NotJoined`.
    Join {
        nickname: String,
        room: String,
    },
    Chat {
        text: String,
        /// Идентификатор уже загруженного файла. Клиент присылает только его:
        /// имя, размер и тип сервер берёт из собственного хранилища, чтобы
        /// подпись под чужой картинкой нельзя было подделать.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachment: Option<Uuid>,
        /// Идентификатор сообщения, на которое отвечают.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<Uuid>,
    },
    Leave,
    /// «Я печатаю». Клиент шлёт не чаще раза в пару секунд, сервер раздаёт
    /// остальным. Сообщение одноразовое: снимать его не нужно, у получателей
    /// оно само гаснет.
    Typing,
    Ping,
}

/// Сервер -> клиент.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Ответ на успешный `Join`. `users` — те, кто уже в комнате (без самого
    /// себя), `history` — последние реплики, чтобы вошедший не смотрел в пустой
    /// экран и не терял переписку после обрыва связи.
    Welcome {
        your_id: UserId,
        room: String,
        nickname: String,
        users: Vec<UserInfo>,
        history: Vec<ChatMessage>,
    },
    UserJoined {
        user: UserInfo,
    },
    UserLeft {
        user: UserInfo,
    },
    Chat(ChatMessage),
    Typing {
        user: UserInfo,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
    Pong,
}

/// Машиночитаемая причина ошибки: клиент должен реагировать на `code`,
/// а `message` показывать человеку.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Ник уже занят в этой комнате — клиенту надо переспросить ник.
    NicknameTaken,
    InvalidNickname,
    InvalidRoom,
    InvalidMessage,
    /// Прислали что-то до `Join`.
    NotJoined,
    /// Повторный `Join` в том же соединении.
    AlreadyJoined,
    RoomFull,
    ServerFull,
    RateLimited,
    Internal,
}

impl ServerMessage {
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        ServerMessage::Error {
            code,
            message: message.into(),
        }
    }
}
