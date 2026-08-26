//! Состояние клиента и переходы между состояниями.
//!
//! Логика намеренно вынесена из рендера и из сети: `update` — обычная функция
//! без ввода-вывода, поэтому поведение клиента проверяется юнит-тестами, а не
//! глазами в терминале.

use std::{collections::HashSet, time::Instant};

use common::{
    Attachment, AttachmentKind, ChatMessage, ClientMessage, REPLY_EXCERPT_CHARS, ReplyPreview,
    ServerMessage, UserInfo, validate,
};
use image::RgbImage;
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
    "/view — показать последнюю картинку прямо в терминале",
    "/open — открыть последнее вложение внешней программой",
    "/clear — очистить историю на экране",
    "/quit — выход",
    "// в начале строки — отправить текст, начинающийся со слэша",
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

/// Что показывает просмотрщик картинок поверх переписки.
#[derive(Debug)]
pub enum ViewerState {
    Loading,
    Ready(Box<RgbImage>),
    Failed(String),
}

#[derive(Debug)]
pub struct Viewer {
    pub name: String,
    pub state: ViewerState,
}

#[derive(Debug, Clone)]
pub enum Action {
    Key(KeyEvent),
    Paste(String),
    Net(NetEvent),
    /// Картинка скачана и разобрана — или не вышло.
    Image(Result<Box<RgbImage>, String>),
    /// Прокрутка колесом: вверх — положительное число строк.
    Scroll(i32),
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
}

/// Экран входа. Он же — место, куда клиент возвращается, если ник занят:
/// человек правит одно поле и пробует снова, не перезапуская программу.
#[derive(Debug, Clone, Default)]
pub struct Login {
    pub nickname: Input,
    pub room: Input,
    pub field: Field,
    pub error: Option<String>,
}

impl Login {
    fn active(&mut self) -> &mut Input {
        match self.field {
            Field::Nickname => &mut self.nickname,
            Field::Room => &mut self.room,
        }
    }

    fn limit(&self) -> usize {
        match self.field {
            Field::Nickname => validate::MAX_NICKNAME_CHARS,
            Field::Room => validate::MAX_ROOM_CHARS,
        }
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
    /// Поднять соединение заново — при входе, смене комнаты или ника.
    Connect {
        nickname: String,
        room: String,
    },
    /// Открыть адрес системным просмотрщиком.
    Open(String),
    /// Скачать картинку, чтобы показать её в терминале.
    Fetch(String),
    /// Звоночек терминала: единственное уведомление, доступное из TUI.
    Bell,
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
    /// http-адрес сервера: из него собираются ссылки на вложения.
    pub media_base: String,
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
    pub should_quit: bool,
}

impl State {
    /// Создаёт состояние. Если ник уже известен из аргументов, экран входа
    /// пропускается и клиент сразу подключается.
    pub fn new(nickname: Option<String>, room: String) -> (Self, Vec<Command>) {
        let mut state = Self {
            screen: Screen::Login(Login {
                nickname: Input::new(nickname.clone().unwrap_or_default()),
                room: Input::new(room.clone()),
                field: Field::Nickname,
                error: None,
            }),
            nickname: String::new(),
            room: room.clone(),
            media_base: String::new(),
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
            should_quit: false,
        };

        match nickname {
            Some(nickname) => {
                state.nickname = nickname.clone();
                state.screen = Screen::Chat;
                (state, vec![Command::Connect { nickname, room }])
            }
            None => (state, Vec::new()),
        }
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
        });
        mentions_me
    }

    fn forget_room(&mut self) {
        self.entries.clear();
        self.seen.clear();
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
        Action::Net(event) => on_net(state, event),
        Action::Image(result) => {
            if let Some(viewer) = &mut state.viewer {
                viewer.state = match result {
                    Ok(image) => ViewerState::Ready(image),
                    Err(reason) => ViewerState::Failed(reason),
                };
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
        KeyCode::Tab | KeyCode::Down => {
            login.field = match login.field {
                Field::Nickname => Field::Room,
                Field::Room => Field::Nickname,
            };
        }
        KeyCode::BackTab | KeyCode::Up => {
            login.field = match login.field {
                Field::Nickname => Field::Room,
                Field::Room => Field::Nickname,
            };
        }
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

    state.nickname = nickname.clone();
    state.room = room.clone();
    state.screen = Screen::Chat;
    state.forget_room();
    vec![Command::Connect { nickname, room }]
}

/// Прокручивает историю: положительное число строк — вверх, к прошлому.
fn scroll_by(state: &mut State, delta: i32) {
    let scrollback = state.scrollback as i64 + i64::from(delta);
    state.scrollback = scrollback.clamp(0, state.viewport.max_scroll() as i64) as usize;
}

/// Дополняет ник по Tab, перебирая совпадения при повторных нажатиях.
fn complete_nickname(state: &mut State) {
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

    let key = validate::nickname_key(&prefix);
    let matches: Vec<String> = state
        .users
        .iter()
        .filter(|user| validate::nickname_key(&user.nickname).starts_with(&key))
        .map(|user| user.nickname.clone())
        .collect();
    if matches.is_empty() {
        return;
    }

    let index = state
        .completion
        .as_ref()
        .map_or(0, |completion| (completion.index + 1) % matches.len());
    // В начале строки к человеку принято обращаться через запятую.
    let suffix = if at == 0 { ", " } else { " " };
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
        KeyCode::Tab => complete_nickname(state),
        _ => {
            edit_key(&mut state.input, key, validate::MAX_TEXT_CHARS);
        }
    }
    Vec::new()
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
            for line in HELP {
                state.system(SystemKind::Info, *line);
            }
            Vec::new()
        }
        "quit" | "exit" => {
            state.should_quit = true;
            vec![Command::Quit]
        }
        "clear" => {
            state.entries.clear();
            state.seen.clear();
            state.scrollback = 0;
            Vec::new()
        }
        "view" => view_command(state),
        "open" => open_command(state),
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

    state.viewer = Some(Viewer {
        name: attachment.name,
        state: ViewerState::Loading,
    });
    vec![Command::Fetch(url)]
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
                room: "general".into()
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

        assert_eq!(
            commands,
            [Command::Connect {
                nickname: "alice".into(),
                room: "general".into()
            }]
        );
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
        typed(&mut state, "/help");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty());
        assert!(texts(&state).iter().any(|line| line.contains("/join")));
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
                room: "rust".into()
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
                room: "general".into()
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
        for text in ["первое сообщение", "второе про котов", "третье про котов"] {
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
        assert_eq!(state.search.as_ref().unwrap().current, 1, "перебор не замкнут");

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
