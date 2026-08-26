//! Снимок интерфейса для глаз, а не для утверждений.
//!
//! Запуск: `cargo test -p tui --test preview -- --nocapture`. Тест ничего не
//! проверяет — он печатает экран, чтобы вёрстку можно было оценить целиком,
//! не поднимая сервер и не открывая терминал.

use common::{ChatMessage, ServerMessage, UserInfo};
use ratatui::{Terminal, backend::TestBackend};
use tui::app::{Action, NetEvent, State, update};
use uuid::Uuid;

fn user(nickname: &str) -> UserInfo {
    UserInfo {
        id: Uuid::new_v4(),
        nickname: nickname.into(),
    }
}

#[test]
fn chat_screen() {
    let (mut state, _) = State::new(Some("alice".into()), "general".into());
    let bob = user("bob");
    let carol = user("carol");
    let me = Uuid::new_v4();

    update(
        &mut state,
        Action::Net(NetEvent::Message(ServerMessage::Welcome {
            your_id: me,
            room: "general".into(),
            nickname: "alice".into(),
            users: vec![bob.clone(), carol.clone()],
            history: vec![],
        })),
    );

    let mut at = 1_700_000_000_000;
    let mut say = |state: &mut State, from: &UserInfo, text: &str, reply: Option<&str>| {
        at += 300_000;
        update(
            state,
            Action::Net(NetEvent::Message(ServerMessage::Chat(ChatMessage {
                id: Uuid::new_v4(),
                from: from.clone(),
                text: text.into(),
                ts: at,
                attachment: None,
                reply: reply.map(|excerpt| common::ReplyPreview {
                    id: Uuid::new_v4(),
                    nickname: "bob".into(),
                    excerpt: excerpt.into(),
                }),
            }))),
        );
    };

    say(
        &mut state,
        &bob,
        "поднял сервер на ноуте, зайдите проверить",
        None,
    );
    say(&mut state, &bob, "адрес в закрепе", None);
    say(
        &mut state,
        &carol,
        "alice, глянь логи на всякий случай",
        None,
    );
    say(
        &mut state,
        &UserInfo {
            id: me,
            nickname: "alice".into(),
        },
        "смотрю, там всё тихо",
        Some("alice, глянь логи на всякий случай"),
    );

    // Кто-то печатает — подсказка внизу уступает место живой строке.
    update(
        &mut state,
        Action::Net(NetEvent::Message(ServerMessage::Typing { user: carol })),
    );

    let mut terminal = Terminal::new(TestBackend::new(84, 20)).unwrap();
    terminal
        .draw(|frame| tui::ui::draw(frame, &mut state, &mut tui::images::Images::disabled()))
        .unwrap();
    println!("{}", terminal.backend());

    // Справка поверх переписки.
    state.help = true;
    terminal
        .draw(|frame| tui::ui::draw(frame, &mut state, &mut tui::images::Images::disabled()))
        .unwrap();
    println!("{}", terminal.backend());
    state.help = false;

    // Обзор файлов.
    state.browser = Some(tui::app::Browser {
        dir: std::path::PathBuf::from("C:/Users/egord/Downloads"),
        entries: vec![
            file("..", true, 0),
            file("отпуск", true, 0),
            file("кот.png", false, 245_000),
            file("схема.jpg", false, 1_200_000),
            file("заметки.txt", false, 3_100),
        ],
        selected: 2,
        filter: Default::default(),
        loading: false,
        error: None,
    });
    terminal
        .draw(|frame| tui::ui::draw(frame, &mut state, &mut tui::images::Images::disabled()))
        .unwrap();
    println!("{}", terminal.backend());
}

fn file(name: &str, is_dir: bool, size: u64) -> tui::files::FileEntry {
    tui::files::FileEntry {
        name: name.to_string(),
        path: std::path::PathBuf::from(name),
        is_dir,
        size,
        supported: !is_dir && tui::files::is_supported(std::path::Path::new(name)),
    }
}
