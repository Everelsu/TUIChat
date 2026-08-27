use std::{io, io::Write as _, path::PathBuf, time::Duration};

use clap::Parser;
use common::validate;
use null_terminal::{
    app::{Action, Command, State, update},
    config::Config,
    host::{self, Hosted},
    images::Images,
    launcher, media,
    net::{self, NetConfig, NetHandle},
    sound::Sound,
    ui,
};
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

#[derive(Parser)]
#[command(name = "null_terminal", about = "null_terminal — чат в терминале")]
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
    /// Перезапускать ли клиент в нормальном терминале: `auto` — только после
    /// двойного клика, `always` — из любого старого окна, `never` — никогда.
    #[arg(long)]
    terminal: Option<String>,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();
    // Настройки — это значения по умолчанию, аргументы командной строки их
    // перекрывают: запуск с чужим ником не должен ничего перетирать до входа.
    let config = Config::load();

    // Перезапуск в нормальном терминале — самое первое дело: интерфейс,
    // собранный из цвета и полублоков, в conhost выглядит сломанным, и
    // человек решает, что сломана программа. Уходим до всякой подготовки,
    // чтобы не оставлять за собой ни поднятого сервера, ни raw-режима.
    let terminal_mode = args
        .terminal
        .as_deref()
        .or(Some(config.terminal.as_str()))
        .and_then(launcher::Mode::parse)
        .unwrap_or_default();
    if launcher::relaunch_if_needed(terminal_mode, &config.terminal_program) {
        return Ok(());
    }
    let server = args.server.unwrap_or_else(|| config.server.clone());
    let room_arg = args.room.unwrap_or_else(|| config.room.clone());
    // Ник из аргументов означает «войти сразу», из настроек — только
    // подставить в поле.
    let nick_arg = args.nick.clone();

    // Аргументы проверяем до запуска интерфейса: сообщение об ошибке в обычном
    // терминале читается лучше, чем красная строка внутри TUI.
    let nickname = match nick_arg.as_deref().map(validate::clean_nickname) {
        Some(Ok(nickname)) => Some(nickname),
        Some(Err(err)) => fail(err),
        None => None,
    };
    let remembered = config
        .nickname
        .as_deref()
        .and_then(|nickname| validate::clean_nickname(nickname).ok());
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
        remembered,
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
    let path = null_terminal::config::dir()?.join("crash.log");
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
    remembered: Option<String>,
    room: String,
    hosted: Option<Hosted>,
) -> io::Result<()> {
    let (actions, mut incoming) = unbounded_channel();
    spawn_input(actions.clone());

    let (mut state, mut startup) = State::new(nickname, room);
    if let Some(remembered) = &remembered {
        state.prefill_nickname(remembered);
    }
    // Цвета переносим из настроек в состояние: рисование не должно лезть
    // в файлы, а команда /color правит уже готовую таблицу.
    state.last_dir = config.last_dir.clone();
    state.colors = config
        .colors
        .iter()
        .filter_map(|(nickname, value)| {
            Some((nickname.clone(), null_terminal::config::parse_color(value)?))
        })
        .collect();
    // Ссылки на вложения выводятся из адреса сокета — отдельного параметра
    // для них не нужно.
    state.set_server(url.clone());
    state.media_base = net::media_base(&url);
    // Миниатюры прямо в ленте показываем только там, где терминал умеет
    // настоящую графику: полублоками картинка в десять строк — цветной шум.
    state.images_auto = images.inline_friendly();
    state.images_choice = config.inline_images;
    state.apply_images();
    // Оформление: тема, колонка людей и то, где открываться заново. Всё это
    // правится на вкладке «вид» и возвращается сюда при следующем запуске.
    state.theme = null_terminal::theme::Theme::parse(&config.theme).unwrap_or_default();
    state.sidebar = config.sidebar;
    // Список устройств спрашиваем один раз: опрос звуковой подсистемы не
    // бесплатный, а между двумя нажатиями стрелки наушники не меняются.
    state.audio = audio_from(&config);
    sound.set_gain(state.audio.volume);
    let chosen = state.audio.output.clone();
    if chosen.is_some() || state.audio.input.is_some() {
        sound.use_devices(chosen.as_deref(), state.audio.input.clone());
    }
    // Из настроек, а не из аргумента запуска: `--terminal` — разовый обход,
    // а на вкладке «вид» человек правит то, что останется.
    state.terminal_mode = launcher::Mode::parse(&config.terminal).unwrap_or_default();
    // Адрес для второго человека показываем прямо в переписке: иначе первое,
    // что он спросит, — «а куда подключаться».
    if let Some(hosted) = &hosted {
        for line in hosted.invitations() {
            let _ = actions.send(Action::Info(line));
        }
    }

    // На экране входа сразу тянем список комнат: человек видит, куда можно
    // зайти, ещё до того, как что-то введёт.
    if matches!(state.screen, null_terminal::app::Screen::Login(_)) {
        startup.push(Command::FetchRooms(state.media_base.clone()));
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

    // По тику крутится спиннер, бежит блик по заголовку, гаснет вспышка у
    // новых сообщений и едет содержимое вкладки. Шестнадцать кадров в
    // секунду: движение уже слитное, а рисование текстового экрана столько
    // раз в секунду не стоит ничего заметного.
    let mut ticker = tokio::time::interval(Duration::from_millis(60));
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
            Action::Voice(id, Ok(bytes)) => {
                // Форму волны считаем по тем же байтам, что и играем: второй
                // раз качать ради графика было бы расточительно.
                let wave = media::waveform(&bytes);
                let outcome = match sound.play_voice(bytes) {
                    Ok(()) => Action::Idle,
                    Err(reason) => Action::Notice(reason),
                };
                // График кладём до проигрывания: даже если звука на машине
                // нет, увидеть длительность и форму — уже польза.
                if let Some(wave) = wave {
                    let _ = actions.send(Action::Waveform(id, wave));
                }
                outcome
            }
            Action::Voice(_, Err(reason)) => Action::Notice(reason),
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

        // Состояние звука знает только звук, а клавише F3 надо решать,
        // включать или останавливать: переносим его в состояние каждый кадр.
        state.playing = sound.is_playing();
        // Досмотренная заливка не должна бежать дальше: когда звук кончился,
        // график возвращается в спокойный вид.
        if !state.playing {
            state.playing_voice = None;
        }

        if sound.is_recording() {
            state.busy = Some(format!(
                "запись {} с · /rec — отправить",
                sound.recorded_seconds()
            ));
        }

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
            Command::FetchRooms(base) => fetch_rooms(base, actions.clone()),
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
            // Уход в меню: соединение закрывается, и его запоздалые события
            // не всплывают уже на главном экране.
            Command::Disconnect => {
                if let Some(previous) = network.take() {
                    previous.close();
                }
            }
            // Выбор устройств применяется на месте: услышать сигнал в новых
            // наушниках — единственный способ убедиться, что выбрал те.
            Command::Audio => {
                let output = state.audio.output.clone();
                let input = state.audio.input.clone();
                sound.set_gain(state.audio.volume);
                if sound.use_devices(output.as_deref(), input) {
                    sound.chime();
                } else if !state.audio.outputs.is_empty() {
                    let _ = actions.send(Action::Notice(
                        "не удалось открыть выбранные динамики".to_string(),
                    ));
                }
            }
            Command::Open(url) => open_in_system_viewer(&url),
            Command::Fetch(id, url) => fetch_image(id, url, actions.clone()),
            Command::Upload(path) => upload_file(
                state.media_base.clone(),
                path,
                state.upload_limit,
                actions.clone(),
            ),
            Command::ReadDir(path) => read_dir(path, actions.clone()),
            Command::PlayVoice(id, url) => fetch_voice(id, url, actions.clone()),
            Command::StopVoice => sound.stop_voice(),
            Command::ToggleRecording => toggle_recording(sound, state, actions.clone()),
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
    config.theme = state.theme.name().to_string();
    config.sound_output = state.audio.output.clone().unwrap_or_default();
    config.sound_input = state.audio.input.clone().unwrap_or_default();
    config.chime = state.audio.chime;
    config.volume = state.audio.volume as u8;
    config.sidebar = state.sidebar;
    config.inline_images = state.images_choice;
    config.terminal = match state.terminal_mode {
        launcher::Mode::Auto => "auto",
        launcher::Mode::Always => "always",
        launcher::Mode::Never => "never",
    }
    .to_string();
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

/// Собирает состояние звука: что нашлось в системе и что из этого выбрано.
///
/// Выбранное устройство могло исчезнуть вместе с наушниками — тогда выбор
/// молча становится системным: показывать в настройках имя того, чего больше
/// нет, значит врать.
fn audio_from(config: &Config) -> null_terminal::app::Audio {
    let outputs = null_terminal::sound::outputs();
    let inputs = null_terminal::sound::inputs();
    let keep = |name: &str, list: &[String]| {
        let name = name.trim();
        (!name.is_empty() && list.iter().any(|found| found == name)).then(|| name.to_string())
    };
    null_terminal::app::Audio {
        output: keep(&config.sound_output, &outputs),
        input: keep(&config.sound_input, &inputs),
        chime: config.chime,
        volume: (config.volume as usize).min(null_terminal::sound::GAINS.len() - 1),
        outputs,
        inputs,
    }
}

/// Цвет в виде `#rrggbb` — так его потом можно править руками.
fn color_to_hex(color: ratatui::style::Color) -> String {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Начинает запись или заканчивает её и отправляет.
///
/// Одна команда на оба действия: во время записи всё равно ничего другого
/// не делаешь, а помнить две — лишнее.
fn toggle_recording(sound: &mut Sound, state: &mut State, actions: UnboundedSender<Action>) {
    if !sound.is_recording() {
        match sound.start_recording() {
            Ok(()) => state.busy = Some("запись · /rec — отправить".to_string()),
            Err(reason) => {
                let _ = actions.send(Action::Notice(reason));
            }
        }
        return;
    }

    let bytes = match sound.stop_recording() {
        Ok(bytes) => bytes,
        Err(reason) => {
            state.busy = None;
            let _ = actions.send(Action::Notice(reason));
            return;
        }
    };
    state.busy = Some("отправляю голосовое".to_string());

    let base = state.media_base.clone();
    let limit = state.upload_limit;
    tokio::spawn(async move {
        let result = media::upload_any(base, "голосовое.wav".to_string(), bytes, limit).await;
        let _ = actions.send(Action::Uploaded(result));
    });
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
                // Держим поднятое живым до конца работы: уронив `Hosted`,
                // мы закрыли бы туннель, и друг остался бы с тикетом,
                // по которому уже никто не отвечает.
                let _hosted = hosted;
                std::future::pending::<()>().await;
            }
            Err(err) => {
                let _ = actions.send(Action::Notice(format!(
                    "не удалось поднять сервер на порту {port}: {err}"
                )));
            }
        }
    });
}

/// Спрашивает у сервера список комнат для экрана входа.
///
/// Обычный `GET /rooms` по тому же http, что и вложения. Ошибку не прячем —
/// человеку показывается, почему список пуст (сервер не поднят, чужой адрес,
/// https), а вход это всё равно не блокирует.
fn fetch_rooms(base: String, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        let url = format!("{}/rooms", base.trim_end_matches('/'));
        let result = media::fetch_any(url).await.and_then(|bytes| {
            serde_json::from_slice::<Vec<common::RoomSummary>>(&bytes)
                .map_err(|err| format!("сервер ответил не списком комнат: {err}"))
        });
        let _ = actions.send(Action::Rooms(result));
    });
}

/// Читает каталог в отдельном потоке.
///
/// На сетевом диске это способно думать секундами, а интерфейс всё это время
/// должен продолжать отвечать.
fn read_dir(path: PathBuf, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        let target = path.clone();
        let work = tokio::task::spawn_blocking(move || null_terminal::files::read_dir(&target));
        let result = work
            .await
            .unwrap_or_else(|err| Err(format!("чтение каталога сорвалось: {err}")));
        let _ = actions.send(Action::Directory { dir: path, result });
    });
}

/// Качает голосовое и отдаёт байты главному циклу.
fn fetch_voice(id: uuid::Uuid, url: String, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        let result = media::fetch_any(url).await;
        let _ = actions.send(Action::Voice(id, result));
    });
}

/// Скачивает вложение и кладёт его на диск.
fn save_file(url: String, destination: PathBuf, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        let result = match media::fetch_any(url).await {
            Ok(bytes) => tokio::task::spawn_blocking(move || {
                if let Some(parent) = destination.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)
                        .map_err(|err| format!("не удалось создать каталог: {err}"))?;
                }
                std::fs::write(&destination, bytes)
                    .map_err(|err| format!("не удалось записать файл: {err}"))?;
                Ok(destination)
            })
            .await
            .unwrap_or_else(|err| Err(format!("сохранение сорвалось: {err}"))),
            Err(reason) => Err(reason),
        };
        let _ = actions.send(Action::Saved(result));
    });
}

/// Отправляет файл на сервер в отдельном потоке.
fn upload_file(
    base: String,
    path: std::path::PathBuf,
    limit: usize,
    actions: UnboundedSender<Action>,
) {
    tokio::spawn(async move {
        // Читаем файл отдельным потоком: на сетевом диске это думает секундами.
        let read = tokio::task::spawn_blocking(move || {
            let bytes =
                std::fs::read(&path).map_err(|err| format!("не удалось прочитать файл: {err}"))?;
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "файл".to_string());
            Ok::<_, String>((name, bytes))
        })
        .await
        .unwrap_or_else(|err| Err(format!("отправка сорвалась: {err}")));

        let result = match read {
            Ok((name, bytes)) => media::upload_any(base, name, bytes, limit).await,
            Err(reason) => Err(reason),
        };
        let _ = actions.send(Action::Uploaded(result));
    });
}

/// Качает и разбирает картинку в отдельном потоке.
///
/// Скачивание и декодирование блокируют, а интерфейс должен продолжать
/// отвечать: результат прилетит обычным действием, как сообщение из сети.
fn fetch_image(id: uuid::Uuid, url: String, actions: UnboundedSender<Action>) {
    tokio::spawn(async move {
        let result = match media::fetch_any(url).await {
            // Разборщики чужих форматов на битом файле иногда паникуют. Без
            // отдельного потока и этой ветки просмотр навсегда застревал бы
            // на «загружаю…».
            Ok(bytes) => tokio::task::spawn_blocking(move || media::decode(&bytes).map(Box::new))
                .await
                .unwrap_or_else(|err| Err(format!("разбор картинки сорвался: {err}"))),
            Err(reason) => Err(reason),
        };
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
                    // Щелчок по ленте: строку под курсором интерфейс
                    // сопоставит с сообщением сам — здесь известны только
                    // экранные координаты.
                    MouseEventKind::Down(event::MouseButton::Left) => {
                        Action::Click(mouse.column, mouse.row)
                    }
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
