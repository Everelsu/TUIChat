use std::{io, io::Write as _, path::PathBuf, time::Duration};

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
    config::Config,
    host::{self, Hosted},
    images::Images,
    media,
    net::{self, NetConfig, NetHandle},
    sound::Sound,
    ui,
};

#[derive(Parser)]
#[command(about = "Терминальный клиент чата")]
struct Args {
    /// Адрес WebSocket-эндпоинта сервера. По умолчанию — из настроек.
    #[arg(long)]
    server: Option<String>,
    /// Ник. Если не задан, берётся из настроек, а иначе спрашивается на входе.
    #[arg(long)]
    nick: Option<String>,
    /// Комната. По умолчанию — последняя из настроек.
    #[arg(long)]
    room: Option<String>,
    /// Поднять сервер прямо в этом клиенте: второму человеку тогда нужен
    /// только адрес, а отдельный процесс запускать не надо.
    #[arg(long)]
    host: bool,
    /// Порт для `--host`.
    #[arg(long, default_value_t = 8080)]
    port: u16,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();
    // Настройки — это значения по умолчанию, аргументы командной строки их
    // перекрывают: запуск с чужим ником не должен ничего перетирать до входа.
    let config = Config::load();
    let server = args.server.unwrap_or_else(|| config.server.clone());
    let room_arg = args.room.unwrap_or_else(|| config.room.clone());
    let nick_arg = args.nick.or_else(|| config.nickname.clone());

    // Аргументы проверяем до запуска интерфейса: сообщение об ошибке в обычном
    // терминале читается лучше, чем красная строка внутри TUI.
    let nickname = match nick_arg.as_deref().map(validate::clean_nickname) {
        Some(Ok(nickname)) => Some(nickname),
        Some(Err(err)) => fail(err),
        None => None,
    };
    let room = match validate::clean_room(&room_arg) {
        Ok(room) => room,
        Err(err) => fail(err),
    };

    // Паника не должна оставлять терминал в raw-режиме без курсора, а её
    // текст — мелькать вместе с закрывающимся альтернативным экраном.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let path = log_crash(info);

        // Паника в рабочем потоке процесс не убивает: интерфейс продолжает
        // работать, и разбирать терминал из-за неё нельзя — иначе одна битая
        // картинка выбрасывала бы человека из переписки.
        if std::thread::current().name() != Some("main") {
            return;
        }

        let _ = restore();
        match path {
            Some(path) => eprintln!("Клиент упал. Подробности: {}", path.display()),
            None => eprintln!("Клиент упал, и записать журнал не удалось."),
        }
        hook(info);
    }));

    // Сервер поднимаем до интерфейса: если порт занят, честнее сказать об
    // этом в обычном терминале, чем красной строкой внутри чата.
    let hosted = if args.host {
        match host::start(args.port).await {
            Ok(addresses) => Some(addresses),
            Err(err) => {
                eprintln!("не удалось поднять сервер: {err}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    let server = match &hosted {
        Some(hosted) => hosted.url.clone(),
        None => server,
    };

    let (mut terminal, images) = setup()?;
    // Звуковую карту открываем один раз: на каждый сигнал это заметная
    // задержка и щелчок в динамиках.
    let sound = Sound::open();
    let result = run(
        &mut terminal,
        images,
        sound,
        config,
        server,
        nickname,
        room,
        hosted,
    )
    .await;
    restore()?;
    result
}

/// Дописывает падение в журнал рядом с настройками.
///
/// В TUI стандартный вывод занят интерфейсом, поэтому сообщение о панике
/// иначе исчезает вместе с альтернативным экраном, и понять, что случилось,
/// уже нельзя.
fn log_crash(info: &std::panic::PanicHookInfo<'_>) -> Option<PathBuf> {
    let path = tui::config::dir()?.join("crash.log");
    std::fs::create_dir_all(path.parent()?).ok()?;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let when = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let backtrace = std::backtrace::Backtrace::force_capture();
    writeln!(file, "--- {when}\n{info}\n{backtrace}\n").ok()?;
    Some(path)
}

fn fail(err: validate::ValidationError) -> ! {
    eprintln!("{err}");
    std::process::exit(2);
}

#[allow(clippy::too_many_arguments)]
async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut images: Images,
    mut sound: Sound,
    mut config: Config,
    url: String,
    nickname: Option<String>,
    room: String,
    hosted: Option<Hosted>,
) -> io::Result<()> {
    let (actions, mut incoming) = unbounded_channel();
    spawn_input(actions.clone());

    let (mut state, startup) = State::new(nickname, room);
    // Цвета переносим из настроек в состояние: рисование не должно лезть
    // в файлы, а команда /color правит уже готовую таблицу.
    state.last_dir = config.last_dir.clone();
    state.colors = config
        .colors
        .iter()
        .filter_map(|(nickname, value)| Some((nickname.clone(), tui::config::parse_color(value)?)))
        .collect();
    // Ссылки на вложения выводятся из адреса сокета — отдельного параметра
    // для них не нужно.
    state.set_server(url.clone());
    state.media_base = net::media_base(&url);
    // Миниатюры прямо в ленте показываем только там, где терминал умеет
    // настоящую графику: полублоками картинка в десять строк — цветной шум.
    state.inline_images = config.inline_images.unwrap_or_else(|| images.inline_friendly());
    // Адрес для второго человека показываем прямо в переписке: иначе первое,
    // что он спросит, — «а куда подключаться».
    if let Some(hosted) = &hosted {
        for line in hosted.invitations() {
            let _ = actions.send(Action::Info(line));
        }
    }

    let mut network: Option<NetHandle> = None;
    let mut url = url;
    apply(
        &mut network,
        &actions,
        &mut url,
        &mut config,
        &mut state,
        &mut sound,
        startup,
    );

    // По тику крутится спиннер, бегут точки «печатает» и гаснет вспышка у
    // новых сообщений. Восемь кадров в секунду — предел, за которым глаз
    // разницы не видит, а процессор уже греется зря.
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    terminal.draw(|frame| ui::draw(frame, &mut state, &mut images))?;

    loop {
        let action = tokio::select! {
            action = incoming.recv() => match action {
                Some(action) => action,
                None => break,
            },
            _ = ticker.tick() => Action::Tick,
        };

        // Звук — такой же побочный эффект, как звоночек: байтам голосового
        // в состоянии клиента делать нечего.
        let action = match action {
            Action::Voice(Ok(bytes)) => match sound.play_voice(bytes) {
                Ok(()) => Action::Idle,
                Err(reason) => Action::Notice(reason),
            },
            Action::Voice(Err(reason)) => Action::Notice(reason),
            other => other,
        };

        let commands = update(&mut state, action);
        apply(
            &mut network,
            &actions,
            &mut url,
            &mut config,
            &mut state,
            &mut sound,
            commands,
        );

        terminal.draw(|frame| ui::draw(frame, &mut state, &mut images))?;
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

#[allow(clippy::too_many_arguments)]
fn apply(
    network: &mut Option<NetHandle>,
    actions: &UnboundedSender<Action>,
    url: &mut String,
    config: &mut Config,
    state: &mut State,
    sound: &mut Sound,
    commands: Vec<Command>,
) {
    for command in commands {
        match command {
            Command::Send(msg) => {
                if let Some(network) = network.as_ref() {
                    network.send(msg);
                }
            }
            Command::Host(port) => host_here(port, actions.clone()),
            Command::Connect {
                nickname,
                room,
                server,
            } => {
                // Адрес мог смениться: человек вставил чужой на экране входа
                // или поднял свой сервер командой.
                if !server.is_empty() && server != *url {
                    *url = server;
                    state.media_base = net::media_base(url);
                    config.server = url.clone();
                }
                // Прошлое соединение прощается и умолкает: его запоздавшие
                // события не должны всплыть уже в новой комнате.
                if let Some(previous) = network.take() {
                    previous.close();
                }
                *network = Some(net::spawn(
                    NetConfig::new(url.as_str(), nickname, room),
                    actions.clone(),
                ));
            }
            Command::Open(url) => open_in_system_viewer(&url),
            Command::Fetch(id, url) => fetch_image(id, url, actions.clone()),
            Command::Upload(path) => upload_file(state.media_base.clone(), path, actions.clone()),
            Command::ReadDir(path) => read_dir(path, actions.clone()),
            Command::PlayVoice(url) => fetch_voice(url, actions.clone()),
            Command::StopVoice => sound.stop_voice(),
            Command::Save { url, destination } => save_file(url, destination, actions.clone()),
            // Звоночек — единственное уведомление, доступное из терминала:
            // системных всплывашек у нас нет.
            // Свой сигнал слышно и там, где звоночек терминала выключен.
            Command::Bell if sound.is_available() => sound.chime(),
            // Звука нет — остаётся звоночек. Сразу сбрасываем буфер:
            // иначе символ пролежит в нём до следующего вывода.
            Command::Bell => {
                print!("");
                let _ = io::Write::flush(&mut io::stdout());
            }
            Command::SaveConfig => save_config(config, state, actions),
            Command::Quit => {}
        }
    }
}

/// Переносит настройки из состояния в файл.
///
/// Ошибку записи показываем в переписке: молча терять настройки хуже, чем
/// сказать об этом, а прерывать из-за неё работу — тем более незачем.
fn save_config(config: &mut Config, state: &State, actions: &UnboundedSender<Action>) {
    if !state.nickname.is_empty() {
        config.nickname = Some(state.nickname.clone());
    }
    if !state.room.is_empty() {
        config.room = state.room.clone();
    }
    if state.last_dir.is_some() {
        config.last_dir = state.last_dir.clone();
    }
    config
        .colors
        .retain(|nickname, _| state.colors.contains_key(nickname));
    for (nickname, color) in &state.colors {
        config.set_color(nickname, &color_to_hex(*color));
    }

    if let Err(err) = config.save() {
        let _ = actions.send(Action::Notice(format!(
            "не удалось сохранить настройки: {err}"
        )));
    }
}

/// Цвет в виде `#rrggbb` — так его потом можно править руками.
fn color_to_hex(color: ratatui::style::Color) -> String {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Поднимает сервер прямо здесь и докладывает адрес для друга.
fn host_here(port: u16, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        match host::start(port).await {
            Ok(hosted) => {
                let _ = actions.send(Action::Hosted {
                    url: hosted.url.clone(),
                    lines: hosted.invitations(),
                });
            }
            Err(err) => {
                let _ = actions.send(Action::Notice(format!(
                    "не удалось поднять сервер на порту {port}: {err}"
                )));
            }
        }
    });
}

/// Читает каталог в отдельном потоке.
///
/// На сетевом диске это способно думать секундами, а интерфейс всё это время
/// должен продолжать отвечать.
fn read_dir(path: PathBuf, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        let target = path.clone();
        let work = tokio::task::spawn_blocking(move || tui::files::read_dir(&target));
        let result = work
            .await
            .unwrap_or_else(|err| Err(format!("чтение каталога сорвалось: {err}")));
        let _ = actions.send(Action::Directory { dir: path, result });
    });
}

/// Качает голосовое и отдаёт байты главному циклу.
fn fetch_voice(url: String, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        let work = tokio::task::spawn_blocking(move || media::fetch(&url));
        let result = work
            .await
            .unwrap_or_else(|err| Err(format!("скачивание сорвалось: {err}")));
        let _ = actions.send(Action::Voice(result));
    });
}

/// Скачивает вложение и кладёт его на диск.
fn save_file(url: String, destination: PathBuf, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        let work = tokio::task::spawn_blocking(move || {
            let bytes = media::fetch(&url)?;
            if let Some(parent) = destination.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("не удалось создать каталог: {err}"))?;
            }
            std::fs::write(&destination, bytes)
                .map_err(|err| format!("не удалось записать файл: {err}"))?;
            Ok(destination)
        });
        let result = work
            .await
            .unwrap_or_else(|err| Err(format!("сохранение сорвалось: {err}")));
        let _ = actions.send(Action::Saved(result));
    });
}

/// Отправляет файл на сервер в отдельном потоке.
fn upload_file(base: String, path: std::path::PathBuf, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        let work = tokio::task::spawn_blocking(move || media::upload(&base, &path));
        let result = work
            .await
            .unwrap_or_else(|err| Err(format!("отправка сорвалась: {err}")));
        let _ = actions.send(Action::Uploaded(result));
    });
}

/// Качает и разбирает картинку в отдельном потоке.
///
/// Скачивание и декодирование блокируют, а интерфейс должен продолжать
/// отвечать: результат прилетит обычным действием, как сообщение из сети.
fn fetch_image(id: uuid::Uuid, url: String, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        let work = tokio::task::spawn_blocking(move || {
            media::fetch(&url)
                .and_then(|bytes| media::decode(&bytes))
                .map(Box::new)
        });

        // Разборщики чужих форматов на битом файле иногда паникуют. Без этой
        // ветки просмотр навсегда застревал бы на «загружаю…».
        let result = work
            .await
            .unwrap_or_else(|err| Err(format!("разбор картинки сорвался: {err}")));
        let _ = actions.send(Action::Image(id, result));
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
                Err(err) => {
                    // Молчаливо умерший ввод выглядит как зависшая программа,
                    // поэтому о поломке говорим вслух.
                    let _ = actions.send(Action::Notice(format!(
                        "клавиатура больше не читается: {err}"
                    )));
                    break;
                }
            };
            if actions.send(action).is_err() {
                break;
            }
        }
    });
}

fn setup() -> io::Result<(Terminal<CrosstermBackend<io::Stdout>>, Images)> {
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

    // Опрос терминала — строго здесь: он пишет escape-последовательность и
    // ждёт ответ из stdin. Запусти мы раньше чтение клавиатуры, ответ достался
    // бы ему, а в переписку прилетел бы мусор.
    let images = Images::probe();

    Ok((Terminal::new(CrosstermBackend::new(stdout))?, images))
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
