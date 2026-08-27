//! Снимок интерфейса для глаз, а не для утверждений.
//!
//! Запуск: `cargo test -p tui --test preview -- --nocapture`. Тест ничего не
//! проверяет — он печатает экраны, чтобы вёрстку можно было оценить целиком,
//! не поднимая сервер и не открывая терминал.

use common::{ChatMessage, RoomSummary, ServerMessage, UserInfo};
use null_terminal::app::{Action, NetEvent, State, update};
use ratatui::{Terminal, backend::TestBackend};
use uuid::Uuid;

fn user(nickname: &str) -> UserInfo {
    UserInfo {
        id: Uuid::new_v4(),
        nickname: nickname.into(),
    }
}

/// Печатает экран с подписью.
fn show(title: &str, state: &mut State, width: u16, height: u16) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            null_terminal::ui::draw(frame, state, &mut null_terminal::images::Images::disabled())
        })
        .unwrap();
    println!("\n=== {title} ===");
    println!("{}", terminal.backend());
}

/// Домотать переезд вкладки до конца: на снимке нужна не анимация, а вёрстка.
fn settle(state: &mut State) {
    if let null_terminal::app::Screen::Login(login) = &mut state.screen {
        login.switched = std::time::Instant::now() - std::time::Duration::from_secs(1);
    }
}

#[test]
fn home_screen() {
    let (mut state, _) = State::new(None, "general".into());
    state.prefill_nickname("alice");
    state.set_server("ws://192.168.1.5:8080/ws".into());
    update(
        &mut state,
        Action::Rooms(Ok(vec![
            RoomSummary {
                name: "general".into(),
                users: 3,
            },
            RoomSummary {
                name: "rust".into(),
                users: 1,
            },
            RoomSummary {
                name: "курилка".into(),
                users: 7,
            },
        ])),
    );

    settle(&mut state);
    show("главный экран · войти", &mut state, 96, 30);

    // Список устройств на машине сборки не спросишь — показываем ожидаемое.
    state.audio.outputs = vec![
        "Наушники (Realtek High Definition Audio)".into(),
        "Динамики монитора (HDMI)".into(),
    ];
    state.audio.inputs = vec!["Микрофон гарнитуры".into()];

    for title in ["поднять", "вид", "звук", "справка"] {
        update(
            &mut state,
            Action::Key(ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Tab,
                ratatui::crossterm::event::KeyModifiers::NONE,
            )),
        );
        settle(&mut state);
        show(&format!("главный экран · {title}"), &mut state, 96, 30);
    }

    // Узкое окно: заголовок уходит, вкладки схлопываются, форма остаётся.
    let (mut narrow, _) = State::new(None, "general".into());
    settle(&mut narrow);
    show("главный экран · узкое окно", &mut narrow, 48, 16);
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
            upload_limit: common::validate::MAX_UPLOAD_BYTES as u64,
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

    show("переписка", &mut state, 84, 20);

    // Колонка с людьми — по ctrl+p.
    state.sidebar = true;
    show("переписка · колонка людей", &mut state, 96, 20);
    state.sidebar = false;

    // Справка поверх переписки.
    state.help = true;
    show("справка", &mut state, 84, 24);
    state.help_scroll = 12;
    show("справка · пролистана", &mut state, 84, 24);
    state.help_scroll = 0;
    state.help = false;

    // Обзор файлов.
    state.browser = Some(null_terminal::app::Browser {
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
    show("обзор файлов", &mut state, 84, 24);
    state.browser = None;

    // Все четыре темы на одном и том же разговоре.
    for theme in null_terminal::theme::Theme::ALL {
        state.theme = theme;
        show(&format!("тема · {}", theme.title()), &mut state, 84, 12);
    }
}

fn file(name: &str, is_dir: bool, size: u64) -> null_terminal::files::FileEntry {
    null_terminal::files::FileEntry {
        name: name.to_string(),
        path: std::path::PathBuf::from(name),
        is_dir,
        size,
        media: !is_dir && null_terminal::files::is_media(std::path::Path::new(name)),
    }
}
