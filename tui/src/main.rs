use std::{io, time::Duration};

use clap::Parser;
use common::validate;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{
            self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
            EnableMouseCapture, Event, MouseEventKind,
        },
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tui::{
    app::{Action, Command, State, update},
    media,
    net::{self, NetConfig, NetHandle},
    ui,
};

#[derive(Parser)]
#[command(about = "Терминальный клиент чата")]
struct Args {
    /// Адрес WebSocket-эндпоинта сервера.
    #[arg(long, default_value = "ws://127.0.0.1:8080/ws")]
    server: String,
    /// Ник. Если не задан, клиент спросит его на экране входа.
    #[arg(long)]
    nick: Option<String>,
    /// Комната.
    #[arg(long, default_value = "general")]
    room: String,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();

    // Аргументы проверяем до запуска интерфейса: сообщение об ошибке в обычном
    // терминале читается лучше, чем красная строка внутри TUI.
    let nickname = match args.nick.as_deref().map(validate::clean_nickname) {
        Some(Ok(nickname)) => Some(nickname),
        Some(Err(err)) => fail(err),
        None => None,
    };
    let room = match validate::clean_room(&args.room) {
        Ok(room) => room,
        Err(err) => fail(err),
    };

    // Паника в чужом коде не должна оставлять терминал в raw-режиме без курсора.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        hook(info);
    }));

    let mut terminal = setup()?;
    let result = run(&mut terminal, args.server, nickname, room).await;
    restore()?;
    result
}

fn fail(err: validate::ValidationError) -> ! {
    eprintln!("{err}");
    std::process::exit(2);
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    url: String,
    nickname: Option<String>,
    room: String,
) -> io::Result<()> {
    let (actions, mut incoming) = unbounded_channel();
    spawn_input(actions.clone());

    let (mut state, startup) = State::new(nickname, room);
    // Ссылки на вложения выводятся из адреса сокета — отдельного параметра
    // для них не нужно.
    state.media_base = net::media_base(&url);
    let mut network: Option<NetHandle> = None;
    apply(&mut network, &actions, &url, startup);

    // Тик нужен не для анимации, а чтобы тикал обратный отсчёт до реконнекта.
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    terminal.draw(|frame| ui::draw(frame, &mut state))?;

    loop {
        let action = tokio::select! {
            action = incoming.recv() => match action {
                Some(action) => action,
                None => break,
            },
            _ = ticker.tick() => Action::Tick,
        };

        let commands = update(&mut state, action);
        apply(&mut network, &actions, &url, commands);

        terminal.draw(|frame| ui::draw(frame, &mut state))?;
        // Выходим после отрисовки: человек должен успеть увидеть последний кадр.
        if state.should_quit {
            break;
        }
    }

    if let Some(network) = network {
        network.shutdown().await;
    }
    Ok(())
}

fn apply(
    network: &mut Option<NetHandle>,
    actions: &UnboundedSender<Action>,
    url: &str,
    commands: Vec<Command>,
) {
    for command in commands {
        match command {
            Command::Send(msg) => {
                if let Some(network) = network.as_ref() {
                    network.send(msg);
                }
            }
            Command::Connect { nickname, room } => {
                // Прошлое соединение прощается и умолкает: его запоздавшие
                // события не должны всплыть уже в новой комнате.
                if let Some(previous) = network.take() {
                    previous.close();
                }
                *network = Some(net::spawn(
                    NetConfig::new(url, nickname, room),
                    actions.clone(),
                ));
            }
            Command::Open(url) => open_in_system_viewer(&url),
            Command::Fetch(url) => fetch_image(url, actions.clone()),
            // Звоночек — единственное уведомление, доступное из терминала:
            // системных всплывашек у нас нет.
            Command::Bell => print!(""),
            Command::Quit => {}
        }
    }
}

/// Качает и разбирает картинку в отдельном потоке.
///
/// Скачивание и декодирование блокируют, а интерфейс должен продолжать
/// отвечать: результат прилетит обычным действием, как сообщение из сети.
fn fetch_image(url: String, actions: UnboundedSender<Action>) {
    tokio::task::spawn_blocking(move || {
        let result = media::fetch(&url)
            .and_then(|bytes| media::decode(&bytes))
            .map(Box::new);
        let _ = actions.send(Action::Image(result));
    });
}

/// Открывает адрес тем, чем система открывает такие адреса обычно.
///
/// Ошибку глотаем намеренно: если открывать нечем, ругаться в терминал,
/// который сейчас занят интерфейсом, всё равно некуда.
fn open_in_system_viewer(url: &str) {
    let mut command = if cfg!(target_os = "windows") {
        let mut command = std::process::Command::new("cmd");
        // Пустая строка — это заголовок окна: без неё start примет за него
        // сам адрес и ничего не откроет.
        command.args(["/C", "start", "", url]);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    let _ = command.spawn();
}

/// Клавиатуру читаем отдельным системным потоком.
///
/// Асинхронный `EventStream` из crossterm потребовал бы держать crossterm
/// прямой зависимостью и вручную сводить его версию с той, что внутри ratatui.
/// Поток с блокирующим `read()` от этого избавляет и умирает вместе с процессом.
fn spawn_input(actions: UnboundedSender<Action>) {
    std::thread::spawn(move || {
        loop {
            let action = match event::read() {
                Ok(Event::Key(key)) => Action::Key(key),
                Ok(Event::Paste(text)) => Action::Paste(text),
                Ok(Event::Mouse(mouse)) => match mouse.kind {
                    MouseEventKind::ScrollUp => Action::Scroll(3),
                    MouseEventKind::ScrollDown => Action::Scroll(-3),
                    _ => continue,
                },
                // Перерисовать после изменения размера окна.
                Ok(Event::Resize(..)) => Action::Tick,
                Ok(_) => continue,
                Err(_) => break,
            };
            if actions.send(action).is_err() {
                break;
            }
        }
    });
}

fn setup() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Bracketed paste: без него вставка нескольких строк придёт как череда
    // нажатий Enter и разошлётся в комнату по кускам.
    // Перехват мыши нужен ради прокрутки колесом. Выделение текста он ломает,
    // но во всех современных терминалах его возвращает Shift с зажатой мышью.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore() -> io::Result<()> {
    execute!(
        io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    disable_raw_mode()
}
