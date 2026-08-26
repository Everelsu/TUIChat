//! Общие для сервера и клиентов типы: протокол + валидация ввода.

pub mod protocol;
pub mod validate;

pub use protocol::{
    Attachment, AttachmentKind, ChatMessage, ClientMessage, ErrorCode, REPLY_EXCERPT_CHARS,
    ReplyPreview, ServerMessage, UserId, UserInfo,
};
pub use validate::{
    MAX_FRAME_BYTES, MAX_NICKNAME_CHARS, MAX_ROOM_CHARS, MAX_TEXT_CHARS, MAX_UPLOAD_BYTES,
    ValidationError, clean_file_name, clean_nickname, clean_room, clean_text, nickname_key,
};

use std::time::{SystemTime, UNIX_EPOCH};

/// Текущее время в миллисекундах UTC — то, что кладётся в `ServerMessage::Chat::ts`.
/// Время проставляет только сервер: часы клиентов доверия не заслуживают.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::MAX_FILE_NAME_CHARS;
    use serde_json::json;
    use uuid::Uuid;

    fn roundtrip_client(msg: ClientMessage) {
        let s = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&s).unwrap(),
            msg,
            "json: {s}"
        );
    }

    fn roundtrip_server(msg: ServerMessage) {
        let s = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerMessage>(&s).unwrap(),
            msg,
            "json: {s}"
        );
    }

    #[test]
    fn client_messages_roundtrip() {
        roundtrip_client(ClientMessage::Join {
            nickname: "alice".into(),
            room: "general".into(),
        });
        roundtrip_client(ClientMessage::Chat {
            text: "привет".into(),
            attachment: Some(Uuid::new_v4()),
            reply_to: Some(Uuid::new_v4()),
        });
        roundtrip_client(ClientMessage::Leave);
        roundtrip_client(ClientMessage::Ping);
    }

    #[test]
    fn server_messages_roundtrip() {
        let user = UserInfo {
            id: Uuid::new_v4(),
            nickname: "bob".into(),
        };
        let message = ChatMessage {
            id: Uuid::new_v4(),
            from: user.clone(),
            text: "как дела?".into(),
            ts: 1_700_000_000_000,
            attachment: None,
            reply: None,
        };
        roundtrip_server(ServerMessage::Welcome {
            your_id: Uuid::new_v4(),
            room: "general".into(),
            nickname: "alice".into(),
            users: vec![user.clone()],
            history: vec![message.clone()],
        });
        roundtrip_server(ServerMessage::UserJoined { user: user.clone() });
        roundtrip_server(ServerMessage::UserLeft { user: user.clone() });
        roundtrip_server(ServerMessage::Chat(message));
        roundtrip_server(ServerMessage::error(ErrorCode::NicknameTaken, "ник занят"));
        roundtrip_server(ServerMessage::Pong);
    }

    /// Форма JSON — часть публичного контракта: на неё завязан веб-клиент на JS.
    /// Если этот тест упал, значит сломана совместимость с уже написанными клиентами.
    #[test]
    fn wire_format_is_stable() {
        let join = ClientMessage::Join {
            nickname: "alice".into(),
            room: "general".into(),
        };
        assert_eq!(
            serde_json::to_value(&join).unwrap(),
            json!({"type": "join", "nickname": "alice", "room": "general"})
        );

        let leave = ClientMessage::Leave;
        assert_eq!(
            serde_json::to_value(&leave).unwrap(),
            json!({"type": "leave"})
        );

        // Вынос полей чата в отдельную структуру не должен менять форму кадра:
        // веб-клиент читает id, from, text и ts на верхнем уровне.
        let id = Uuid::nil();
        let chat = ServerMessage::Chat(ChatMessage {
            id,
            from: UserInfo {
                id,
                nickname: "bob".into(),
            },
            text: "привет".into(),
            ts: 1_700_000_000_000,
            attachment: None,
            reply: None,
        });
        assert_eq!(
            serde_json::to_value(&chat).unwrap(),
            json!({
                "type": "chat",
                "id": id,
                "from": {"id": id, "nickname": "bob"},
                "text": "привет",
                "ts": 1_700_000_000_000_i64,
            })
        );

        let err = ServerMessage::error(ErrorCode::NicknameTaken, "ник занят");
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({"type": "error", "code": "nickname_taken", "message": "ник занят"})
        );
    }

    #[test]
    fn attachment_travels_as_a_nested_object() {
        let id = Uuid::nil();
        let chat = ServerMessage::Chat(ChatMessage {
            id,
            from: UserInfo {
                id,
                nickname: "bob".into(),
            },
            text: String::new(),
            ts: 1_700_000_000_000,
            attachment: Some(Attachment {
                id,
                kind: AttachmentKind::Image,
                name: "кот.jpg".into(),
                size: 1024,
                mime: "image/jpeg".into(),
            }),
            reply: None,
        });

        let value = serde_json::to_value(&chat).unwrap();
        assert_eq!(value["attachment"]["kind"], "image");
        assert_eq!(value["attachment"]["name"], "кот.jpg");

        // Клиент присылает только идентификатор: остальное сервер знает сам.
        let outgoing = ClientMessage::Chat {
            text: "смотри".into(),
            attachment: Some(id),
            reply_to: None,
        };
        assert_eq!(
            serde_json::to_value(&outgoing).unwrap(),
            json!({"type": "chat", "text": "смотри", "attachment": id})
        );
    }

    #[test]
    fn chat_without_attachment_keeps_the_old_shape() {
        // Старый клиент ничего не знает про вложения: поля не должно быть
        // в кадре, если вложения нет.
        let outgoing = ClientMessage::Chat {
            text: "привет".into(),
            attachment: None,
            reply_to: None,
        };
        assert_eq!(
            serde_json::to_value(&outgoing).unwrap(),
            json!({"type": "chat", "text": "привет"})
        );
        // И наоборот: кадр без поля читается как отсутствие вложения.
        let parsed: ClientMessage =
            serde_json::from_str(r#"{"type":"chat","text":"привет"}"#).unwrap();
        assert_eq!(parsed, outgoing);
    }

    #[test]
    fn file_names_are_made_safe() {
        assert_eq!(clean_file_name("кот.jpg"), "кот.jpg");
        // Имя приходит от клиента и не должно уметь указывать на путь.
        assert_eq!(clean_file_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(clean_file_name("  "), "файл");
        assert_eq!(clean_file_name("a\u{1b}b.png"), "a_b.png");
        assert!(clean_file_name(&"я".repeat(200)).chars().count() <= MAX_FILE_NAME_CHARS);
    }

    #[test]
    fn unknown_message_type_is_rejected() {
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"nuke"}"#).is_err());
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"chat"}"#).is_err());
    }

    #[test]
    fn nicknames() {
        assert_eq!(clean_nickname("  alice ").unwrap(), "alice");
        assert_eq!(clean_nickname("Алиса").unwrap(), "Алиса");
        assert!(matches!(
            clean_nickname("   "),
            Err(ValidationError::Empty { .. })
        ));
        assert!(matches!(
            clean_nickname("a b"),
            Err(ValidationError::IllegalChar { .. })
        ));
        assert!(matches!(
            clean_nickname("a\u{200b}b"),
            Err(ValidationError::IllegalChar { .. })
        ));
        assert!(matches!(
            clean_nickname("a\u{1b}[31m"),
            Err(ValidationError::IllegalChar { .. })
        ));
        let long = "a".repeat(MAX_NICKNAME_CHARS + 1);
        assert!(matches!(
            clean_nickname(&long),
            Err(ValidationError::TooLong { .. })
        ));
        // Лимит считается в символах, а не в байтах.
        assert!(clean_nickname(&"я".repeat(MAX_NICKNAME_CHARS)).is_ok());
    }

    #[test]
    fn nickname_uniqueness_ignores_case() {
        assert_eq!(nickname_key("Alice"), nickname_key("alice"));
        assert_ne!(nickname_key("alice"), nickname_key("alice2"));
    }

    #[test]
    fn rooms() {
        assert_eq!(clean_room(" General ").unwrap(), "general");
        assert_eq!(clean_room("room-1_x").unwrap(), "room-1_x");
        assert!(matches!(clean_room(""), Err(ValidationError::Empty { .. })));
        assert!(matches!(
            clean_room("общая"),
            Err(ValidationError::IllegalChar { .. })
        ));
        assert!(matches!(
            clean_room("a/b"),
            Err(ValidationError::IllegalChar { .. })
        ));
        let long = "a".repeat(MAX_ROOM_CHARS + 1);
        assert!(matches!(
            clean_room(&long),
            Err(ValidationError::TooLong { .. })
        ));
    }

    #[test]
    fn texts() {
        assert_eq!(clean_text("  привет  ").unwrap(), "привет");
        // Многострочная вставка схлопывается в одну строку.
        assert_eq!(clean_text("одна\nдве\r\nтри").unwrap(), "одна две три");
        assert_eq!(clean_text("a\t\tb").unwrap(), "a b");
        // Управляющие последовательности терминала не должны доходить до рендера.
        assert_eq!(clean_text("\u{1b}[31mred\u{1b}[0m").unwrap(), "[31mred[0m");
        assert!(matches!(
            clean_text("   \n\t "),
            Err(ValidationError::Empty { .. })
        ));
        assert!(matches!(
            clean_text("\u{200b}"),
            Err(ValidationError::Empty { .. })
        ));
        let long = "a".repeat(MAX_TEXT_CHARS + 1);
        assert!(matches!(
            clean_text(&long),
            Err(ValidationError::TooLong { .. })
        ));
    }

    #[test]
    fn oversized_input_is_rejected_before_char_scan() {
        let huge = "a".repeat(MAX_FRAME_BYTES + 1);
        assert!(matches!(
            clean_text(&huge),
            Err(ValidationError::TooLong {
                max: MAX_FRAME_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn now_ms_is_sane() {
        // Больше, чем 2020-01-01 — ловит перепутанные секунды/миллисекунды.
        assert!(now_ms() > 1_577_836_800_000);
    }
}
