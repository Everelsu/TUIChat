//! Состояние клиента и переходы между состояниями.
//!
//! Логика намеренно вынесена из рендера и из сети: `update` — обычная функция
//! без ввода-вывода, поэтому поведение клиента проверяется юнит-тестами, а не
//! глазами в терминале.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use common::{
    Attachment, AttachmentKind, ChatMessage, ClientMessage, REPLY_EXCERPT_CHARS, ReplyPreview,
    ServerMessage, UserInfo, validate,
};
use image::RgbImage;
use ratatui::style::Color;

use crate::{config, files::FileEntry};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use uuid::Uuid;

/// Сколько сообщений держим в истории. Без потолка длинная сессия съедает
/// память, а прокрутка на десятки тысяч строк всё равно бесполезна.
pub const MAX_ENTRIES: usize = 2000;

/// Сколько отправленных строк можно пролистать стрелкой вверх.
const MAX_SENT: usize = 100;

const SCROLL_STEP: usize = 10;

pub const HELP: &[&str] = &[
    "/join <комната> — перейти в другую комнату",
    "/nick <ник> — сменить ник",
    "/send [путь] — отправить файл, без пути — выбрать",
    "/view — показать картинку в терминале",
    "/rec — записать голосовое, повторно — отправить",
    "/play, /stop — проиграть голосовое и остановить",
    "/save [путь] — сохранить вложение на диск",
    "/open — открыть вложение внешней программой",
    "/color [ник] <цвет> — цвет ника, «-» сбрасывает",
    "/host [порт] — поднять свой сервер и позвать друга",
    "/clear — очистить историю на экране",
    "/quit — выход",
    "//текст — отправить текст со слэша в начале",
];

/// Команды для дополнения по Tab. Порядок — как в справке.
const COMMANDS: [&str; 14] = [
    "/help", "/join", "/nick", "/send", "/view", "/play", "/stop", "/save", "/open", "/color",
    "/clear", "/host", "/rec", "/quit",
];

/// Однострочное поле ввода: текст и позиция курсора.
///
/// Курсор считается в символах, а не в байтах: с байтами кириллица режется
/// посередине и строка перестаёт быть валидным UTF-8.
#[derive(Debug, Default, Clone)]
pub struct Input {
    pub text: String,
    pub cursor: usize,
}

impl Input {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        Self { text, cursor }
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn byte_at(&self, index: usize) -> usize {
        self.text
            .char_indices()
            .nth(index)
            .map_or(self.text.len(), |(at, _)| at)
    }

    fn insert(&mut self, text: &str, limit: usize) {
        for ch in text.chars() {
            if self.len() >= limit {
                break;
            }
            let at = self.byte_at(self.cursor);
            self.text.insert(at, ch);
            self.cursor += 1;
        }
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let at = self.byte_at(self.cursor - 1);
            self.text.remove(at);
            self.cursor -= 1;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.len() {
            let at = self.byte_at(self.cursor);
            self.text.remove(at);
        }
    }

    /// Удаляет слово перед курсором вместе с прилипшими к нему пробелами.
    fn kill_word(&mut self) {
        let mut end = self.cursor;
        let chars: Vec<char> = self.text.chars().collect();
        while end > 0 && chars[end - 1].is_whitespace() {
            end -= 1;
        }
        while end > 0 && !chars[end - 1].is_whitespace() {
            end -= 1;
        }
        let from = self.byte_at(end);
        let to = self.byte_at(self.cursor);
        self.text.replace_range(from..to, "");
        self.cursor = end;
    }

    fn kill_to_start(&mut self) {
        let to = self.byte_at(self.cursor);
        self.text.replace_range(..to, "");
        self.cursor = 0;
    }

    fn kill_to_end(&mut self) {
        let from = self.byte_at(self.cursor);
        self.text.truncate(from);
    }

    fn set(&mut self, text: impl Into<String>) {
        *self = Input::new(text);
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
}

/// События от сетевой задачи.
#[derive(Debug, Clone)]
pub enum NetEvent {
    Connecting {
        attempt: u32,
    },
    Message(ServerMessage),
    Disconnected {
        reason: String,
        retry_at: Instant,
    },
    /// Переподключение не поможет: ник занят, комната кривая и т.п.
    Fatal {
        reason: String,
    },
}

/// Обзор файлов поверх переписки.
#[derive(Debug)]
pub struct Browser {
    pub dir: std::path::PathBuf,
    pub entries: Vec<FileEntry>,
    pub selected: usize,
    /// Отсев по имени: в каталоге на сотню файлов стрелками не находишься.
    pub filter: Input,
    pub loading: bool,
    pub error: Option<String>,
}

impl Browser {
    /// Строки, оставшиеся после отсева.
    pub fn visible(&self) -> Vec<&FileEntry> {
        let needle = self.filter.text.to_lowercase();
        self.entries
            .iter()
            .filter(|entry| {
                // «..» не прячем никогда: иначе из отфильтрованного каталога
                // некуда деться.
                entry.name == ".."
                    || needle.is_empty()
                    || entry.name.to_lowercase().contains(&needle)
            })
            .collect()
    }

    pub fn current(&self) -> Option<&FileEntry> {
        self.visible().get(self.selected).copied()
    }

    fn move_by(&mut self, delta: i32) {
        let count = self.visible().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        let next = (self.selected as i32 + delta).clamp(0, count as i32 - 1);
        self.selected = next as usize;
    }
}

/// Миниатюра картинки, показываемой прямо в ленте.
#[derive(Debug)]
pub enum Thumbnail {
    Loading,
    Ready(Box<RgbImage>),
    Failed,
}

/// Что показывает просмотрщик картинок поверх переписки.
#[derive(Debug)]
pub enum ViewerState {
    Loading,
    Ready(Box<RgbImage>),
    Failed(String),
}

#[derive(Debug)]
pub struct Viewer {
    /// Идентификатор вложения: по нему терминал понимает, что картинка та же
    /// и перекодировать её заново не нужно.
    pub id: Uuid,
    pub name: String,
    pub state: ViewerState,
}

/// Размер действия определяется самым большим вариантом — приветствием со
/// списком участников и историей. Складывать его в Box смысла нет: по каналу
/// проходит несколько сообщений в секунду, а лишняя аллокация была бы на
/// каждое нажатие клавиши.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Action {
    Key(KeyEvent),
    Paste(String),
    Net(NetEvent),
    /// Картинка скачана и разобрана — или не вышло. Идентификатор нужен,
    /// чтобы понять, куда её класть: в просмотр или в ленту.
    Image(Uuid, Result<Box<RgbImage>, String>),
    /// Файл загружен на сервер — или не вышло.
    Uploaded(Result<Attachment, String>),
    /// Вложение сохранено на диск — или не вышло.
    Saved(Result<std::path::PathBuf, String>),
    /// Каталог прочитан — или не вышло.
    Directory {
        dir: std::path::PathBuf,
        result: Result<Vec<FileEntry>, String>,
    },
    /// Голосовое скачано: байты уходят в звук, минуя состояние клиента.
    Voice(Result<Vec<u8>, String>),
    /// Сервер поднят прямо здесь: адрес для себя и строки-приглашения.
    Hosted {
        url: String,
        lines: Vec<String>,
    },
    /// Прокрутка колесом: вверх — положительное число строк.
    Scroll(i32),
    /// Сообщение от самого клиента: сломался ввод, не записался конфиг и т.п.
    Notice(String),
    /// Спокойное сообщение от клиента: адрес для друга, ход дела.
    Info(String),
    /// Долгая работа закончилась: убрать бегунок.
    Idle,
    /// Перерисовка по таймеру: нужна, чтобы тикал обратный отсчёт до реконнекта.
    Tick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting { attempt: u32 },
    Online,
    Reconnecting { reason: String, retry_at: Instant },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Field {
    #[default]
    Nickname,
    Room,
    /// Адрес сервера. Без него подключиться к чужому серверу можно было бы
    /// только флагом при запуске — то есть никак, если клиент уже открыт.
    Server,
}

/// Экран входа. Он же — место, куда клиент возвращается, если ник занят:
/// человек правит одно поле и пробует снова, не перезапуская программу.
#[derive(Debug, Clone, Default)]
pub struct Login {
    pub nickname: Input,
    pub room: Input,
    pub server: Input,
    pub field: Field,
    pub error: Option<String>,
}

impl Login {
    fn active(&mut self) -> &mut Input {
        match self.field {
            Field::Nickname => &mut self.nickname,
            Field::Room => &mut self.room,
            Field::Server => &mut self.server,
        }
    }

    fn limit(&self) -> usize {
        match self.field {
            Field::Nickname => validate::MAX_NICKNAME_CHARS,
            Field::Room => validate::MAX_ROOM_CHARS,
            Field::Server => validate::MAX_TEXT_CHARS,
        }
    }

    fn next_field(&mut self, back: bool) {
        self.field = match (self.field, back) {
            (Field::Nickname, false) => Field::Room,
            (Field::Room, false) => Field::Server,
            (Field::Server, false) => Field::Nickname,
            (Field::Nickname, true) => Field::Server,
            (Field::Room, true) => Field::Nickname,
            (Field::Server, true) => Field::Room,
        };
    }
}

#[derive(Debug, Clone)]
pub enum Screen {
    Login(Login),
    Chat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemKind {
    Info,
    Join,
    Leave,
    Error,
}

/// Записи почти все — реплики, системные строки редки. Уносить содержимое
/// реплики в Box значило бы аллокацию на каждое сообщение и разыменовку на
/// каждой отрисовке ради экономии на редком варианте: при потолке в 2000
/// записей речь идёт о полумегабайте.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Entry {
    Chat {
        id: Uuid,
        from: String,
        text: String,
        ts: i64,
        /// Своё сообщение подсвечивается иначе — иначе в плотной переписке
        /// не видно, дошло ли отправленное.
        mine: bool,
        /// В сообщении упомянут наш ник.
        mentions_me: bool,
        /// Картинку в терминале не показать, поэтому рядом печатается ссылка.
        attachment: Option<Attachment>,
        /// Цитата сообщения, на которое отвечают.
        reply: Option<ReplyPreview>,
        /// Когда реплика появилась на экране: по ней рисуется вспышка,
        /// подсказывающая, что именно сейчас пришло.
        arrived: Instant,
    },
    System {
        text: String,
        kind: SystemKind,
    },
}

/// Что главному циклу нужно сделать после обработки действия.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Send(ClientMessage),
    /// Поднять соединение заново — при входе, смене комнаты, ника или сервера.
    Connect {
        nickname: String,
        room: String,
        server: String,
    },
    /// Поднять сервер прямо в этом клиенте.
    Host(u16),
    /// Открыть адрес системным просмотрщиком.
    Open(String),
    /// Скачать картинку, чтобы показать её в терминале.
    Fetch(Uuid, String),
    /// Отправить файл на сервер и приложить его к сообщению.
    Upload(std::path::PathBuf),
    /// Прочитать каталог для обзора файлов.
    ReadDir(std::path::PathBuf),
    /// Скачать и проиграть голосовое.
    PlayVoice(String),
    /// Остановить проигрывание.
    StopVoice,
    /// Начать запись с микрофона или закончить её и отправить.
    ToggleRecording,
    /// Скачать вложение и положить его на диск.
    Save {
        url: String,
        destination: std::path::PathBuf,
    },
    /// Звоночек терминала: единственное уведомление, доступное из TUI.
    Bell,
    /// Записать настройки на диск: ник, комнату и цвета.
    SaveConfig,
    Quit,
}

/// Сообщение, на которое сейчас готовится ответ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTarget {
    pub id: Uuid,
    pub nickname: String,
    pub excerpt: String,
}

/// Поиск по истории.
#[derive(Debug, Default)]
pub struct Search {
    pub query: Input,
    /// Номера подходящих записей, по возрастанию.
    pub matches: Vec<usize>,
    /// Какое совпадение сейчас выбрано.
    pub current: usize,
}

impl Search {
    pub fn current_entry(&self) -> Option<usize> {
        self.matches.get(self.current).copied()
    }

    pub fn is_match(&self, entry: usize) -> bool {
        self.matches.binary_search(&entry).is_ok()
    }
}

/// Перебор ников по Tab.
#[derive(Debug, Clone)]
pub struct Completion {
    /// Что человек успел набрать до первого Tab.
    prefix: String,
    /// Позиция начала слова в символах.
    at: usize,
    index: usize,
}

/// Размеры области сообщений, известные только после отрисовки.
/// Нужны, чтобы прокрутка не уезжала за пределы истории.
#[derive(Debug, Default, Clone, Copy)]
pub struct Viewport {
    pub height: usize,
    pub total_lines: usize,
}

impl Viewport {
    fn max_scroll(&self) -> usize {
        self.total_lines.saturating_sub(self.height)
    }
}

#[derive(Debug)]
pub struct State {
    pub screen: Screen,
    pub nickname: String,
    pub room: String,
    /// Адрес сервера, к которому подключаемся.
    pub server: String,
    /// http-адрес сервера: из него собираются ссылки на вложения.
    pub media_base: String,
    /// Цвета ников, заданные человеком. Ключ — ник в нижнем регистре.
    pub colors: HashMap<String, Color>,
    /// Каталог, в котором последний раз выбирали файл.
    pub last_dir: Option<String>,
    /// Картинки, показываемые прямо в переписке. Ключ — идентификатор вложения.
    pub thumbnails: HashMap<Uuid, Thumbnail>,
    /// Что клиент делает прямо сейчас: качает, отправляет, сохраняет.
    /// Пока не пусто, внизу крутится бегунок.
    pub busy: Option<String>,
    /// Умеет ли терминал настоящую графику. Полублоками миниатюра в несколько
    /// строк превращается в цветной шум, поэтому там остаётся строка с именем.
    pub inline_images: bool,
    /// Кто сейчас печатает и когда об этом сказали в последний раз.
    pub typing: HashMap<Uuid, (String, Instant)>,
    /// Когда мы сами последний раз сообщили, что печатаем.
    typing_sent: Option<Instant>,
    pub status: Status,
    pub me: Option<Uuid>,
    pub users: Vec<UserInfo>,
    pub entries: Vec<Entry>,
    /// id уже показанных реплик: по ним отбрасываются дубли, когда после
    /// переподключения история комнаты накладывается на уже увиденное.
    seen: HashSet<Uuid>,
    pub input: Input,
    /// Открытый поиск по истории.
    pub search: Option<Search>,
    /// Номер записи, которую сейчас выбирают для ответа.
    pub picking: Option<usize>,
    /// Выбранное сообщение: уйдёт вместе со следующей репликой.
    pub replying: Option<ReplyTarget>,
    /// Номер первой строки каждой записи после последней отрисовки.
    /// Заполняется при рендере: только там известно, во сколько строк
    /// развернулось сообщение при текущей ширине окна.
    pub entry_lines: Vec<usize>,
    completion: Option<Completion>,
    /// Отправленные строки для листания стрелками.
    sent: Vec<String>,
    sent_cursor: Option<usize>,
    draft: String,
    /// На сколько строк история прокручена вверх от низа.
    pub scrollback: usize,
    pub viewport: Viewport,
    /// Счётчик тиков — по нему крутится спиннер подключения.
    pub tick: u64,
    /// Открытая картинка поверх переписки.
    pub viewer: Option<Viewer>,
    /// Открытый обзор файлов.
    pub browser: Option<Browser>,
    /// Открыта ли справка. Она тоже поверх: одиннадцать строк в ленте
    /// выталкивают из виду сам разговор, ради которого её и открывали.
    pub help: bool,
    pub should_quit: bool,
}

/// Сколько показываем «печатает», если новых сигналов не приходит.
///
/// Сигнал одноразовый: отменять его отдельным сообщением не нужно, он гаснет
/// сам — так не приходится думать про потерянные «я перестал печатать».
pub const TYPING_TTL: Duration = Duration::from_secs(4);

/// Как часто сообщаем, что печатаем.
const TYPING_EVERY: Duration = Duration::from_secs(2);

impl State {
    /// Кто печатает прямо сейчас, в алфавитном порядке.
    pub fn typing_now(&self) -> Vec<&str> {
        let now = Instant::now();
        let mut names: Vec<&str> = self
            .typing
            .values()
            .filter(|(_, at)| now.duration_since(*at) < TYPING_TTL)
            .map(|(nickname, _)| nickname.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    /// Создаёт состояние.
    ///
    /// Экран входа пропускается, только если ник задан явно при запуске:
    /// это осознанное «войди сразу». Запомненный ник такого не означает.
    pub fn new(nickname: Option<String>, room: String) -> (Self, Vec<Command>) {
        let mut state = Self {
            screen: Screen::Login(Login {
                nickname: Input::new(nickname.clone().unwrap_or_default()),
                room: Input::new(room.clone()),
                server: Input::default(),
                field: Field::Nickname,
                error: None,
            }),
            nickname: String::new(),
            room: room.clone(),
            server: String::new(),
            media_base: String::new(),
            colors: HashMap::new(),
            last_dir: None,
            thumbnails: HashMap::new(),
            busy: None,
            inline_images: false,
            typing: HashMap::new(),
            typing_sent: None,
            status: Status::Connecting { attempt: 0 },
            me: None,
            users: Vec::new(),
            entries: Vec::new(),
            seen: HashSet::new(),
            input: Input::default(),
            search: None,
            picking: None,
            replying: None,
            entry_lines: Vec::new(),
            completion: None,
            sent: Vec::new(),
            sent_cursor: None,
            draft: String::new(),
            scrollback: 0,
            viewport: Viewport::default(),
            tick: 0,
            viewer: None,
            browser: None,
            help: false,
            should_quit: false,
        };

        match nickname {
            Some(nickname) => {
                state.nickname = nickname.clone();
                state.screen = Screen::Chat;
                // Адрес подставит главный цикл: он знает и настройки,
                // и аргументы командной строки.
                (
                    state,
                    vec![Command::Connect {
                        nickname,
                        room,
                        server: String::new(),
                    }],
                )
            }
            None => (state, Vec::new()),
        }
    }

    /// Подставляет запомненный ник в поле входа.
    ///
    /// Именно подставляет, а не входит: ник из настроек — это подсказка,
    /// чтобы не набирать его заново, а не согласие войти немедленно. Иначе
    /// сменить комнату или сервер стало бы невозможно — экран входа просто
    /// не показывался бы.
    pub fn prefill_nickname(&mut self, nickname: &str) {
        if let Screen::Login(login) = &mut self.screen {
            login.nickname = Input::new(nickname);
        }
        self.nickname = nickname.to_string();
    }

    /// Задаёт адрес сервера — и в состоянии, и в поле экрана входа.
    pub fn set_server(&mut self, url: String) {
        if let Screen::Login(login) = &mut self.screen {
            login.server = Input::new(url.clone());
        }
        self.server = url;
    }

    pub fn is_online(&self) -> bool {
        matches!(self.status, Status::Online)
    }

    fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
        while self.entries.len() > MAX_ENTRIES {
            // Вместе с записью забываем её id, иначе множество виденных
            // сообщений росло бы вечно.
            if let Entry::Chat { id, .. } = self.entries.remove(0) {
                self.seen.remove(&id);
            }
        }
    }

    fn system(&mut self, kind: SystemKind, text: impl Into<String>) {
        self.push(Entry::System {
            text: text.into(),
            kind,
        });
    }

    /// Добавляет реплику, если её ещё не показывали.
    /// Возвращает `true`, если в ней упомянули нас.
    fn push_chat(&mut self, message: ChatMessage) -> bool {
        // Раз пришло сообщение, печатать человек закончил.
        self.typing.remove(&message.from.id);
        if !self.seen.insert(message.id) {
            return false;
        }
        let mine = self.me == Some(message.from.id);
        let mentions_me = !mine && mentions(&message.text, &self.nickname);
        self.push(Entry::Chat {
            id: message.id,
            from: message.from.nickname,
            text: message.text,
            ts: message.ts,
            mine,
            mentions_me,
            attachment: message.attachment,
            reply: message.reply,
            arrived: Instant::now(),
        });
        mentions_me
    }

    fn forget_room(&mut self) {
        self.entries.clear();
        self.seen.clear();
        self.thumbnails.clear();
        self.users.clear();
        self.scrollback = 0;
    }

    fn sort_users(&mut self) {
        self.users
            .sort_by_key(|user| validate::nickname_key(&user.nickname));
    }

    fn remember_sent(&mut self, line: &str) {
        if self.sent.last().map(String::as_str) != Some(line) {
            self.sent.push(line.to_string());
            if self.sent.len() > MAX_SENT {
                self.sent.remove(0);
            }
        }
        self.sent_cursor = None;
        self.draft.clear();
    }

    /// Листание истории ввода: `-1` — вверх, `+1` — вниз.
    fn recall(&mut self, direction: i32) {
        if self.sent.is_empty() {
            return;
        }
        let next = match (self.sent_cursor, direction) {
            (None, -1) => {
                self.draft = self.input.text.clone();
                Some(self.sent.len() - 1)
            }
            (Some(0), -1) => Some(0),
            (Some(index), -1) => Some(index - 1),
            (Some(index), 1) if index + 1 < self.sent.len() => Some(index + 1),
            // Дошли до конца истории — возвращаем то, что человек набирал.
            (Some(_), 1) => None,
            _ => return,
        };

        self.sent_cursor = next;
        match next {
            Some(index) => {
                let line = self.sent[index].clone();
                self.input.set(line);
            }
            None => {
                let draft = std::mem::take(&mut self.draft);
                self.input.set(draft);
            }
        }
    }
}

/// Сколько последних записей просматриваем в поисках картинок для ленты.
///
/// Дальше вверх человек редко уходит, а качать всю историю сразу — лишний
/// трафик и лишняя память.
const THUMBNAIL_LOOKBACK: usize = 30;

/// Ставит в очередь скачивание картинок, которых ещё нет.
fn queue_thumbnails(state: &mut State) -> Vec<Command> {
    if !state.inline_images || state.media_base.is_empty() {
        return Vec::new();
    }

    let wanted: Vec<Attachment> = state
        .entries
        .iter()
        .rev()
        .take(THUMBNAIL_LOOKBACK)
        .filter_map(|entry| match entry {
            Entry::Chat {
                attachment: Some(attachment),
                ..
            } if attachment.kind == AttachmentKind::Image => Some(attachment.clone()),
            _ => None,
        })
        .filter(|attachment| !state.thumbnails.contains_key(&attachment.id))
        .collect();

    wanted
        .into_iter()
        .map(|attachment| {
            let url = format!("{}/media/{}", state.media_base, attachment.id);
            state.thumbnails.insert(attachment.id, Thumbnail::Loading);
            Command::Fetch(attachment.id, url)
        })
        .collect()
}

/// Приводит Ctrl с русской буквой к латинскому сочетанию.
///
/// При русской раскладке Ctrl+F физически приходит как «Ctrl+а», Ctrl+R — как
/// «Ctrl+к», а Ctrl+C — как «Ctrl+с». То есть все сочетания отваливаются ровно
/// тогда, когда ими и пользуются: когда человек пишет по-русски.
fn normalize_shortcut(key: KeyEvent) -> KeyEvent {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return key;
    }
    let KeyCode::Char(ch) = key.code else {
        return key;
    };

    // Раскладка ЙЦУКЕН поверх QWERTY: буква на той же физической клавише.
    let latin = match ch.to_lowercase().next().unwrap_or(ch) {
        'й' => 'q',
        'ц' => 'w',
        'у' => 'e',
        'к' => 'r',
        'е' => 't',
        'н' => 'y',
        'г' => 'u',
        'ш' => 'i',
        'щ' => 'o',
        'з' => 'p',
        'ф' => 'a',
        'ы' => 's',
        'в' => 'd',
        'а' => 'f',
        'п' => 'g',
        'р' => 'h',
        'о' => 'j',
        'л' => 'k',
        'д' => 'l',
        'я' => 'z',
        'ч' => 'x',
        'с' => 'c',
        'м' => 'v',
        'и' => 'b',
        'т' => 'n',
        'ь' => 'm',
        _ => return key,
    };

    KeyEvent {
        code: KeyCode::Char(latin),
        ..key
    }
}

/// Упомянут ли ник в тексте. Сравнение без учёта регистра — писать «Alice»
/// и «alice» люди будут вперемешку.
fn mentions(text: &str, nickname: &str) -> bool {
    !nickname.is_empty() && text.to_lowercase().contains(&nickname.to_lowercase())
}

pub fn update(state: &mut State, action: Action) -> Vec<Command> {
    match action {
        Action::Tick => {
            state.tick = state.tick.wrapping_add(1);
            Vec::new()
        }
        Action::Key(key) => {
            // На Windows crossterm присылает и нажатие, и отпускание: без этой
            // проверки каждый символ вводится дважды.
            if key.kind != KeyEventKind::Press {
                return Vec::new();
            }
            let key = normalize_shortcut(key);
            match &mut state.screen {
                Screen::Login(_) => on_login_key(state, key),
                Screen::Chat => on_chat_key(state, key),
            }
        }
        Action::Paste(text) => {
            // Многострочную вставку сервер всё равно схлопнет — делаем это
            // сразу, чтобы человек видел ровно то, что уйдёт в комнату.
            let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
            match &mut state.screen {
                Screen::Login(login) => {
                    let limit = login.limit();
                    login.active().insert(&flat, limit);
                }
                Screen::Chat => state.input.insert(&flat, validate::MAX_TEXT_CHARS),
            }
            Vec::new()
        }
        Action::Scroll(delta) => {
            scroll_by(state, delta);
            Vec::new()
        }
        Action::Notice(text) => {
            state.busy = None;
            state.system(SystemKind::Error, text);
            Vec::new()
        }
        Action::Info(text) => {
            state.system(SystemKind::Info, text);
            Vec::new()
        }
        Action::Idle => {
            state.busy = None;
            Vec::new()
        }
        Action::Hosted { url, lines } => {
            state.busy = None;
            for line in lines {
                state.system(SystemKind::Info, line);
            }
            state.server = url.clone();
            // Подключаемся к самим себе: снаружи это выглядит как обычный вход.
            vec![Command::Connect {
                nickname: state.nickname.clone(),
                room: state.room.clone(),
                server: url,
            }]
        }
        Action::Uploaded(Ok(attachment)) => {
            state.busy = None;
            state.system(SystemKind::Info, format!("отправляю {}", attachment.name));
            // Подпись не запрашиваем: команда уже съела строку ввода, а
            // писать её отдельным сообщением привычнее, чем в аргументах.
            vec![Command::Send(ClientMessage::Chat {
                text: String::new(),
                attachment: Some(attachment.id),
                reply_to: state.replying.take().map(|target| target.id),
            })]
        }
        Action::Uploaded(Err(reason)) => {
            state.busy = None;
            state.system(SystemKind::Error, reason);
            Vec::new()
        }
        Action::Directory { dir, result } => {
            if let Some(browser) = &mut state.browser {
                browser.loading = false;
                browser.dir = dir;
                browser.selected = 0;
                browser.filter.clear();
                match result {
                    Ok(entries) => {
                        browser.entries = entries;
                        browser.error = None;
                    }
                    Err(reason) => {
                        // Каталог мог исчезнуть или оказаться закрытым —
                        // показываем причину, но обзор не закрываем.
                        browser.entries.clear();
                        browser.error = Some(reason);
                    }
                }
            }
            Vec::new()
        }
        Action::Saved(Ok(path)) => {
            state.busy = None;
            state.system(SystemKind::Info, format!("сохранено: {}", path.display()));
            Vec::new()
        }
        Action::Saved(Err(reason)) => {
            state.busy = None;
            state.system(SystemKind::Error, reason);
            Vec::new()
        }
        // Проигрывание — побочный эффект главного цикла: сюда действие
        // доходит, только если звук не смог его перехватить.
        Action::Voice(_) => Vec::new(),
        Action::Net(event) => on_net(state, event),
        Action::Image(id, result) => {
            // Бегунок снимаем, только если ждали именно эту картинку: фоновые
            // миниатюры качаются молча и к нему отношения не имеют.
            if state.viewer.as_ref().is_some_and(|viewer| viewer.id == id) {
                state.busy = None;
            }
            // Одна и та же картинка может понадобиться и просмотру, и ленте:
            // раскладываем по обоим местам, лишнего скачивания при этом нет.
            if let Some(viewer) = &mut state.viewer
                && viewer.id == id
            {
                viewer.state = match &result {
                    Ok(image) => ViewerState::Ready(image.clone()),
                    Err(reason) => ViewerState::Failed(reason.clone()),
                };
            }
            if state.thumbnails.contains_key(&id) {
                state.thumbnails.insert(
                    id,
                    match result {
                        Ok(image) => Thumbnail::Ready(image),
                        Err(_) => Thumbnail::Failed,
                    },
                );
            }
            Vec::new()
        }
    }
}

/// Правка текста, общая для всех полей ввода. `true` — клавиша обработана.
fn edit_key(input: &mut Input, key: KeyEvent, limit: usize) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('a') if ctrl => input.cursor = 0,
        KeyCode::Char('e') if ctrl => input.cursor = input.len(),
        KeyCode::Char('u') if ctrl => input.kill_to_start(),
        KeyCode::Char('k') if ctrl => input.kill_to_end(),
        KeyCode::Char('w') if ctrl => input.kill_word(),
        KeyCode::Char(ch) if !ctrl => input.insert(&ch.to_string(), limit),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.cursor = input.cursor.saturating_sub(1),
        KeyCode::Right => input.cursor = (input.cursor + 1).min(input.len()),
        KeyCode::Home => input.cursor = 0,
        KeyCode::End => input.cursor = input.len(),
        _ => return false,
    }
    true
}

fn on_login_key(state: &mut State, key: KeyEvent) -> Vec<Command> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code == KeyCode::Esc || (ctrl && matches!(key.code, KeyCode::Char('c' | 'd'))) {
        state.should_quit = true;
        return vec![Command::Quit];
    }

    let Screen::Login(login) = &mut state.screen else {
        return Vec::new();
    };

    match key.code {
        KeyCode::Enter => return login_submit(state),
        KeyCode::Tab | KeyCode::Down => login.next_field(false),
        KeyCode::BackTab | KeyCode::Up => login.next_field(true),
        _ => {
            let limit = login.limit();
            edit_key(login.active(), key, limit);
        }
    }
    Vec::new()
}

fn login_submit(state: &mut State) -> Vec<Command> {
    let Screen::Login(login) = &mut state.screen else {
        return Vec::new();
    };

    // Проверяем теми же правилами, что и сервер: ошибка видна сразу, без
    // лишнего похода по сети.
    let nickname = match validate::clean_nickname(&login.nickname.text) {
        Ok(nickname) => nickname,
        Err(err) => {
            login.field = Field::Nickname;
            login.error = Some(err.to_string());
            return Vec::new();
        }
    };
    let room = match validate::clean_room(&login.room.text) {
        Ok(room) => room,
        Err(err) => {
            login.field = Field::Room;
            login.error = Some(err.to_string());
            return Vec::new();
        }
    };

    let typed = login.server.text.trim().to_string();

    // Пустое поле означает «оставить как есть»: адрес обычно уже известен
    // из настроек, и переписывать его каждый раз незачем.
    let raw = match (typed.is_empty(), state.server.is_empty()) {
        (false, _) => typed,
        (true, false) => state.server.clone(),
        // Ни в поле, ни в настройках ничего нет — значит, сервер свой,
        // на этой же машине. Отказывать здесь не за что.
        (true, true) => crate::net::DEFAULT_SERVER.to_string(),
    };
    let server = match crate::net::normalize_server(&raw) {
        Ok(server) => server,
        Err(reason) => {
            if let Screen::Login(login) = &mut state.screen {
                login.field = Field::Server;
                login.error = Some(reason);
            }
            return Vec::new();
        }
    };

    state.nickname = nickname.clone();
    state.room = room.clone();
    state.server = server.clone();
    state.screen = Screen::Chat;
    state.forget_room();
    vec![Command::Connect {
        nickname,
        room,
        server,
    }]
}

/// Прокручивает историю: положительное число строк — вверх, к прошлому.
fn scroll_by(state: &mut State, delta: i32) {
    let scrollback = state.scrollback as i64 + i64::from(delta);
    state.scrollback = scrollback.clamp(0, state.viewport.max_scroll() as i64) as usize;
}

/// Дополняет по Tab: в начале строки — команду, дальше — ник.
///
/// Команду в начале строки человек и ждёт: набирать её целиком, когда клиент
/// и так знает список, — лишняя работа.
fn complete(state: &mut State) {
    // Повторный Tab продолжает перебор, любая другая клавиша его сбрасывает:
    // иначе после правки строки перебор шёл бы по устаревшему слову.
    let (prefix, at) = match &state.completion {
        Some(completion) => (completion.prefix.clone(), completion.at),
        None => {
            let before: String = state.input.text.chars().take(state.input.cursor).collect();
            let at = before
                .rfind(char::is_whitespace)
                .map_or(0, |index| before[..index].chars().count() + 1);
            (before.chars().skip(at).collect::<String>(), at)
        }
    };
    if prefix.is_empty() {
        return;
    }

    // Слово со слэша в начале строки — это команда, а не ник.
    let command = at == 0 && prefix.starts_with('/');
    let matches: Vec<String> = if command {
        let needle = prefix.to_lowercase();
        COMMANDS
            .iter()
            .filter(|name| name.starts_with(&needle))
            .map(|name| (*name).to_string())
            .collect()
    } else {
        let key = validate::nickname_key(&prefix);
        state
            .users
            .iter()
            .filter(|user| validate::nickname_key(&user.nickname).starts_with(&key))
            .map(|user| user.nickname.clone())
            .collect()
    };
    if matches.is_empty() {
        return;
    }

    let index = state
        .completion
        .as_ref()
        .map_or(0, |completion| (completion.index + 1) % matches.len());
    // В начале строки к человеку обращаются через запятую, команде запятая
    // ни к чему.
    let suffix = if command || at > 0 { " " } else { ", " };
    let completed = format!("{}{suffix}", matches[index]);

    let head: String = state.input.text.chars().take(at).collect();
    let tail: String = state.input.text.chars().skip(state.input.cursor).collect();
    state.input = Input::new(format!("{head}{completed}{tail}"));
    state.input.cursor = at + completed.chars().count();
    state.completion = Some(Completion { prefix, at, index });
}

/// Пересчитывает совпадения под текущий запрос.
fn refresh_matches(state: &mut State) {
    let Some(search) = &state.search else {
        return;
    };
    let needle = search.query.text.to_lowercase();
    let matches: Vec<usize> = if needle.is_empty() {
        Vec::new()
    } else {
        state
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry_text(entry).to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect()
    };

    if let Some(search) = &mut state.search {
        search.matches = matches;
        // Начинаем с самого свежего совпадения: в переписке нужнее последнее,
        // а не первое.
        search.current = search.matches.len().saturating_sub(1);
    }
    reveal_current_match(state);
}

fn entry_text(entry: &Entry) -> &str {
    match entry {
        Entry::Chat { text, .. } => text,
        Entry::System { text, .. } => text,
    }
}

/// Переходит к соседнему совпадению: `1` — к более свежему.
fn step_match(state: &mut State, direction: i32) {
    let Some(search) = &mut state.search else {
        return;
    };
    if search.matches.is_empty() {
        return;
    }
    let count = search.matches.len() as i32;
    let next = (search.current as i32 + direction).rem_euclid(count);
    search.current = next as usize;
    reveal_current_match(state);
}

/// Прокручивает историю так, чтобы выбранное совпадение оказалось на виду.
fn reveal_current_match(state: &mut State) {
    let Some(entry) = state.search.as_ref().and_then(Search::current_entry) else {
        return;
    };
    let Some(&line) = state.entry_lines.get(entry) else {
        return;
    };

    let height = state.viewport.height.max(1);
    let total = state.viewport.total_lines;
    // Ставим найденное примерно в середину окна: так видно и то, что было
    // до него, и то, что после.
    let end = (line + height / 2 + 1).clamp(height.min(total), total);
    state.scrollback = total.saturating_sub(end);
}

fn on_browser_key(state: &mut State, key: KeyEvent) -> Vec<Command> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code == KeyCode::Esc || (ctrl && matches!(key.code, KeyCode::Char('c' | 'd'))) {
        state.browser = None;
        return Vec::new();
    }

    let Some(browser) = &mut state.browser else {
        return Vec::new();
    };
    match key.code {
        KeyCode::Up => browser.move_by(-1),
        KeyCode::Down => browser.move_by(1),
        KeyCode::PageUp => browser.move_by(-10),
        KeyCode::PageDown => browser.move_by(10),
        // Влево — наверх по дереву. Backspace занят правкой отсева, но на
        // пустом отсеве делает то же самое: так привычнее.
        KeyCode::Left => return leave_dir(state),
        KeyCode::Backspace if browser.filter.is_empty() => return leave_dir(state),
        KeyCode::Enter => return choose(state),
        _ => {
            if edit_key(&mut browser.filter, key, validate::MAX_TEXT_CHARS) {
                browser.selected = 0;
            }
        }
    }
    Vec::new()
}

/// Поднимается на уровень выше.
fn leave_dir(state: &mut State) -> Vec<Command> {
    let Some(browser) = &mut state.browser else {
        return Vec::new();
    };
    let Some(parent) = browser.dir.parent().map(std::path::Path::to_path_buf) else {
        return Vec::new();
    };
    browser.loading = true;
    vec![Command::ReadDir(parent)]
}

/// Открывает каталог или отправляет выбранный файл.
fn choose(state: &mut State) -> Vec<Command> {
    let Some(browser) = &state.browser else {
        return Vec::new();
    };
    let Some(entry) = browser.current().cloned() else {
        return Vec::new();
    };

    if entry.is_dir {
        if let Some(browser) = &mut state.browser {
            browser.loading = true;
        }
        return vec![Command::ReadDir(entry.path)];
    }

    let dir = browser.dir.clone();
    state.browser = None;
    state.last_dir = Some(dir.to_string_lossy().to_string());
    state.busy = Some(format!("отправляю {}", entry.name));
    // Каталог запоминаем: в следующий раз обзор откроется там же.
    vec![Command::Upload(entry.path), Command::SaveConfig]
}

fn on_search_key(state: &mut State, key: KeyEvent) -> Vec<Command> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            state.search = None;
            return Vec::new();
        }
        KeyCode::Char('c' | 'd') if ctrl => {
            state.should_quit = true;
            return vec![Command::Quit];
        }
        // Enter и стрелки ходят по совпадениям: сам запрос уже набран.
        KeyCode::Enter | KeyCode::Down => step_match(state, 1),
        KeyCode::Up => step_match(state, -1),
        _ => {
            let Some(search) = &mut state.search else {
                return Vec::new();
            };
            if edit_key(&mut search.query, key, validate::MAX_TEXT_CHARS) {
                refresh_matches(state);
            }
        }
    }
    Vec::new()
}

/// Соседняя реплика: системные строки при выборе ответа пропускаются.
fn neighbour_chat(state: &State, from: usize, direction: i32) -> Option<usize> {
    let mut index = from as i64;
    loop {
        index += i64::from(direction);
        if index < 0 || index as usize >= state.entries.len() {
            return None;
        }
        if matches!(state.entries[index as usize], Entry::Chat { .. }) {
            return Some(index as usize);
        }
    }
}

fn last_chat(state: &State) -> Option<usize> {
    state
        .entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Chat { .. }))
}

/// Начинает выбор сообщения для ответа или подтверждает выбранное.
fn toggle_picking(state: &mut State) {
    match state.picking.take() {
        Some(index) => confirm_reply(state, index),
        None => {
            state.picking = last_chat(state);
            if state.picking.is_none() {
                state.system(SystemKind::Error, "отвечать пока не на что");
            }
            reveal_entry(state, state.picking);
        }
    }
}

fn confirm_reply(state: &mut State, index: usize) {
    let Some(Entry::Chat {
        id,
        from,
        text,
        attachment,
        ..
    }) = state.entries.get(index)
    else {
        return;
    };

    // Цитату показываем свою, но с сервером уйдёт только идентификатор:
    // настоящую цитату он соберёт сам из истории комнаты.
    let excerpt = if text.is_empty() {
        attachment
            .as_ref()
            .map_or(String::new(), |attachment| attachment.name.clone())
    } else {
        text.chars().take(REPLY_EXCERPT_CHARS).collect()
    };
    state.replying = Some(ReplyTarget {
        id: *id,
        nickname: from.clone(),
        excerpt,
    });
}

/// Прокручивает историю к записи, если она за пределами окна.
fn reveal_entry(state: &mut State, entry: Option<usize>) {
    let Some(entry) = entry else {
        return;
    };
    let Some(&line) = state.entry_lines.get(entry) else {
        return;
    };
    let height = state.viewport.height.max(1);
    let total = state.viewport.total_lines;
    let end = (line + height / 2 + 1).clamp(height.min(total), total);
    state.scrollback = total.saturating_sub(end);
}

fn on_chat_key(state: &mut State, key: KeyEvent) -> Vec<Command> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Пока открыта картинка, клавиши закрывают её, а не выходят из программы:
    // Esc в просмотрщике — это «назад», а не «выход».
    if state.viewer.is_some() {
        if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
            state.viewer = None;
        }
        return Vec::new();
    }

    if state.browser.is_some() {
        return on_browser_key(state, key);
    }

    if state.help {
        // Справку закрывает что угодно осмысленное: искать нужную клавишу,
        // чтобы убрать подсказку, — отдельное издевательство.
        if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
            state.help = false;
        }
        return Vec::new();
    }

    if state.search.is_some() {
        return on_search_key(state, key);
    }

    // Выбор сообщения для ответа перехватывает стрелки и Enter: пока он открыт,
    // они значат «другое сообщение» и «это оно».
    if let Some(index) = state.picking {
        match key.code {
            KeyCode::Esc => state.picking = None,
            KeyCode::Char('c' | 'd') if ctrl => {
                state.should_quit = true;
                return vec![Command::Quit];
            }
            KeyCode::Enter => {
                state.picking = None;
                confirm_reply(state, index);
            }
            // Повторный Ctrl+R подтверждает выбор — так же, как Enter.
            KeyCode::Char('r') if ctrl => {
                state.picking = None;
                confirm_reply(state, index);
            }
            KeyCode::Up => {
                if let Some(next) = neighbour_chat(state, index, -1) {
                    state.picking = Some(next);
                    reveal_entry(state, Some(next));
                }
            }
            KeyCode::Down => {
                if let Some(next) = neighbour_chat(state, index, 1) {
                    state.picking = Some(next);
                    reveal_entry(state, Some(next));
                }
            }
            _ => {}
        }
        return Vec::new();
    }

    if key.code != KeyCode::Tab {
        state.completion = None;
    }

    match key.code {
        KeyCode::Char('f') if ctrl => {
            state.search = Some(Search::default());
            return Vec::new();
        }
        KeyCode::Char('r') if ctrl => {
            toggle_picking(state);
            return Vec::new();
        }
        // Отдельная клавиша для файла: набирать «/send» ради выбора картинки
        // всё-таки лишний шаг.
        KeyCode::Char('o') if ctrl => return send_command(state, ""),
        // Пока ответ взведён, Esc снимает его, а не выходит из программы.
        KeyCode::Esc if state.replying.is_some() => {
            state.replying = None;
            return Vec::new();
        }
        KeyCode::Esc => {
            state.should_quit = true;
            return vec![Command::Quit];
        }
        KeyCode::Char('c' | 'd') if ctrl => {
            state.should_quit = true;
            return vec![Command::Quit];
        }
        KeyCode::Enter => return submit(state),
        KeyCode::Up => state.recall(-1),
        KeyCode::Down => state.recall(1),
        KeyCode::PageUp => scroll_by(state, SCROLL_STEP as i32),
        KeyCode::PageDown => scroll_by(state, -(SCROLL_STEP as i32)),
        KeyCode::Tab => complete(state),
        _ => {
            let changed = edit_key(&mut state.input, key, validate::MAX_TEXT_CHARS);
            if changed && !state.input.is_empty() {
                return announce_typing(state);
            }
        }
    }
    Vec::new()
}

/// Сообщает остальным, что мы печатаем, — но не чаще, чем нужно.
fn announce_typing(state: &mut State) -> Vec<Command> {
    if !state.is_online() {
        return Vec::new();
    }
    let now = Instant::now();
    if state
        .typing_sent
        .is_some_and(|last| now.duration_since(last) < TYPING_EVERY)
    {
        return Vec::new();
    }

    state.typing_sent = Some(now);
    vec![Command::Send(ClientMessage::Typing)]
}

fn submit(state: &mut State) -> Vec<Command> {
    let line = state.input.text.trim().to_string();
    if line.is_empty() {
        return Vec::new();
    }

    // Команда занимает всю строку; «//» — способ отправить текст, который сам
    // начинается со слэша.
    if line.starts_with('/') && !line.starts_with("//") {
        state.remember_sent(&line);
        state.input.clear();
        return run_command(state, &line);
    }

    let body = line.strip_prefix('/').map_or(line.clone(), str::to_string);
    let text = match validate::clean_text(&body) {
        Ok(text) => text,
        Err(err) => {
            state.system(SystemKind::Error, err.to_string());
            return Vec::new();
        }
    };
    if !state.is_online() {
        // Ввод намеренно не очищаем: набранное не должно пропадать из-за обрыва.
        state.system(SystemKind::Error, "нет соединения, сообщение не отправлено");
        return Vec::new();
    }

    state.remember_sent(&line);
    state.input.clear();
    state.typing_sent = None;
    // Отправлять файлы из терминала пока нельзя: показать результат он всё
    // равно не сможет, а загрузка без превью — сомнительное удобство.
    let reply_to = state.replying.take().map(|target| target.id);
    vec![Command::Send(ClientMessage::Chat {
        text,
        attachment: None,
        reply_to,
    })]
}

fn run_command(state: &mut State, line: &str) -> Vec<Command> {
    let mut parts = line[1..].splitn(2, ' ');
    let name = parts.next().unwrap_or_default().to_lowercase();
    let arg = parts.next().unwrap_or_default().trim().to_string();

    match name.as_str() {
        "help" | "?" => {
            state.help = true;
            Vec::new()
        }
        "quit" | "exit" => {
            state.should_quit = true;
            vec![Command::Quit]
        }
        "clear" => {
            state.entries.clear();
            state.seen.clear();
            state.thumbnails.clear();
            state.scrollback = 0;
            Vec::new()
        }
        "view" => view_command(state),
        "open" => open_command(state),
        "send" => send_command(state, &arg),
        "rec" => vec![Command::ToggleRecording],
        "play" => play_command(state),
        "stop" => vec![Command::StopVoice],
        "save" => save_command(state, &arg),
        "color" => color_command(state, &arg),
        "host" => host_command(state, &arg),
        "join" => join_command(state, &arg),
        "nick" => nick_command(state, &arg),
        other => {
            state.system(
                SystemKind::Error,
                format!("неизвестная команда /{other}, список — /help"),
            );
            Vec::new()
        }
    }
}

/// Последнее вложение в комнате.
fn last_attachment(state: &State) -> Option<Attachment> {
    state.entries.iter().rev().find_map(|entry| match entry {
        Entry::Chat {
            attachment: Some(attachment),
            ..
        } => Some(attachment.clone()),
        _ => None,
    })
}

fn attachment_url(state: &State, attachment: &Attachment) -> Option<String> {
    (!state.media_base.is_empty()).then(|| format!("{}/media/{}", state.media_base, attachment.id))
}

/// Показывает последнюю картинку прямо в переписке.
fn view_command(state: &mut State) -> Vec<Command> {
    let Some(attachment) = last_attachment(state) else {
        state.system(SystemKind::Error, "в этой комнате пока нет вложений");
        return Vec::new();
    };
    if attachment.kind != AttachmentKind::Image {
        state.system(
            SystemKind::Error,
            "это не картинка — звук слушается через /open",
        );
        return Vec::new();
    }
    let Some(url) = attachment_url(state, &attachment) else {
        state.system(SystemKind::Error, "неизвестен адрес сервера");
        return Vec::new();
    };

    let id = attachment.id;
    state.busy = Some(format!("качаю {}", attachment.name));
    state.viewer = Some(Viewer {
        id,
        name: attachment.name,
        state: ViewerState::Loading,
    });
    vec![Command::Fetch(id, url)]
}

/// Открывает последнее пришедшее вложение во внешней программе.
///
/// Терминал картинку не покажет, а полный адрес в ленте рвётся переносом и
/// становится бесполезным, поэтому открываем сами.
fn open_command(state: &mut State) -> Vec<Command> {
    let Some(attachment) = last_attachment(state) else {
        state.system(SystemKind::Error, "в этой комнате пока нет вложений");
        return Vec::new();
    };
    let Some(url) = attachment_url(state, &attachment) else {
        state.system(SystemKind::Error, "неизвестен адрес сервера");
        return Vec::new();
    };

    state.system(SystemKind::Info, format!("открываю {}", attachment.name));
    vec![Command::Open(url)]
}

/// Проигрывает последнее голосовое.
fn play_command(state: &mut State) -> Vec<Command> {
    let Some(attachment) = last_attachment(state) else {
        state.system(SystemKind::Error, "в этой комнате пока нет вложений");
        return Vec::new();
    };
    if attachment.kind != AttachmentKind::Audio {
        state.system(SystemKind::Error, "последнее вложение — не голосовое");
        return Vec::new();
    }
    let Some(url) = attachment_url(state, &attachment) else {
        state.system(SystemKind::Error, "неизвестен адрес сервера");
        return Vec::new();
    };

    state.busy = Some(format!("качаю {}", attachment.name));
    vec![Command::PlayVoice(url)]
}

/// Сохраняет последнее вложение на диск.
fn save_command(state: &mut State, arg: &str) -> Vec<Command> {
    let Some(attachment) = last_attachment(state) else {
        state.system(SystemKind::Error, "в этой комнате пока нет вложений");
        return Vec::new();
    };
    let Some(url) = attachment_url(state, &attachment) else {
        state.system(SystemKind::Error, "неизвестен адрес сервера");
        return Vec::new();
    };

    let arg = arg.trim().trim_matches(['"', '\'']).trim();
    let destination = if arg.is_empty() {
        downloads_dir().join(&attachment.name)
    } else {
        let path = std::path::PathBuf::from(arg);
        // Указали каталог — дописываем имя файла сами.
        if path.is_dir() {
            path.join(&attachment.name)
        } else {
            path
        }
    };

    state.busy = Some(format!("сохраняю {}", attachment.name));
    vec![Command::Save { url, destination }]
}

/// Куда складывать файлы, если путь не указан.
fn downloads_dir() -> std::path::PathBuf {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    match home.map(std::path::PathBuf::from) {
        // «Загрузки» — то место, где такие файлы ищут в первую очередь.
        Some(home) if home.join("Downloads").is_dir() => home.join("Downloads"),
        Some(home) => home,
        None => std::path::PathBuf::from("."),
    }
}

/// Отправляет файл с диска.
fn send_command(state: &mut State, arg: &str) -> Vec<Command> {
    // Пути с пробелами люди берут в кавычки, а перетаскивание файла в окно
    // терминала вставляет их само.
    let path = arg.trim().trim_matches(['"', '\'']).trim();
    if !state.is_online() {
        state.system(SystemKind::Error, "нет соединения, файл не отправлен");
        return Vec::new();
    }
    if state.media_base.is_empty() {
        state.system(SystemKind::Error, "неизвестен адрес сервера");
        return Vec::new();
    }

    // Без аргумента открываем обзор: набирать путь руками — ровно тот костыль,
    // из-за которого отправка файла в терминале ощущается наказанием.
    if path.is_empty() {
        let dir = crate::files::start_dir(state.last_dir.as_deref());
        state.browser = Some(Browser {
            dir: dir.clone(),
            entries: Vec::new(),
            selected: 0,
            filter: Input::default(),
            loading: true,
            error: None,
        });
        return vec![Command::ReadDir(dir)];
    }

    state.busy = Some(format!("отправляю {path}"));
    vec![Command::Upload(std::path::PathBuf::from(path))]
}

/// Поднимает сервер прямо здесь и подключается к нему.
fn host_command(state: &mut State, arg: &str) -> Vec<Command> {
    let port = if arg.trim().is_empty() {
        8080
    } else {
        match arg.trim().parse::<u16>() {
            Ok(port) => port,
            Err(_) => {
                state.system(SystemKind::Error, "порт — это число, например /host 9000");
                return Vec::new();
            }
        }
    };

    state.busy = Some("поднимаю сервер".to_string());
    vec![Command::Host(port)]
}

/// Задаёт цвет ника: `/color <цвет>` для себя, `/color <ник> <цвет>` для чужого,
/// `-` вместо цвета возвращает цвет по умолчанию.
fn color_command(state: &mut State, arg: &str) -> Vec<Command> {
    let mut parts = arg.split_whitespace();
    let (nickname, value) = match (parts.next(), parts.next()) {
        (Some(value), None) => (state.nickname.clone(), value.to_string()),
        (Some(nickname), Some(value)) => (nickname.to_string(), value.to_string()),
        _ => {
            state.system(
                SystemKind::Error,
                "нужно так: /color #d97757 или /color bob cyan",
            );
            return Vec::new();
        }
    };

    let key = validate::nickname_key(&nickname);
    if value == "-" {
        state.colors.remove(&key);
        state.system(SystemKind::Info, format!("цвет {nickname} сброшен"));
        return vec![Command::SaveConfig];
    }

    let Some(color) = config::parse_color(&value) else {
        state.system(
            SystemKind::Error,
            format!("не понял цвет {value}: нужен #rrggbb или название вроде cyan"),
        );
        return Vec::new();
    };

    state.colors.insert(key, color);
    state.system(SystemKind::Info, format!("цвет {nickname} теперь {value}"));
    vec![Command::SaveConfig]
}

fn join_command(state: &mut State, arg: &str) -> Vec<Command> {
    let room = match validate::clean_room(arg) {
        Ok(room) => room,
        Err(err) => {
            state.system(SystemKind::Error, err.to_string());
            return Vec::new();
        }
    };
    if room == state.room && state.is_online() {
        state.system(SystemKind::Info, format!("вы уже в комнате {room}"));
        return Vec::new();
    }

    // Комната другая — старая переписка к ней отношения не имеет.
    state.forget_room();
    state.system(SystemKind::Info, format!("перехожу в комнату {room}"));
    state.room = room.clone();
    vec![Command::Connect {
        nickname: state.nickname.clone(),
        room,
        server: state.server.clone(),
    }]
}

fn nick_command(state: &mut State, arg: &str) -> Vec<Command> {
    let nickname = match validate::clean_nickname(arg) {
        Ok(nickname) => nickname,
        Err(err) => {
            state.system(SystemKind::Error, err.to_string());
            return Vec::new();
        }
    };
    if nickname == state.nickname {
        return Vec::new();
    }

    // Сервер не умеет переименовывать участника, поэтому входим заново.
    state.system(SystemKind::Info, format!("меняю ник на {nickname}"));
    state.nickname = nickname.clone();
    vec![Command::Connect {
        nickname,
        room: state.room.clone(),
        server: state.server.clone(),
    }]
}

fn on_net(state: &mut State, event: NetEvent) -> Vec<Command> {
    match event {
        NetEvent::Connecting { attempt } => {
            state.status = Status::Connecting { attempt };
        }
        NetEvent::Disconnected { reason, retry_at } => {
            state.system(SystemKind::Error, format!("соединение потеряно: {reason}"));
            state.status = Status::Reconnecting { reason, retry_at };
            // Список участников больше не актуален, а показывать устаревший —
            // хуже, чем пустой: человек будет писать «в пустоту».
            state.users.clear();
        }
        NetEvent::Fatal { reason } => {
            // Ник занят или комната кривая — это чинится вводом другого
            // значения, поэтому возвращаемся на экран входа, а не падаем.
            state.screen = Screen::Login(Login {
                nickname: Input::new(state.nickname.clone()),
                room: Input::new(state.room.clone()),
                server: Input::new(state.server.clone()),
                field: Field::Nickname,
                error: Some(reason),
            });
            state.users.clear();
        }
        NetEvent::Message(msg) => return on_server(state, msg),
    }
    Vec::new()
}

fn on_server(state: &mut State, msg: ServerMessage) -> Vec<Command> {
    let mut commands = Vec::new();
    match msg {
        ServerMessage::Welcome {
            your_id,
            room,
            nickname,
            users,
            history,
        } => {
            let reconnected = state.me.is_some();
            state.me = Some(your_id);
            state.room = room.clone();
            state.nickname = nickname.clone();
            state.users = users;
            state.users.push(UserInfo {
                id: your_id,
                nickname: nickname.clone(),
            });
            state.sort_users();
            state.status = Status::Online;

            // История комнаты: при первом входе она заполняет пустой экран,
            // при переподключении — возвращает пропущенное. Дубли отсекаются
            // по id, поэтому наложение на уже показанное безопасно.
            let mut mentioned = false;
            for message in history {
                mentioned |= state.push_chat(message);
            }

            if mentioned {
                // Пока нас не было, нас звали — стоит об этом сообщить.
                commands.push(Command::Bell);
            }
            if reconnected {
                state.system(SystemKind::Info, "соединение восстановлено");
            } else {
                state.system(
                    SystemKind::Info,
                    format!("вы вошли в комнату {room} как {nickname}"),
                );
                state.system(SystemKind::Info, "/help — список команд");
            }
            // Ник и комната запоминаются после удачного входа, а не при вводе:
            // сохранять то, что сервер отверг, смысла нет.
            commands.push(Command::SaveConfig);
        }
        ServerMessage::UserJoined { user } => {
            state.system(
                SystemKind::Join,
                format!("{} вошёл в комнату", user.nickname),
            );
            if !state.users.iter().any(|known| known.id == user.id) {
                state.users.push(user);
                state.sort_users();
            }
        }
        ServerMessage::UserLeft { user } => {
            state.system(SystemKind::Leave, format!("{} вышел", user.nickname));
            state.users.retain(|known| known.id != user.id);
            state.typing.remove(&user.id);
        }
        ServerMessage::Typing { user } => {
            state
                .typing
                .insert(user.id, (user.nickname, Instant::now()));
        }
        ServerMessage::Chat(message) => {
            if state.push_chat(message) {
                commands.push(Command::Bell);
            }
        }
        ServerMessage::Error { message, .. } => {
            state.system(SystemKind::Error, message);
        }
        // Прикладной pong гасит сетевая задача, до состояния он не доходит.
        ServerMessage::Pong => {}
    }
    commands.extend(queue_thumbnails(state));
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn key(code: KeyCode) -> Action {
        Action::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(ch: char) -> Action {
        Action::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL))
    }

    fn user(nickname: &str) -> UserInfo {
        UserInfo {
            id: Uuid::new_v4(),
            nickname: nickname.into(),
        }
    }

    fn chat_message(from: UserInfo, text: &str) -> ChatMessage {
        ChatMessage {
            id: Uuid::new_v4(),
            from,
            text: text.into(),
            ts: 1_700_000_000_000,
            attachment: None,
            reply: None,
        }
    }

    /// Клиент, уже вошедший в комнату.
    fn connected() -> (State, Uuid) {
        let (mut state, _) = State::new(Some("alice".into()), "general".into());
        let me = Uuid::new_v4();
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Welcome {
                your_id: me,
                room: "general".into(),
                nickname: "alice".into(),
                users: vec![],
                history: vec![],
            })),
        );
        (state, me)
    }

    fn typed(state: &mut State, text: &str) {
        for ch in text.chars() {
            update(state, key(KeyCode::Char(ch)));
        }
    }

    fn texts(state: &State) -> Vec<&str> {
        state
            .entries
            .iter()
            .map(|entry| match entry {
                Entry::Chat { text, .. } => text.as_str(),
                Entry::System { text, .. } => text.as_str(),
            })
            .collect()
    }

    #[test]
    fn nickname_from_arguments_skips_the_login_screen() {
        let (state, commands) = State::new(Some("alice".into()), "general".into());

        assert!(matches!(state.screen, Screen::Chat));
        assert_eq!(
            commands,
            [Command::Connect {
                nickname: "alice".into(),
                room: "general".into(),
                server: String::new(),
            }]
        );
    }

    #[test]
    fn without_a_nickname_the_login_screen_asks_for_one() {
        let (mut state, commands) = State::new(None, "general".into());
        assert!(commands.is_empty());
        assert!(matches!(state.screen, Screen::Login(_)));

        typed(&mut state, "alice");
        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(matches!(
            commands.as_slice(),
            [Command::Connect { nickname, room, .. }] if nickname == "alice" && room == "general"
        ));
        assert!(matches!(state.screen, Screen::Chat));
    }

    #[test]
    fn login_validates_before_going_to_the_network() {
        let (mut state, _) = State::new(None, "general".into());
        typed(&mut state, "a b");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(
            commands.is_empty(),
            "кривой ник не должен уходить на сервер"
        );
        let Screen::Login(login) = &state.screen else {
            panic!("остаёмся на экране входа");
        };
        assert!(login.error.is_some());
    }

    #[test]
    fn taken_nickname_returns_to_the_login_screen() {
        let (mut state, _) = connected();

        update(
            &mut state,
            Action::Net(NetEvent::Fatal {
                reason: "ник alice в этой комнате уже занят".into(),
            }),
        );

        // Раньше это был фатальный выход: приходилось перезапускать программу
        // с другим ником.
        let Screen::Login(login) = &state.screen else {
            panic!("ожидался экран входа, а не выход");
        };
        assert_eq!(login.nickname.text, "alice");
        assert_eq!(login.room.text, "general");
        assert!(login.error.as_deref().unwrap().contains("занят"));
    }

    #[test]
    fn tab_switches_login_fields() {
        let (mut state, _) = State::new(None, "general".into());
        typed(&mut state, "alice");
        update(&mut state, key(KeyCode::Tab));
        update(&mut state, ctrl('u'));
        typed(&mut state, "rust");

        let Screen::Login(login) = &state.screen else {
            panic!("ожидался экран входа");
        };
        assert_eq!(
            (login.nickname.text.as_str(), login.room.text.as_str()),
            ("alice", "rust")
        );
    }

    #[test]
    fn welcome_history_fills_the_empty_screen() {
        let (mut state, _) = State::new(Some("alice".into()), "general".into());
        let bob = user("bob");

        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Welcome {
                your_id: Uuid::new_v4(),
                room: "general".into(),
                nickname: "alice".into(),
                users: vec![bob.clone()],
                history: vec![
                    chat_message(bob.clone(), "первое"),
                    chat_message(bob, "второе"),
                ],
            })),
        );

        let shown = texts(&state);
        assert_eq!(shown[0], "первое");
        assert_eq!(shown[1], "второе");
    }

    #[test]
    fn replayed_history_does_not_duplicate_messages() {
        let (mut state, me) = connected();
        let bob = user("bob");
        let message = chat_message(bob.clone(), "привет");
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Chat(message.clone()))),
        );

        // Обрыв и переподключение: сервер отдаст ту же реплику в истории.
        update(
            &mut state,
            Action::Net(NetEvent::Disconnected {
                reason: "обрыв".into(),
                retry_at: Instant::now() + Duration::from_secs(1),
            }),
        );
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Welcome {
                your_id: me,
                room: "general".into(),
                nickname: "alice".into(),
                users: vec![bob.clone()],
                history: vec![message, chat_message(bob, "пропущенное")],
            })),
        );

        let shown = texts(&state);
        assert_eq!(
            shown.iter().filter(|text| **text == "привет").count(),
            1,
            "реплика показана дважды: {shown:?}"
        );
        assert!(
            shown.contains(&"пропущенное"),
            "пропущенное за время обрыва не восстановлено: {shown:?}"
        );
    }

    #[test]
    fn mentions_are_marked() {
        let (mut state, _) = connected();

        for text in ["Alice, ты тут?", "просто сообщение"] {
            update(
                &mut state,
                Action::Net(NetEvent::Message(ServerMessage::Chat(chat_message(
                    user("bob"),
                    text,
                )))),
            );
        }

        let flags: Vec<_> = state
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Chat { mentions_me, .. } => Some(*mentions_me),
                _ => None,
            })
            .collect();
        assert_eq!(flags, [true, false]);
    }

    #[test]
    fn own_messages_are_marked() {
        let (mut state, me) = connected();

        for from in [
            UserInfo {
                id: me,
                nickname: "alice".into(),
            },
            user("bob"),
        ] {
            update(
                &mut state,
                Action::Net(NetEvent::Message(ServerMessage::Chat(chat_message(
                    from,
                    "привет",
                )))),
            );
        }

        let mine: Vec<_> = state
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Chat { mine, .. } => Some(*mine),
                _ => None,
            })
            .collect();
        assert_eq!(mine, [true, false]);
    }

    #[test]
    fn submit_sends_and_clears_input() {
        let (mut state, _) = connected();
        typed(&mut state, "  привет  ");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(matches!(
            commands.as_slice(),
            [Command::Send(ClientMessage::Chat { text, .. })] if text == "привет"
        ));
        assert!(state.input.is_empty());
    }

    #[test]
    fn submit_while_offline_keeps_the_text() {
        let (mut state, _) = State::new(Some("alice".into()), "general".into());
        typed(&mut state, "привет");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty());
        assert_eq!(state.input.text, "привет");
    }

    #[test]
    fn slash_commands_do_not_reach_the_room() {
        let (mut state, _) = connected();
        let before = state.entries.len();
        typed(&mut state, "/help");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty());
        assert!(state.help, "справка не открылась");
        // Справка поверх, а не в ленте: одиннадцать строк выталкивали бы
        // из виду сам разговор, ради которого её и открывали.
        assert_eq!(state.entries.len(), before);
    }

    #[test]
    fn any_key_closes_the_help() {
        let (mut state, _) = connected();
        typed(&mut state, "/help");
        update(&mut state, key(KeyCode::Enter));

        update(&mut state, key(KeyCode::Char('x')));

        assert!(!state.help);
        // Закрытие справки не должно попадать в поле ввода.
        assert!(state.input.is_empty());
    }

    #[test]
    fn double_slash_escapes_a_message() {
        let (mut state, _) = connected();
        typed(&mut state, "//join это не команда");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(matches!(
            commands.as_slice(),
            [Command::Send(ClientMessage::Chat { text, .. })] if text == "/join это не команда"
        ));
    }

    #[test]
    fn unknown_command_is_reported() {
        let (mut state, _) = connected();
        typed(&mut state, "/dance");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty());
        assert!(matches!(
            state.entries.last(),
            Some(Entry::System {
                kind: SystemKind::Error,
                ..
            })
        ));
    }

    #[test]
    fn join_command_switches_rooms_and_forgets_the_old_one() {
        let (mut state, _) = connected();
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Chat(chat_message(
                user("bob"),
                "старое",
            )))),
        );
        typed(&mut state, "/join rust");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert_eq!(
            commands,
            [Command::Connect {
                nickname: "alice".into(),
                room: "rust".into(),
                server: String::new(),
            }]
        );
        assert!(
            !texts(&state).contains(&"старое"),
            "переписка прошлой комнаты осталась на экране"
        );
    }

    #[test]
    fn open_command_targets_the_last_attachment() {
        let (mut state, _) = connected();
        state.media_base = "http://192.168.1.5:8080".into();
        let id = Uuid::from_u128(7);
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Chat(ChatMessage {
                id,
                from: user("bob"),
                text: String::new(),
                ts: 1_700_000_000_000,
                attachment: Some(Attachment {
                    id,
                    kind: common::AttachmentKind::Image,
                    name: "кот.png".into(),
                    size: 1024,
                    mime: "image/png".into(),
                }),
                reply: None,
            }))),
        );
        typed(&mut state, "/open");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert_eq!(
            commands,
            [Command::Open(format!("http://192.168.1.5:8080/media/{id}"))]
        );
    }

    #[test]
    fn open_command_without_attachments_explains_itself() {
        let (mut state, _) = connected();
        typed(&mut state, "/open");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty());
        assert!(matches!(
            state.entries.last(),
            Some(Entry::System {
                kind: SystemKind::Error,
                ..
            })
        ));
    }

    #[test]
    fn join_command_rejects_a_bad_room() {
        let (mut state, _) = connected();
        typed(&mut state, "/join общая");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty());
        assert!(matches!(
            state.entries.last(),
            Some(Entry::System {
                kind: SystemKind::Error,
                ..
            })
        ));
    }

    #[test]
    fn nick_command_reconnects_with_the_new_name() {
        let (mut state, _) = connected();
        typed(&mut state, "/nick bob");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert_eq!(
            commands,
            [Command::Connect {
                nickname: "bob".into(),
                room: "general".into(),
                server: String::new(),
            }]
        );
    }

    #[test]
    fn sent_lines_are_recalled_with_arrows() {
        let (mut state, _) = connected();
        for line in ["первое", "второе"] {
            typed(&mut state, line);
            update(&mut state, key(KeyCode::Enter));
        }
        typed(&mut state, "черновик");

        update(&mut state, key(KeyCode::Up));
        assert_eq!(state.input.text, "второе");
        update(&mut state, key(KeyCode::Up));
        assert_eq!(state.input.text, "первое");
        update(&mut state, key(KeyCode::Down));
        assert_eq!(state.input.text, "второе");

        // Спустившись ниже последней строки, получаем обратно недописанное.
        update(&mut state, key(KeyCode::Down));
        assert_eq!(state.input.text, "черновик");
    }

    #[test]
    fn editing_shortcuts_work_on_character_boundaries() {
        let (mut state, _) = connected();
        typed(&mut state, "привет большой мир");

        update(&mut state, ctrl('w'));
        assert_eq!(state.input.text, "привет большой ");

        update(&mut state, ctrl('a'));
        update(&mut state, key(KeyCode::Right));
        update(&mut state, ctrl('k'));
        assert_eq!(state.input.text, "п");

        update(&mut state, ctrl('u'));
        assert!(state.input.is_empty());
    }

    #[test]
    fn key_release_is_ignored() {
        let (mut state, _) = connected();
        let mut press = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        press.kind = KeyEventKind::Release;

        update(&mut state, Action::Key(press));

        // На Windows иначе каждый символ дублировался бы.
        assert!(state.input.is_empty());
    }

    #[test]
    fn input_is_capped_at_protocol_limit() {
        let (mut state, _) = connected();
        typed(&mut state, &"a".repeat(validate::MAX_TEXT_CHARS + 10));

        assert_eq!(state.input.len(), validate::MAX_TEXT_CHARS);
    }

    #[test]
    fn paste_is_flattened_to_one_line() {
        let (mut state, _) = connected();

        update(&mut state, Action::Paste("одна\nдве\tтри".into()));

        assert_eq!(state.input.text, "одна две три");
    }

    #[test]
    fn history_is_capped_and_forgets_old_ids() {
        let (mut state, _) = connected();

        for _ in 0..MAX_ENTRIES + 50 {
            update(
                &mut state,
                Action::Net(NetEvent::Message(ServerMessage::Chat(chat_message(
                    user("bob"),
                    "спам",
                )))),
            );
        }

        assert_eq!(state.entries.len(), MAX_ENTRIES);
        assert!(
            state.seen.len() <= MAX_ENTRIES,
            "множество виденных id растёт быстрее истории"
        );
    }

    #[test]
    fn esc_and_ctrl_c_request_shutdown() {
        for action in [key(KeyCode::Esc), ctrl('c')] {
            let (mut state, _) = connected();

            let commands = update(&mut state, action);

            assert_eq!(commands, [Command::Quit]);
            assert!(state.should_quit);
        }
    }

    /// Комната с несколькими репликами и заполненной картой строк:
    /// без неё поиску некуда прокручивать.
    fn with_history() -> State {
        let (mut state, _) = connected();
        for text in ["первое сообщение", "второе про котов", "третье про котов"]
        {
            update(
                &mut state,
                Action::Net(NetEvent::Message(ServerMessage::Chat(chat_message(
                    user("bob"),
                    text,
                )))),
            );
        }
        state.viewport = Viewport {
            height: 10,
            total_lines: 40,
        };
        state.entry_lines = (0..state.entries.len()).map(|index| index * 2).collect();
        state
    }

    #[test]
    fn ctrl_r_picks_the_last_message_and_sends_a_reply() {
        let mut state = with_history();

        update(&mut state, ctrl('r'));
        let picked = state.picking.expect("выбор не начался");
        update(&mut state, key(KeyCode::Enter));

        let target = state.replying.clone().expect("цитата не взведена");
        assert_eq!(target.excerpt, "третье про котов");
        assert!(state.picking.is_none());

        typed(&mut state, "согласен");
        let commands = update(&mut state, key(KeyCode::Enter));

        let Some(Command::Send(ClientMessage::Chat { reply_to, .. })) = commands.first() else {
            panic!("ответ не ушёл: {commands:?}");
        };
        // С сервером уходит только идентификатор: цитату он соберёт сам.
        assert_eq!(*reply_to, Some(target.id));
        let Entry::Chat { id, .. } = &state.entries[picked] else {
            panic!("выбрана не реплика");
        };
        assert_eq!(target.id, *id);
        // Цитата одноразовая: следующее сообщение уйдёт обычным.
        assert!(state.replying.is_none());
    }

    #[test]
    fn arrows_walk_through_messages_while_picking() {
        let mut state = with_history();
        update(&mut state, ctrl('r'));
        let last = state.picking.unwrap();

        update(&mut state, key(KeyCode::Up));
        assert!(state.picking.unwrap() < last, "вверх не сработало");

        update(&mut state, key(KeyCode::Down));
        assert_eq!(state.picking, Some(last));
    }

    #[test]
    fn picking_skips_system_lines() {
        let mut state = with_history();
        state.system(SystemKind::Info, "кто-то вошёл");
        update(&mut state, ctrl('r'));

        // Системная строка — не сообщение, отвечать на неё нечего.
        let picked = state.picking.expect("выбор не начался");
        assert!(matches!(state.entries[picked], Entry::Chat { .. }));
    }

    #[test]
    fn escape_drops_the_pending_reply_without_quitting() {
        let mut state = with_history();
        update(&mut state, ctrl('r'));
        update(&mut state, key(KeyCode::Enter));
        assert!(state.replying.is_some());

        let commands = update(&mut state, key(KeyCode::Esc));

        assert!(commands.is_empty());
        assert!(state.replying.is_none());
        // Esc снял цитату, а не закрыл программу.
        assert!(!state.should_quit);
    }

    #[test]
    fn escape_cancels_picking() {
        let mut state = with_history();
        update(&mut state, ctrl('r'));

        update(&mut state, key(KeyCode::Esc));

        assert!(state.picking.is_none());
        assert!(state.replying.is_none());
        assert!(!state.should_quit);
    }

    #[test]
    fn reply_to_a_picture_quotes_its_name() {
        let (mut state, _) = connected();
        let id = Uuid::from_u128(9);
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Chat(ChatMessage {
                id,
                from: user("bob"),
                text: String::new(),
                ts: 1_700_000_000_000,
                attachment: Some(Attachment {
                    id,
                    kind: AttachmentKind::Image,
                    name: "кот.png".into(),
                    size: 1024,
                    mime: "image/png".into(),
                }),
                reply: None,
            }))),
        );

        update(&mut state, ctrl('r'));
        update(&mut state, key(KeyCode::Enter));

        // У картинки без подписи цитировать нечего, кроме имени файла.
        assert_eq!(state.replying.unwrap().excerpt, "кот.png");
    }

    #[test]
    fn nothing_to_reply_to_is_explained() {
        let (mut state, _) = connected();

        update(&mut state, ctrl('r'));

        assert!(state.picking.is_none());
        assert!(matches!(
            state.entries.last(),
            Some(Entry::System {
                kind: SystemKind::Error,
                ..
            })
        ));
    }

    #[test]
    fn search_finds_matching_messages() {
        let mut state = with_history();

        update(&mut state, ctrl('f'));
        typed(&mut state, "котов");

        let search = state.search.as_ref().expect("поиск не открылся");
        assert_eq!(search.matches.len(), 2);
        // Начинаем с самого свежего: в переписке нужнее последнее совпадение.
        assert_eq!(search.current, 1);
    }

    #[test]
    fn search_walks_through_matches_in_a_circle() {
        let mut state = with_history();
        update(&mut state, ctrl('f'));
        typed(&mut state, "котов");

        update(&mut state, key(KeyCode::Enter));
        assert_eq!(state.search.as_ref().unwrap().current, 0);

        update(&mut state, key(KeyCode::Enter));
        assert_eq!(
            state.search.as_ref().unwrap().current,
            1,
            "перебор не замкнут"
        );

        update(&mut state, key(KeyCode::Up));
        assert_eq!(state.search.as_ref().unwrap().current, 0);
    }

    #[test]
    fn search_scrolls_to_the_found_message() {
        let mut state = with_history();
        state.scrollback = 0;

        update(&mut state, ctrl('f'));
        typed(&mut state, "первое");

        // Найденное далеко вверху — история обязана прокрутиться к нему.
        assert!(state.scrollback > 0, "поиск не прокрутил историю");
    }

    #[test]
    fn search_ignores_case() {
        let mut state = with_history();

        update(&mut state, ctrl('f'));
        typed(&mut state, "КОТОВ");

        assert_eq!(state.search.as_ref().unwrap().matches.len(), 2);
    }

    #[test]
    fn empty_query_matches_nothing() {
        let mut state = with_history();

        update(&mut state, ctrl('f'));

        assert!(state.search.as_ref().unwrap().matches.is_empty());
    }

    #[test]
    fn escape_closes_search_without_quitting() {
        let mut state = with_history();
        update(&mut state, ctrl('f'));
        typed(&mut state, "котов");

        let commands = update(&mut state, key(KeyCode::Esc));

        assert!(commands.is_empty());
        assert!(state.search.is_none());
        // Esc в поиске — это «закрыть поиск», а не «выйти из программы».
        assert!(!state.should_quit);
    }

    #[test]
    fn typing_during_search_does_not_reach_the_message_input() {
        let mut state = with_history();
        update(&mut state, ctrl('f'));

        typed(&mut state, "котов");

        assert!(state.input.is_empty());
        assert_eq!(state.search.as_ref().unwrap().query.text, "котов");
    }

    #[test]
    fn color_command_sets_own_color() {
        let (mut state, _) = connected();
        typed(&mut state, "/color #d97757");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert_eq!(commands, [Command::SaveConfig]);
        assert_eq!(
            state.colors.get("alice"),
            Some(&ratatui::style::Color::Rgb(217, 119, 87))
        );
    }

    #[test]
    fn color_command_sets_someone_elses_color() {
        let (mut state, _) = connected();
        typed(&mut state, "/color Bob cyan");

        update(&mut state, key(KeyCode::Enter));

        // Ключ всегда в нижнем регистре: Bob и bob — один человек.
        assert!(state.colors.contains_key("bob"));
    }

    #[test]
    fn color_command_resets_with_a_dash() {
        let (mut state, _) = connected();
        typed(&mut state, "/color bob cyan");
        update(&mut state, key(KeyCode::Enter));

        typed(&mut state, "/color bob -");
        let commands = update(&mut state, key(KeyCode::Enter));

        assert_eq!(commands, [Command::SaveConfig]);
        assert!(!state.colors.contains_key("bob"));
    }

    #[test]
    fn nonsense_color_is_rejected_without_saving() {
        let (mut state, _) = connected();
        typed(&mut state, "/color розовый в крапинку");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty(), "испорченный цвет пошёл в настройки");
        assert!(matches!(
            state.entries.last(),
            Some(Entry::System {
                kind: SystemKind::Error,
                ..
            })
        ));
    }

    #[test]
    fn successful_join_asks_to_remember_the_nickname() {
        let (mut state, _) = State::new(Some("alice".into()), "general".into());

        let commands = update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Welcome {
                your_id: Uuid::new_v4(),
                room: "general".into(),
                nickname: "alice".into(),
                users: vec![],
                history: vec![],
            })),
        );

        // Запоминаем то, что сервер принял, а не то, что ввели.
        assert!(commands.contains(&Command::SaveConfig));
    }

    #[test]
    fn notice_shows_up_as_a_system_error() {
        let (mut state, _) = connected();

        update(&mut state, Action::Notice("клавиатура отвалилась".into()));

        assert!(matches!(
            state.entries.last(),
            Some(Entry::System {
                kind: SystemKind::Error,
                ..
            })
        ));
    }

    #[test]
    fn typing_is_announced_at_most_once_in_a_while() {
        let (mut state, _) = connected();

        let first = update(&mut state, key(KeyCode::Char('п')));
        let second = update(&mut state, key(KeyCode::Char('р')));

        assert_eq!(first, [Command::Send(ClientMessage::Typing)]);
        // Второй символ подряд — не повод слать ещё раз.
        assert!(second.is_empty());
    }

    #[test]
    fn typing_is_not_announced_while_offline() {
        let (mut state, _) = State::new(Some("alice".into()), "general".into());

        let commands = update(&mut state, key(KeyCode::Char('п')));

        assert!(commands.is_empty());
    }

    #[test]
    fn sending_resets_the_typing_timer() {
        let (mut state, _) = connected();
        update(&mut state, key(KeyCode::Char('п')));
        typed(&mut state, "ривет");
        update(&mut state, key(KeyCode::Enter));

        // Новый набор — новая новость, ждать паузу заново незачем.
        let commands = update(&mut state, key(KeyCode::Char('е')));

        assert_eq!(commands, [Command::Send(ClientMessage::Typing)]);
    }

    #[test]
    fn someone_typing_is_visible_until_it_expires() {
        let (mut state, _) = connected();
        let bob = user("bob");

        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Typing {
                user: bob.clone(),
            })),
        );
        assert_eq!(state.typing_now(), ["bob"]);

        // Сигнал одноразовый: «перестал печатать» никто не присылает, строка
        // должна погаснуть сама.
        state
            .typing
            .insert(bob.id, ("bob".into(), Instant::now() - TYPING_TTL));
        assert!(state.typing_now().is_empty());
    }

    #[test]
    fn a_message_stops_the_typing_line() {
        let (mut state, _) = connected();
        let bob = user("bob");
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Typing {
                user: bob.clone(),
            })),
        );

        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Chat(chat_message(
                bob,
                "привет",
            )))),
        );

        assert!(state.typing_now().is_empty());
    }

    #[test]
    fn leaving_stops_the_typing_line() {
        let (mut state, _) = connected();
        let bob = user("bob");
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Typing {
                user: bob.clone(),
            })),
        );

        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::UserLeft { user: bob })),
        );

        assert!(state.typing_now().is_empty());
    }

    fn picture_message(id: Uuid, kind: AttachmentKind) -> ServerMessage {
        ServerMessage::Chat(ChatMessage {
            id,
            from: user("bob"),
            text: String::new(),
            ts: 1_700_000_000_000,
            attachment: Some(Attachment {
                id,
                kind,
                name: "кот.png".into(),
                size: 1024,
                mime: "image/png".into(),
            }),
            reply: None,
        })
    }

    /// Клиент в терминале с настоящей графикой.
    fn with_graphics() -> State {
        let (mut state, _) = connected();
        state.inline_images = true;
        state.media_base = "http://127.0.0.1:8080".into();
        state
    }

    #[test]
    fn picture_in_the_feed_is_fetched_by_itself() {
        let mut state = with_graphics();
        let id = Uuid::from_u128(11);

        let commands = update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                id,
                AttachmentKind::Image,
            ))),
        );

        assert!(commands.contains(&Command::Fetch(
            id,
            format!("http://127.0.0.1:8080/media/{id}")
        )));
        assert!(matches!(
            state.thumbnails.get(&id),
            Some(Thumbnail::Loading)
        ));
    }

    #[test]
    fn nothing_is_fetched_when_the_terminal_cannot_draw() {
        let (mut state, _) = connected();
        state.media_base = "http://127.0.0.1:8080".into();
        let id = Uuid::from_u128(12);

        let commands = update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                id,
                AttachmentKind::Image,
            ))),
        );

        // Полублоками миниатюра в десять строк — цветной шум, качать её незачем.
        assert!(commands.is_empty());
        assert!(state.thumbnails.is_empty());
    }

    #[test]
    fn voice_messages_are_not_fetched_as_pictures() {
        let mut state = with_graphics();
        let id = Uuid::from_u128(13);

        let commands = update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                id,
                AttachmentKind::Audio,
            ))),
        );

        assert!(commands.is_empty());
    }

    #[test]
    fn the_same_picture_is_fetched_once() {
        let mut state = with_graphics();
        let id = Uuid::from_u128(14);
        update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                id,
                AttachmentKind::Image,
            ))),
        );

        // Та же картинка приходит второй раз — например, в истории комнаты
        // после переподключения.
        let again = update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                id,
                AttachmentKind::Image,
            ))),
        );

        assert!(again.is_empty());
    }

    #[test]
    fn downloaded_picture_lands_in_the_feed() {
        let mut state = with_graphics();
        let id = Uuid::from_u128(15);
        update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                id,
                AttachmentKind::Image,
            ))),
        );

        update(
            &mut state,
            Action::Image(id, Ok(Box::new(image::RgbImage::new(4, 4)))),
        );

        assert!(matches!(
            state.thumbnails.get(&id),
            Some(Thumbnail::Ready(_))
        ));
    }

    #[test]
    fn a_broken_picture_is_remembered_as_broken() {
        let mut state = with_graphics();
        let id = Uuid::from_u128(16);
        update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                id,
                AttachmentKind::Image,
            ))),
        );

        update(&mut state, Action::Image(id, Err("404".into())));

        // Иначе клиент качал бы её снова и снова на каждом новом сообщении.
        assert!(matches!(state.thumbnails.get(&id), Some(Thumbnail::Failed)));
    }

    #[test]
    fn one_download_serves_both_the_viewer_and_the_feed() {
        let mut state = with_graphics();
        let id = Uuid::from_u128(17);
        update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                id,
                AttachmentKind::Image,
            ))),
        );
        typed(&mut state, "/view");
        update(&mut state, key(KeyCode::Enter));

        update(
            &mut state,
            Action::Image(id, Ok(Box::new(image::RgbImage::new(4, 4)))),
        );

        assert!(matches!(
            state.viewer.as_ref().map(|viewer| &viewer.state),
            Some(ViewerState::Ready(_))
        ));
        assert!(matches!(
            state.thumbnails.get(&id),
            Some(Thumbnail::Ready(_))
        ));
    }

    #[test]
    fn clearing_the_screen_forgets_the_pictures() {
        let mut state = with_graphics();
        let id = Uuid::from_u128(18);
        update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                id,
                AttachmentKind::Image,
            ))),
        );
        typed(&mut state, "/clear");

        update(&mut state, key(KeyCode::Enter));

        assert!(state.thumbnails.is_empty());
    }

    /// Комната с голосовым от bob.
    fn with_voice() -> (State, Uuid) {
        let (mut state, _) = connected();
        state.media_base = "http://127.0.0.1:8080".into();
        let id = Uuid::from_u128(31);
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Chat(ChatMessage {
                id,
                from: user("bob"),
                text: String::new(),
                ts: 1_700_000_000_000,
                attachment: Some(Attachment {
                    id,
                    kind: AttachmentKind::Audio,
                    name: "голосовое.webm".into(),
                    size: 4096,
                    mime: "audio/webm".into(),
                }),
                reply: None,
            }))),
        );
        (state, id)
    }

    #[test]
    fn play_command_downloads_the_last_voice() {
        let (mut state, id) = with_voice();
        typed(&mut state, "/play");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert_eq!(
            commands,
            [Command::PlayVoice(format!(
                "http://127.0.0.1:8080/media/{id}"
            ))]
        );
    }

    #[test]
    fn play_command_refuses_a_picture() {
        let mut state = with_graphics();
        update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                Uuid::from_u128(32),
                AttachmentKind::Image,
            ))),
        );
        typed(&mut state, "/play");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty());
        assert!(matches!(
            state.entries.last(),
            Some(Entry::System {
                kind: SystemKind::Error,
                ..
            })
        ));
    }

    #[test]
    fn stop_command_stops_playback() {
        let (mut state, _) = with_voice();
        typed(&mut state, "/stop");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert_eq!(commands, [Command::StopVoice]);
    }

    #[test]
    fn save_command_uses_the_attachment_name_by_default() {
        let (mut state, _) = with_voice();
        typed(&mut state, "/save");

        let commands = update(&mut state, key(KeyCode::Enter));

        let [Command::Save { destination, .. }] = commands.as_slice() else {
            panic!("сохранение не началось: {commands:?}");
        };
        // Имя берётся из вложения: придумывать своё — терять то, по которому
        // файл потом искать.
        assert_eq!(
            destination.file_name().unwrap().to_string_lossy(),
            "голосовое.webm"
        );
    }

    #[test]
    fn save_command_takes_an_explicit_path() {
        let (mut state, _) = with_voice();
        typed(&mut state, "/save C:\\звуки\\привет.webm");

        let commands = update(&mut state, key(KeyCode::Enter));

        let [Command::Save { destination, .. }] = commands.as_slice() else {
            panic!("сохранение не началось: {commands:?}");
        };
        assert!(destination.to_string_lossy().ends_with("привет.webm"));
    }

    #[test]
    fn saving_reports_where_the_file_landed() {
        let (mut state, _) = connected();

        update(
            &mut state,
            Action::Saved(Ok(std::path::PathBuf::from("C:/загрузки/кот.png"))),
        );

        let Some(Entry::System { text, kind }) = state.entries.last() else {
            panic!("нет сообщения о сохранении");
        };
        assert_eq!(*kind, SystemKind::Info);
        assert!(text.contains("кот.png"), "{text}");
    }

    #[test]
    fn long_work_shows_a_spinner_and_clears_it() {
        let (mut state, _) = with_voice();
        typed(&mut state, "/save");
        update(&mut state, key(KeyCode::Enter));

        assert!(state.busy.is_some(), "бегунок не появился");

        update(
            &mut state,
            Action::Saved(Ok(std::path::PathBuf::from("кот.png"))),
        );

        assert!(state.busy.is_none(), "бегунок остался крутиться");
    }

    #[test]
    fn background_thumbnails_do_not_touch_the_spinner() {
        let mut state = with_graphics();
        let id = Uuid::from_u128(41);
        update(
            &mut state,
            Action::Net(NetEvent::Message(picture_message(
                id,
                AttachmentKind::Image,
            ))),
        );
        state.busy = Some("сохраняю кот.png".into());

        update(
            &mut state,
            Action::Image(id, Ok(Box::new(image::RgbImage::new(2, 2)))),
        );

        // Миниатюры качаются молча: снимать чужой бегунок они не должны.
        assert_eq!(state.busy.as_deref(), Some("сохраняю кот.png"));
    }

    fn entry(name: &str, is_dir: bool) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            path: std::path::PathBuf::from(format!("C:/фото/{name}")),
            is_dir,
            size: 1024,
            supported: !is_dir,
        }
    }

    /// Клиент с открытым обзором и прочитанным каталогом.
    fn with_browser() -> State {
        let (mut state, _) = connected();
        state.media_base = "http://127.0.0.1:8080".into();
        typed(&mut state, "/send");
        update(&mut state, key(KeyCode::Enter));
        update(
            &mut state,
            Action::Directory {
                dir: std::path::PathBuf::from("C:/фото"),
                result: Ok(vec![
                    entry("..", true),
                    entry("вложенный", true),
                    entry("кот.png", false),
                    entry("пёс.png", false),
                ]),
            },
        );
        state
    }

    #[test]
    fn send_without_a_path_opens_the_browser() {
        let (mut state, _) = connected();
        state.media_base = "http://127.0.0.1:8080".into();
        typed(&mut state, "/send");

        let commands = update(&mut state, key(KeyCode::Enter));

        // Набирать путь руками — ровно тот костыль, ради которого обзор и есть.
        assert!(state.browser.is_some());
        assert!(matches!(commands.as_slice(), [Command::ReadDir(_)]));
    }

    #[test]
    fn send_with_a_path_still_works_directly() {
        let (mut state, _) = connected();
        state.media_base = "http://127.0.0.1:8080".into();
        typed(&mut state, r"/send C:\фото\кот.png");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(state.browser.is_none());
        assert!(matches!(commands.as_slice(), [Command::Upload(_)]));
    }

    #[test]
    fn arrows_walk_the_listing() {
        let mut state = with_browser();

        update(&mut state, key(KeyCode::Down));
        update(&mut state, key(KeyCode::Down));

        let browser = state.browser.as_ref().unwrap();
        assert_eq!(browser.current().unwrap().name, "кот.png");
    }

    #[test]
    fn enter_on_a_directory_reads_it() {
        let mut state = with_browser();
        update(&mut state, key(KeyCode::Down));

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(matches!(commands.as_slice(), [Command::ReadDir(_)]));
        // Обзор не закрывается: мы всё ещё выбираем файл.
        assert!(state.browser.is_some());
    }

    #[test]
    fn enter_on_a_file_sends_it_and_remembers_the_directory() {
        let mut state = with_browser();
        update(&mut state, key(KeyCode::Down));
        update(&mut state, key(KeyCode::Down));

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(state.browser.is_none(), "обзор не закрылся");
        assert!(matches!(
            commands.as_slice(),
            [Command::Upload(_), Command::SaveConfig]
        ));
        // В следующий раз обзор откроется там же, где выбирали в прошлый.
        assert_eq!(state.last_dir.as_deref(), Some("C:/фото"));
        assert!(state.busy.is_some(), "бегунок не появился");
    }

    #[test]
    fn typing_filters_the_listing() {
        let mut state = with_browser();

        typed(&mut state, "пёс");

        let browser = state.browser.as_ref().unwrap();
        let names: Vec<&str> = browser
            .visible()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        // «..» остаётся всегда: иначе из отфильтрованного каталога некуда деться.
        assert_eq!(names, ["..", "пёс.png"]);
    }

    #[test]
    fn escape_closes_the_browser_without_quitting() {
        let mut state = with_browser();

        let commands = update(&mut state, key(KeyCode::Esc));

        assert!(commands.is_empty());
        assert!(state.browser.is_none());
        assert!(!state.should_quit);
    }

    #[test]
    fn unreadable_directory_keeps_the_browser_open() {
        let mut state = with_browser();

        update(
            &mut state,
            Action::Directory {
                dir: std::path::PathBuf::from("C:/закрытый"),
                result: Err("отказано в доступе".into()),
            },
        );

        // Закрывать обзор из-за одного нечитаемого каталога незачем: человек
        // просто вернётся назад.
        let browser = state.browser.as_ref().expect("обзор закрылся");
        assert!(browser.error.is_some());
        assert!(!browser.loading);
    }

    #[test]
    fn shortcuts_work_on_a_russian_layout() {
        // Ctrl+F на русской раскладке физически приходит как «Ctrl+а».
        let (mut state, _) = connected();
        update(&mut state, ctrl('а'));
        assert!(state.search.is_some(), "поиск не открылся по Ctrl+а");

        let mut state = with_history();
        update(&mut state, ctrl('к'));
        assert!(state.picking.is_some(), "ответ не начался по Ctrl+к");

        let (mut state, _) = connected();
        let commands = update(&mut state, ctrl('с'));
        assert_eq!(commands, [Command::Quit], "выход не сработал по Ctrl+с");
    }

    #[test]
    fn russian_letters_still_type_normally() {
        let (mut state, _) = connected();

        typed(&mut state, "как дела");

        // Подмена касается только сочетаний с Ctrl: обычный набор не трогаем.
        assert_eq!(state.input.text, "как дела");
    }

    #[test]
    fn editing_shortcuts_work_on_a_russian_layout() {
        let (mut state, _) = connected();
        typed(&mut state, "привет большой мир");

        // Ctrl+W — это «Ctrl+ц».
        update(&mut state, ctrl('ц'));

        assert_eq!(state.input.text, "привет большой ");
    }

    #[test]
    fn rejected_nickname_comes_back_into_the_field() {
        let (mut state, _) = State::new(None, "general".into());
        typed(&mut state, "крутолёт");
        update(&mut state, key(KeyCode::Enter));

        update(
            &mut state,
            Action::Net(NetEvent::Fatal {
                reason: "ник крутолёт в этой комнате уже занят".into(),
            }),
        );

        let Screen::Login(login) = &state.screen else {
            panic!("ожидался экран входа");
        };
        // Стереть и набрать ник заново — лишняя работа: правится обычно
        // одна буква.
        assert_eq!(login.nickname.text, "крутолёт");
    }

    #[test]
    fn pasted_address_lets_you_join_a_friend() {
        let (mut state, _) = State::new(None, "general".into());
        state.set_server(crate::net::DEFAULT_SERVER.to_string());
        typed(&mut state, "alice");
        update(&mut state, key(KeyCode::Tab));
        update(&mut state, key(KeyCode::Tab));
        update(&mut state, ctrl('u'));
        // Ровно то, что присылают в мессенджере.
        typed(&mut state, "192.168.1.5:8080");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(matches!(
            commands.as_slice(),
            [Command::Connect { server, .. }] if server == "ws://192.168.1.5:8080/ws"
        ));
    }

    #[test]
    fn broken_address_keeps_you_on_the_login_screen() {
        let (mut state, _) = State::new(None, "general".into());
        state.set_server(crate::net::DEFAULT_SERVER.to_string());
        typed(&mut state, "alice");
        update(&mut state, key(KeyCode::Tab));
        update(&mut state, key(KeyCode::Tab));
        update(&mut state, ctrl('u'));
        typed(&mut state, "ws://");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty());
        let Screen::Login(login) = &state.screen else {
            panic!("ожидался экран входа");
        };
        assert_eq!(login.field, Field::Server);
        assert!(login.error.is_some());
    }

    #[test]
    fn host_command_raises_a_server_and_reconnects() {
        let (mut state, _) = connected();
        typed(&mut state, "/host");

        let commands = update(&mut state, key(KeyCode::Enter));
        assert_eq!(commands, [Command::Host(8080)]);
        assert!(state.busy.is_some());

        let commands = update(
            &mut state,
            Action::Hosted {
                url: "ws://127.0.0.1:8080/ws".into(),
                lines: vec!["друг подключается: 192.168.1.5:8080".into()],
            },
        );

        // Приглашение видно в переписке, а клиент сразу входит к себе же.
        assert!(
            texts(&state)
                .iter()
                .any(|line| line.contains("192.168.1.5"))
        );
        assert!(matches!(
            commands.as_slice(),
            [Command::Connect { server, .. }] if server == "ws://127.0.0.1:8080/ws"
        ));
        assert!(state.busy.is_none());
    }

    #[test]
    fn host_command_rejects_a_nonsense_port() {
        let (mut state, _) = connected();
        typed(&mut state, "/host восемь тысяч");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty());
        assert!(state.busy.is_none());
    }

    #[test]
    fn tab_completes_a_command() {
        let (mut state, _) = connected();
        typed(&mut state, "/se");

        update(&mut state, key(KeyCode::Tab));

        // Набирать команду целиком, когда клиент знает список, — лишняя работа.
        assert_eq!(state.input.text, "/send ");
    }

    #[test]
    fn ctrl_o_opens_the_file_browser() {
        let (mut state, _) = connected();
        state.media_base = "http://127.0.0.1:8080".into();

        let commands = update(&mut state, ctrl('o'));

        assert!(state.browser.is_some());
        assert!(matches!(commands.as_slice(), [Command::ReadDir(_)]));
    }

    #[test]
    fn rec_command_toggles_recording() {
        let (mut state, _) = connected();
        typed(&mut state, "/rec");

        let commands = update(&mut state, key(KeyCode::Enter));

        // Одна команда на оба действия: во время записи всё равно ничего
        // другого не делаешь, а помнить две — лишнее.
        assert_eq!(commands, [Command::ToggleRecording]);
    }

    #[test]
    fn tab_completes_a_nickname() {
        let (mut state, _) = connected();
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::UserJoined {
                user: user("bobby"),
            })),
        );
        typed(&mut state, "bo");

        update(&mut state, key(KeyCode::Tab));

        // В начале строки к человеку обращаются через запятую.
        assert_eq!(state.input.text, "bobby, ");
        assert_eq!(state.input.cursor, 7);
    }

    #[test]
    fn repeated_tab_cycles_through_matches() {
        let (mut state, _) = connected();
        for nickname in ["bob", "bobby"] {
            update(
                &mut state,
                Action::Net(NetEvent::Message(ServerMessage::UserJoined {
                    user: user(nickname),
                })),
            );
        }
        typed(&mut state, "привет bo");

        update(&mut state, key(KeyCode::Tab));
        let first = state.input.text.clone();
        update(&mut state, key(KeyCode::Tab));
        let second = state.input.text.clone();
        update(&mut state, key(KeyCode::Tab));

        assert_ne!(first, second, "перебор стоит на месте");
        // Перебор замкнут: после последнего снова первый.
        assert_eq!(state.input.text, first);
        // Не в начале строки запятая не нужна.
        assert!(first.starts_with("привет bob"), "{first}");
    }

    #[test]
    fn tab_without_matches_leaves_the_line_alone() {
        let (mut state, _) = connected();
        typed(&mut state, "zzz");

        update(&mut state, key(KeyCode::Tab));

        assert_eq!(state.input.text, "zzz");
    }

    #[test]
    fn typing_resets_the_completion_cycle() {
        let (mut state, _) = connected();
        for nickname in ["bob", "bobby"] {
            update(
                &mut state,
                Action::Net(NetEvent::Message(ServerMessage::UserJoined {
                    user: user(nickname),
                })),
            );
        }
        typed(&mut state, "bo");
        update(&mut state, key(KeyCode::Tab));

        typed(&mut state, "bo");
        update(&mut state, key(KeyCode::Tab));

        // Перебор начался заново: иначе Tab подставил бы следующего по списку
        // и получилось бы «bob, bobby».
        assert_eq!(state.input.text, "bob, bob ");
    }

    #[test]
    fn mention_rings_the_bell() {
        let (mut state, _) = connected();

        let commands = update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Chat(chat_message(
                user("bob"),
                "alice, ты тут?",
            )))),
        );

        // Единственное уведомление, доступное из терминала.
        assert_eq!(commands, [Command::Bell]);
    }

    #[test]
    fn ordinary_message_stays_silent() {
        let (mut state, _) = connected();

        let commands = update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Chat(chat_message(
                user("bob"),
                "просто сообщение",
            )))),
        );

        assert!(commands.is_empty());
    }

    #[test]
    fn wheel_scrolls_the_history() {
        let (mut state, _) = connected();
        state.viewport = Viewport {
            height: 10,
            total_lines: 40,
        };

        update(&mut state, Action::Scroll(3));
        assert_eq!(state.scrollback, 3);

        update(&mut state, Action::Scroll(-10));
        // Ниже низа истории прокрутка не уходит.
        assert_eq!(state.scrollback, 0);

        update(&mut state, Action::Scroll(1000));
        assert_eq!(state.scrollback, 30, "прокрутка уехала за историю");
    }

    #[test]
    fn scrollback_is_clamped_to_history_height() {
        let (mut state, _) = connected();
        state.viewport = Viewport {
            height: 10,
            total_lines: 15,
        };

        for _ in 0..5 {
            update(&mut state, key(KeyCode::PageUp));
        }
        assert_eq!(
            state.scrollback, 5,
            "прокрутка не должна уезжать за историю"
        );

        for _ in 0..5 {
            update(&mut state, key(KeyCode::PageDown));
        }
        assert_eq!(state.scrollback, 0);
    }
}
