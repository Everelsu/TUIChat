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
    RoomSummary, ServerMessage, UserInfo, validate,
};
use image::RgbImage;
use ratatui::style::Color;

use crate::{config, files::FileEntry, theme::Theme};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use uuid::Uuid;

/// Сколько сообщений держим в истории. Без потолка длинная сессия съедает
/// память, а прокрутка на десятки тысяч строк всё равно бесполезна.
pub const MAX_ENTRIES: usize = 2000;

/// Сколько отправленных строк можно пролистать стрелкой вверх.
const MAX_SENT: usize = 100;

const SCROLL_STEP: usize = 10;

pub const HELP: &[&str] = &[
    // Клавиши идут первыми и без слэшей: тому, кто не пишет код, команда
    // «/rec» ничего не говорит, а подписанная клавиша на клавиатуре — говорит.
    "F2 — записать голосовое, повторно отправить",
    "F3 — послушать последнее голосовое, повторно стоп",
    "F4 — отправить файл: любой, не только картинку",
    "F5 — сохранить присланное на диск",
    "F6 — открыть присланное в системе",
    "F7 — выбрать любое вложение стрелками, enter открыть",
    "щелчок по вложению — открыть его",
    "перетащить файл в окно — отправить его",
    "F1 — эта справка",
    "esc — выйти из комнаты в меню",
    "ctrl+q — закрыть программу",
    "ctrl+p — колонка с людьми справа",
    "",
    "/join <комната> — перейти в другую комнату",
    "/rooms — показать комнаты на сервере",
    "/nick <ник> — сменить ник",
    "/send [путь] — отправить файл, без пути — выбрать",
    "/view — показать картинку в терминале",
    "/rec — записать голосовое, повторно — отправить",
    "/play, /stop — проиграть голосовое и остановить",
    "/save [путь] — сохранить вложение на диск",
    "/open — открыть вложение внешней программой",
    "/color [ник] <цвет> — цвет ника, «-» сбрасывает",
    "/host [порт] — поднять свой сервер и позвать друга",
    "/menu — вернуться в меню",
    "/clear — очистить историю на экране",
    "/quit — выход",
    "//текст — отправить текст со слэша в начале",
];

/// Команды для дополнения по Tab. Порядок — как в справке.
const COMMANDS: [&str; 16] = [
    "/help", "/join", "/rooms", "/nick", "/send", "/view", "/play", "/stop", "/save", "/open",
    "/color", "/clear", "/host", "/menu", "/rec", "/quit",
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
    Voice(Uuid, Result<Vec<u8>, String>),
    /// Форма волны разобрана — теперь голосовое рисуется графиком.
    Waveform(Uuid, crate::media::Waveform),
    /// Сервер поднят прямо здесь: адрес для себя и строки-приглашения.
    Hosted {
        url: String,
        lines: Vec<String>,
    },
    /// Список комнат с сервера — или причина, почему его нет.
    Rooms(Result<Vec<RoomSummary>, String>),
    /// Прокрутка колесом: вверх — положительное число строк.
    Scroll(i32),
    /// Щелчок мышью по окну: колонка и строка.
    Click(u16, u16),
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
    /// Порт своего сервера на вкладке «поднять».
    Port,
}

/// Вкладки главного экрана.
///
/// Раньше вход был единственной формой, а всё остальное пряталось за
/// командами со слэшем: поднять свой сервер можно было только из чата, куда
/// ещё надо было как-то попасть. Вкладки показывают все три входа в чат
/// сразу — зайти, поднять, настроить, — и учить для этого нечего.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HomeTab {
    #[default]
    Join,
    Host,
    Look,
    Sound,
    Help,
}

impl HomeTab {
    pub const ALL: [HomeTab; 5] = [
        HomeTab::Join,
        HomeTab::Host,
        HomeTab::Look,
        HomeTab::Sound,
        HomeTab::Help,
    ];

    pub fn title(self) -> &'static str {
        match self {
            HomeTab::Join => "войти",
            HomeTab::Host => "поднять",
            HomeTab::Look => "вид",
            HomeTab::Sound => "звук",
            HomeTab::Help => "справка",
        }
    }

    /// Значок вкладки. Он же метка: по нему вкладка узнаётся быстрее, чем
    /// по слову, когда взгляд бежит по строке.
    pub fn icon(self) -> &'static str {
        match self {
            HomeTab::Join => "◆",
            HomeTab::Host => "✦",
            HomeTab::Look => "◐",
            HomeTab::Sound => "♪",
            HomeTab::Help => "?",
        }
    }

    pub fn index(self) -> usize {
        HomeTab::ALL
            .iter()
            .position(|&tab| tab == self)
            .unwrap_or(0)
    }

    fn shift(self, back: bool) -> Self {
        let len = HomeTab::ALL.len();
        let next = if back {
            self.index() + len - 1
        } else {
            self.index() + 1
        };
        HomeTab::ALL[next % len]
    }

    /// Поля, по которым ходят стрелки на этой вкладке.
    fn fields(self) -> &'static [Field] {
        match self {
            HomeTab::Join => &[Field::Nickname, Field::Room, Field::Server],
            HomeTab::Host => &[Field::Nickname, Field::Room, Field::Port],
            HomeTab::Look | HomeTab::Sound | HomeTab::Help => &[],
        }
    }
}

/// Строка настроек на вкладке «вид».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Theme,
    Images,
    Sidebar,
    Terminal,
}

impl Setting {
    pub const ALL: [Setting; 4] = [
        Setting::Theme,
        Setting::Images,
        Setting::Sidebar,
        Setting::Terminal,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Setting::Theme => "тема",
            Setting::Images => "картинки в ленте",
            Setting::Sidebar => "панель людей",
            Setting::Terminal => "свой терминал",
        }
    }

    /// Строка под настройкой: чем выбор обернётся на деле.
    pub fn hint(self) -> &'static str {
        match self {
            Setting::Theme => "цвет рамок, вкладок и своих сообщений",
            Setting::Images => "показывать присланное прямо в переписке",
            Setting::Sidebar => "кто в комнате — колонкой справа, ctrl+p",
            Setting::Terminal => "открываться заново там, где есть цвет и графика",
        }
    }
}

/// Строка настроек на вкладке «звук».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundSetting {
    Output,
    Input,
    Chime,
    Volume,
}

impl SoundSetting {
    pub const ALL: [SoundSetting; 4] = [
        SoundSetting::Output,
        SoundSetting::Input,
        SoundSetting::Chime,
        SoundSetting::Volume,
    ];

    pub fn title(self) -> &'static str {
        match self {
            SoundSetting::Output => "динамики",
            SoundSetting::Input => "микрофон",
            SoundSetting::Chime => "звоночек",
            SoundSetting::Volume => "громкость",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            SoundSetting::Output => "куда играют голосовые и звоночек",
            SoundSetting::Input => "с чего пишется голосовое по F2",
            SoundSetting::Chime => "сигнал, когда в комнате назвали ваш ник",
            SoundSetting::Volume => "громкость сигнала, голосовых не касается",
        }
    }
}

/// Звук: что нашлось в системе и что из этого выбрано.
#[derive(Debug, Clone)]
pub struct Audio {
    /// Имена устройств, найденных при запуске. Список не обновляется на ходу:
    /// опрос звуковой подсистемы не бесплатный, а наушники между двумя
    /// нажатиями стрелки обычно не меняются.
    pub outputs: Vec<String>,
    pub inputs: Vec<String>,
    /// Что выбрано. `None` — «как в системе»: этот выбор переживает и смену
    /// наушников, и перезапуск, поэтому он же и по умолчанию.
    pub output: Option<String>,
    pub input: Option<String>,
    /// Подавать ли сигнал при упоминании.
    pub chime: bool,
    /// Ступень громкости сигнала.
    pub volume: usize,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            outputs: Vec::new(),
            inputs: Vec::new(),
            output: None,
            input: None,
            chime: true,
            volume: 1,
        }
    }
}

/// Листает выбор по кругу: «как в системе», а дальше найденные устройства.
fn cycle_device(list: &[String], current: &Option<String>, back: bool) -> Option<String> {
    if list.is_empty() {
        return None;
    }
    let at = match current {
        None => 0,
        Some(name) => list
            .iter()
            .position(|found| found == name)
            .map_or(0, |i| i + 1),
    };
    let len = list.len() + 1;
    let next = if back { at + len - 1 } else { at + 1 };
    match next % len {
        0 => None,
        i => Some(list[i - 1].clone()),
    }
}

/// Главный экран: вход, свой сервер, настройки и справка.
///
/// Он же — место, куда клиент возвращается, если ник занят: человек правит
/// одно поле и пробует снова, не перезапуская программу.
#[derive(Debug, Clone)]
pub struct Login {
    pub tab: HomeTab,
    pub nickname: Input,
    pub room: Input,
    pub server: Input,
    /// Порт для своего сервера. Отдельным полем, а не аргументом запуска:
    /// занятый порт виден сразу, и поменять его можно не выходя из клиента.
    pub port: Input,
    pub field: Field,
    pub error: Option<String>,
    /// Комнаты, живущие сейчас на сервере. Человек выбирает из списка
    /// стрелками и заходит, ни у кого не спрашивая адрес и не пересылая коды.
    pub rooms: Vec<RoomSummary>,
    /// Какая строка списка выбрана. `None` — фокус на полях формы, ввод и
    /// Enter работают по полю «комната».
    pub rooms_selected: Option<usize>,
    /// Что показать над списком: «спрашиваю сервер…» или причина, по которой
    /// список пуст. Пустой список без пояснения выглядит как поломка.
    pub rooms_note: Option<String>,
    /// Выбранная строка на вкладке «вид».
    pub setting: usize,
    /// Чем экран сейчас занят: поднимает сервер, ждёт ответа. Пока не пусто,
    /// Enter ничего не делает — иначе нетерпеливое нажатие подняло бы второй
    /// сервер на том же порту.
    pub busy: Option<String>,
    /// Когда экран открылся: по метке проигрывается появление заголовка.
    pub opened: Instant,
    /// Когда последний раз переключили вкладку: содержимое въезжает.
    pub switched: Instant,
}

impl Default for Login {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            tab: HomeTab::default(),
            nickname: Input::default(),
            room: Input::default(),
            server: Input::default(),
            port: Input::new(DEFAULT_PORT.to_string()),
            field: Field::default(),
            error: None,
            rooms: Vec::new(),
            rooms_selected: None,
            rooms_note: None,
            setting: 0,
            busy: None,
            opened: now,
            switched: now,
        }
    }
}

impl Login {
    fn active(&mut self) -> &mut Input {
        match self.field {
            Field::Nickname => &mut self.nickname,
            Field::Room => &mut self.room,
            Field::Server => &mut self.server,
            Field::Port => &mut self.port,
        }
    }

    fn limit(&self) -> usize {
        match self.field {
            Field::Nickname => validate::MAX_NICKNAME_CHARS,
            Field::Room => validate::MAX_ROOM_CHARS,
            Field::Server => validate::MAX_TEXT_CHARS,
            // Пять цифр — весь диапазон портов. Больше просто не бывает.
            Field::Port => 5,
        }
    }

    /// Переключает вкладку и заодно чинит фокус: поле с прошлой вкладки на
    /// новой может и не существовать.
    fn switch_tab(&mut self, back: bool) {
        self.tab = self.tab.shift(back);
        self.switched = Instant::now();
        self.error = None;
        self.rooms_selected = None;
        // У каждого раздела свой список строк: остаться на четвёртой,
        // перейдя туда, где их две, значит выбрать не то.
        self.setting = 0;
        if let Some(first) = self.tab.fields().first()
            && !self.tab.fields().contains(&self.field)
        {
            self.field = *first;
        }
    }

    fn next_field(&mut self, back: bool) {
        let fields = self.tab.fields();
        if fields.is_empty() {
            return;
        }
        let at = fields.iter().position(|&f| f == self.field).unwrap_or(0);
        let next = if back { at + fields.len() - 1 } else { at + 1 };
        self.field = fields[next % fields.len()];
    }

    /// Стрелка вниз ведёт сверху вниз по всему, что на вкладке есть: сначала
    /// поля, потом список комнат. Одна дорожка вместо двух — не нужно помнить,
    /// какая клавиша куда переводит.
    fn select_down(&mut self) {
        match self.tab {
            HomeTab::Look => {
                self.setting = (self.setting + 1).min(Setting::ALL.len() - 1);
            }
            HomeTab::Sound => {
                self.setting = (self.setting + 1).min(SoundSetting::ALL.len() - 1);
            }
            HomeTab::Join => {
                let fields = self.tab.fields();
                match self.rooms_selected {
                    Some(i) => self.rooms_selected = Some((i + 1).min(self.rooms.len() - 1)),
                    // С последнего поля спускаемся в список, если он есть.
                    None if self.field == *fields.last().unwrap() && !self.rooms.is_empty() => {
                        self.rooms_selected = Some(0);
                    }
                    None => self.next_field(false),
                }
            }
            HomeTab::Host => self.next_field(false),
            HomeTab::Help => {}
        }
    }

    /// Стрелка вверх: с первой строки списка возвращает фокус на форму.
    fn select_up(&mut self) {
        match self.tab {
            HomeTab::Look | HomeTab::Sound => self.setting = self.setting.saturating_sub(1),
            HomeTab::Join => match self.rooms_selected {
                Some(0) => self.rooms_selected = None,
                Some(i) => self.rooms_selected = Some(i - 1),
                None => self.next_field(true),
            },
            HomeTab::Host => self.next_field(true),
            HomeTab::Help => {}
        }
    }

    /// Имя комнаты, в которую человек собрался войти: выбранная в списке
    /// перевешивает то, что набрано в поле, — раз уж на неё явно указали.
    fn chosen_room(&self) -> Option<String> {
        self.rooms_selected
            .and_then(|i| self.rooms.get(i))
            .map(|room| room.name.clone())
    }

    pub fn current_setting(&self) -> Setting {
        Setting::ALL[self.setting.min(Setting::ALL.len() - 1)]
    }

    pub fn current_sound(&self) -> SoundSetting {
        SoundSetting::ALL[self.setting.min(SoundSetting::ALL.len() - 1)]
    }
}

/// Порт, который предлагается для своего сервера.
pub const DEFAULT_PORT: u16 = 8080;

/// Экран в состоянии ровно один, и живёт он столько же, сколько сам клиент:
/// прятать форму входа в Box значило бы платить разыменованием на каждой
/// отрисовке ради трёхсот байт, которых всё равно ровно один экземпляр.
#[allow(clippy::large_enum_variant)]
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
    /// Спросить у сервера список комнат для экрана входа. Строка — http-адрес
    /// сервера (`http://host:port`), с него берётся `GET /rooms`.
    FetchRooms(String),
    /// Открыть адрес системным просмотрщиком.
    Open(String),
    /// Скачать картинку, чтобы показать её в терминале.
    Fetch(Uuid, String),
    /// Отправить файл на сервер и приложить его к сообщению.
    Upload(std::path::PathBuf),
    /// Прочитать каталог для обзора файлов.
    ReadDir(std::path::PathBuf),
    /// Скачать и проиграть голосовое. Идентификатор нужен, чтобы положить
    /// разобранную форму волны рядом с нужным сообщением.
    PlayVoice(Uuid, String),
    /// Остановить проигрывание.
    StopVoice,
    /// Начать запись с микрофона или закончить её и отправить.
    ToggleRecording,
    /// Скачать вложение и положить его на диск.
    Save {
        url: String,
        destination: std::path::PathBuf,
    },
    /// Разорвать соединение и никуда не подключаться: так уходят в меню.
    Disconnect,
    /// Переоткрыть звуковые устройства по выбору из настроек.
    Audio,
    /// Звоночек терминала: единственное уведомление, доступное из TUI.
    Bell,
    /// Записать настройки на диск: ник, комнату и цвета.
    SaveConfig,
    Quit,
}

/// Зачем сейчас ходят по ленте.
///
/// Механизм выбора один и тот же — стрелки и подсветка, — но шагает он по
/// разным записям: отвечать можно на любую реплику, а проигрывать и сохранять
/// имеет смысл только то, к чему приложен файл. Иначе до старого голосового
/// пришлось бы щёлкать через весь разговор.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickMode {
    /// Ответ: шагаем по всем репликам.
    Reply,
    /// Вложение: шагаем только по тем, где есть файл.
    Attachment,
}

/// Запись, подсвеченная в ленте, и то, зачем её выбирают.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pick {
    pub index: usize,
    pub mode: PickMode,
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
    /// Экранная строка, с которой начинается лента. Нужна, чтобы понять, по
    /// какому сообщению щёлкнули: координаты мыши считаются от всего окна.
    pub top: u16,
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
    /// Играет ли сейчас голосовое. Держим в состоянии, чтобы одна и та же
    /// клавиша умела и включить, и остановить: две клавиши на это — уже
    /// инструкция, которую надо помнить.
    pub playing: bool,
    /// Разобранные формы волн по идентификатору вложения. Появляются после
    /// скачивания: до него байтов нет, а рисовать выдуманный график нельзя.
    pub waveforms: HashMap<Uuid, crate::media::Waveform>,
    /// Какое голосовое играет и когда включили: по этому рисуется бегущая
    /// заливка графика.
    pub playing_voice: Option<(Uuid, Instant)>,
    /// Умеет ли терминал настоящую графику. Полублоками миниатюра в несколько
    /// строк превращается в цветной шум, поэтому там остаётся строка с именем.
    ///
    /// Считается из двух вещей ниже и обновляется `apply_images`.
    pub inline_images: bool,
    /// Что человек выбрал в настройках. `None` — «решай сам».
    pub images_choice: Option<bool>,
    /// Что клиент решил сам, посмотрев на терминал.
    pub images_auto: bool,
    /// Тема оформления. Живёт в состоянии, а не в глобальной переменной,
    /// чтобы отрисовку можно было проверить обычным тестом.
    pub theme: Theme,
    /// Показывать ли колонку с людьми справа от переписки.
    pub sidebar: bool,
    /// Перезапускать ли клиент в нормальном терминале. Здесь — чтобы вкладка
    /// «вид» могла это менять; работает настройка при следующем запуске.
    pub terminal_mode: crate::launcher::Mode,
    /// Звуковые устройства и выбор человека.
    pub audio: Audio,
    /// Какой файл согласен принять сервер, к которому мы подключены.
    pub upload_limit: usize,
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
    /// Запись, которую сейчас выбирают в ленте, и зачем.
    pub picking: Option<Pick>,
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
    /// Открыта ли справка. Она тоже поверх: три десятка строк в ленте
    /// вытолкнули бы из виду сам разговор, ради которого её и открывали.
    pub help: bool,
    /// На сколько строк справка пролистана: в окно она целиком не влезает.
    pub help_scroll: usize,
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
                ..Login::default()
            }),
            nickname: String::new(),
            room: room.clone(),
            server: String::new(),
            media_base: String::new(),
            colors: HashMap::new(),
            last_dir: None,
            thumbnails: HashMap::new(),
            busy: None,
            playing: false,
            waveforms: HashMap::new(),
            playing_voice: None,
            inline_images: false,
            images_choice: None,
            images_auto: false,
            theme: Theme::default(),
            sidebar: false,
            terminal_mode: crate::launcher::Mode::default(),
            audio: Audio::default(),
            upload_limit: validate::MAX_UPLOAD_BYTES,
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
            help_scroll: 0,
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

    /// Пересчитывает, показывать ли картинки прямо в ленте: выбор человека
    /// перевешивает догадку клиента, но пока выбора нет — работает догадка.
    pub fn apply_images(&mut self) {
        self.inline_images = self.images_choice.unwrap_or(self.images_auto);
    }

    /// Значение настройки строкой — для экрана и для файла настроек.
    pub fn setting_value(&self, setting: Setting) -> &'static str {
        match setting {
            Setting::Theme => self.theme.title(),
            Setting::Images => match self.images_choice {
                None => "авто",
                Some(true) => "показывать",
                Some(false) => "не надо",
            },
            Setting::Sidebar => {
                if self.sidebar {
                    "показывать"
                } else {
                    "спрятана"
                }
            }
            Setting::Terminal => match self.terminal_mode {
                crate::launcher::Mode::Auto => "авто",
                crate::launcher::Mode::Always => "всегда",
                crate::launcher::Mode::Never => "никогда",
            },
        }
    }

    /// Значение звуковой настройки строкой.
    pub fn sound_value(&self, setting: SoundSetting) -> String {
        match setting {
            SoundSetting::Output => match &self.audio.output {
                Some(name) => name.clone(),
                None => "как в системе".to_string(),
            },
            SoundSetting::Input => match &self.audio.input {
                Some(name) => name.clone(),
                None => "как в системе".to_string(),
            },
            SoundSetting::Chime => if self.audio.chime {
                "включён"
            } else {
                "выключен"
            }
            .to_string(),
            SoundSetting::Volume => match self.audio.volume {
                0 => "тихо",
                2 => "громко",
                _ => "средне",
            }
            .to_string(),
        }
    }

    /// Листает звуковую настройку.
    fn shift_sound(&mut self, setting: SoundSetting, back: bool) {
        match setting {
            SoundSetting::Output => {
                self.audio.output = cycle_device(&self.audio.outputs, &self.audio.output, back);
            }
            SoundSetting::Input => {
                self.audio.input = cycle_device(&self.audio.inputs, &self.audio.input, back);
            }
            SoundSetting::Chime => self.audio.chime = !self.audio.chime,
            SoundSetting::Volume => {
                let steps = crate::sound::GAINS.len();
                let at = self.audio.volume.min(steps - 1);
                let next = if back { at + steps - 1 } else { at + 1 };
                self.audio.volume = next % steps;
            }
        }
    }

    /// Листает значение настройки. Стрелки ходят в обе стороны, Enter — та же
    /// стрелка вправо: перебирать три значения по кругу проще, чем помнить,
    /// какая клавиша что открывает.
    fn shift_setting(&mut self, setting: Setting, back: bool) {
        match setting {
            Setting::Theme => self.theme = self.theme.shift(back),
            Setting::Images => {
                // Три состояния по кругу: авто → показывать → не надо.
                self.images_choice = match (self.images_choice, back) {
                    (None, false) => Some(true),
                    (Some(true), false) => Some(false),
                    (Some(false), false) => None,
                    (None, true) => Some(false),
                    (Some(false), true) => Some(true),
                    (Some(true), true) => None,
                };
                self.apply_images();
            }
            Setting::Sidebar => self.sidebar = !self.sidebar,
            Setting::Terminal => {
                use crate::launcher::Mode;
                self.terminal_mode = match (self.terminal_mode, back) {
                    (Mode::Auto, false) => Mode::Always,
                    (Mode::Always, false) => Mode::Never,
                    (Mode::Never, false) => Mode::Auto,
                    (Mode::Auto, true) => Mode::Never,
                    (Mode::Never, true) => Mode::Always,
                    (Mode::Always, true) => Mode::Auto,
                };
            }
        }
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
        Action::Click(_, row) => {
            // Щелчок по вложению делает с ним то же, что Enter в выборе:
            // голосовое играет, картинку открывает, файл кладёт на диск.
            // По обычной реплике не делаем ничего — случайный клик не должен
            // ничего менять.
            match entry_at_row(state, row) {
                Some(index) if picked_attachment(state, index).is_some() => {
                    act_on_picked(state, index)
                }
                _ => Vec::new(),
            }
        }
        Action::Scroll(delta) => {
            scroll_by(state, delta);
            Vec::new()
        }
        Action::Notice(text) => {
            state.busy = None;
            // На главном экране беда относится к форме: показываем её там же,
            // где человек набирал, — в ленту он ещё даже не заходил.
            match &mut state.screen {
                Screen::Login(login) => {
                    login.busy = None;
                    login.error = Some(text);
                }
                Screen::Chat => state.system(SystemKind::Error, text),
            }
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
            // Сервер встал: теперь переход в переписку осмыслен.
            if matches!(state.screen, Screen::Login(_)) {
                state.screen = Screen::Chat;
            }
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
        Action::Waveform(id, wave) => {
            state.waveforms.insert(id, wave);
            // Заливка графика отсчитывается от момента включения.
            state.playing_voice = Some((id, Instant::now()));
            Vec::new()
        }
        Action::Rooms(result) => {
            match &mut state.screen {
                // На экране входа список ложится в браузер комнат под формой.
                Screen::Login(login) => match result {
                    Ok(rooms) => {
                        login.rooms_note = rooms
                            .is_empty()
                            .then(|| "комнат пока нет — впишите имя, и она заведётся".to_string());
                        // Выбор мог указывать за пределы обновлённого списка.
                        login.rooms_selected = login.rooms_selected.filter(|&i| i < rooms.len());
                        login.rooms = rooms;
                    }
                    Err(reason) => {
                        login.rooms.clear();
                        login.rooms_selected = None;
                        login.rooms_note = Some(reason);
                    }
                },
                // В чате список печатается строками: он пришёл по команде
                // /rooms, и человеку остаётся выбрать /join.
                Screen::Chat => {
                    state.busy = None;
                    match result {
                        Ok(rooms) if rooms.is_empty() => {
                            state.system(SystemKind::Info, "других комнат сейчас нет");
                        }
                        Ok(rooms) => {
                            state.system(SystemKind::Info, "комнаты на сервере:");
                            for room in rooms {
                                let people = if room.users == 1 {
                                    "1 чел.".to_string()
                                } else {
                                    format!("{} чел.", room.users)
                                };
                                let here = if room.name == state.room {
                                    " · вы здесь"
                                } else {
                                    ""
                                };
                                state.system(
                                    SystemKind::Info,
                                    format!("  /join {} — {people}{here}", room.name),
                                );
                            }
                        }
                        Err(reason) => state.system(SystemKind::Error, reason),
                    }
                }
            }
            Vec::new()
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
        Action::Voice(..) => Vec::new(),
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
    // Выход — только явным сочетанием. Esc за годы стал клавишей «назад», и
    // закрывать по ней программу означает терять разговор из-за промаха.
    if ctrl && matches!(key.code, KeyCode::Char('c' | 'd' | 'q')) {
        state.should_quit = true;
        return vec![Command::Quit];
    }

    let Screen::Login(login) = &mut state.screen else {
        return Vec::new();
    };

    // Esc на главном экране возвращает к первой вкладке — той, ради которой
    // клиент и открывают.
    if key.code == KeyCode::Esc {
        if login.tab != HomeTab::Join {
            login.tab = HomeTab::Join;
            login.switched = Instant::now();
            login.field = Field::Nickname;
            login.error = None;
        }
        return Vec::new();
    }

    let tab = login.tab;
    match key.code {
        KeyCode::Enter => return home_submit(state),
        // Tab листает вкладки, стрелки ходят внутри вкладки. Так у клавиш нет
        // двойного смысла: Tab всегда переключает раздел, ↑↓ всегда выбирают
        // строку, ←→ всегда двигают курсор в поле.
        KeyCode::Tab => login.switch_tab(false),
        KeyCode::BackTab => login.switch_tab(true),
        KeyCode::Down => login.select_down(),
        KeyCode::Up => login.select_up(),
        // На вкладке «вид» стрелки вбок листают значение настройки, на
        // остальных — двигают курсор по тексту.
        KeyCode::Left | KeyCode::Right if tab == HomeTab::Look => {
            let setting = login.current_setting();
            state.shift_setting(setting, key.code == KeyCode::Left);
            return vec![Command::SaveConfig];
        }
        KeyCode::Left | KeyCode::Right if tab == HomeTab::Sound => {
            let setting = login.current_sound();
            state.shift_sound(setting, key.code == KeyCode::Left);
            // Устройство переключаем сразу и подаём сигнал: услышать, куда
            // ушёл звук, — единственный способ проверить выбор.
            return vec![Command::Audio, Command::SaveConfig];
        }
        // Ctrl+R — обновить список комнат с адреса, что сейчас в поле.
        KeyCode::Char('r') if ctrl => {
            login.rooms_note = Some("спрашиваю сервер о комнатах…".to_string());
            let field = login.server.text.clone();
            return rooms_fetch(&field, &state.server)
                .map(|command| vec![command])
                .unwrap_or_default();
        }
        _ if tab.fields().is_empty() => {}
        _ => {
            // Правка любого поля означает «войду по набранному»: снимаем выбор
            // из списка, иначе Enter увёл бы не туда, куда смотрит человек.
            login.rooms_selected = None;
            let limit = login.limit();
            // В порт пускаем только цифры: буква в нём всё равно означала бы
            // ошибку, а поймать её при вводе честнее, чем при попытке поднять.
            // Сочетания с ctrl проверку не проходят — они не набирают текст,
            // а правят его целиком, и ctrl+u должен чистить поле как везде.
            if login.field == Field::Port
                && !ctrl
                && matches!(key.code, KeyCode::Char(ch) if !ch.is_ascii_digit())
            {
                return Vec::new();
            }
            edit_key(login.active(), key, limit);
        }
    }
    Vec::new()
}

/// Возвращает на главный экран, разорвав соединение.
///
/// Именно разорвав: «главное меню» — это место, где выбирают, куда идти, и
/// висеть в комнате, стоя в меню, значит показывать другим, что человек тут,
/// когда его тут нет. Переписка при этом остаётся в памяти — вернувшись в ту
/// же комнату, он увидит, на чём остановились.
fn to_home(state: &mut State) -> Vec<Command> {
    state.screen = Screen::Login(Login {
        nickname: Input::new(state.nickname.clone()),
        room: Input::new(state.room.clone()),
        server: Input::new(state.server.clone()),
        ..Login::default()
    });
    state.status = Status::Connecting { attempt: 0 };
    state.users.clear();
    state.typing.clear();
    state.picking = None;
    state.replying = None;
    state.search = None;
    state.help = false;
    state.viewer = None;
    state.browser = None;
    state.busy = None;

    let mut commands = vec![Command::Disconnect, Command::SaveConfig];
    // Заодно освежаем список комнат: адрес известен, а видеть, куда можно
    // зайти, полезно сразу.
    commands.extend(rooms_fetch(&state.server, &state.server));
    commands
}

/// Enter на главном экране: делает то, что написано на вкладке.
fn home_submit(state: &mut State) -> Vec<Command> {
    let Screen::Login(login) = &state.screen else {
        return Vec::new();
    };
    if login.busy.is_some() {
        return Vec::new();
    }
    match login.tab {
        HomeTab::Join => login_submit(state),
        HomeTab::Host => host_submit(state),
        HomeTab::Look => {
            let setting = login.current_setting();
            state.shift_setting(setting, false);
            vec![Command::SaveConfig]
        }
        HomeTab::Sound => {
            let setting = login.current_sound();
            state.shift_sound(setting, false);
            vec![Command::Audio, Command::SaveConfig]
        }
        HomeTab::Help => Vec::new(),
    }
}

/// Поднимает сервер прямо здесь и входит в него же.
///
/// Ник и комната проверяются теми же правилами, что и при обычном входе:
/// поднять комнату и упереться в «ник занят» — худший способ узнать, что ник
/// не годится.
fn host_submit(state: &mut State) -> Vec<Command> {
    let Screen::Login(login) = &mut state.screen else {
        return Vec::new();
    };

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
    let port = match login.port.text.trim().parse::<u16>() {
        Ok(port) => port,
        Err(_) => {
            login.field = Field::Port;
            login.error = Some("порт — число от 1 до 65535".to_string());
            return Vec::new();
        }
    };

    state.nickname = nickname;
    state.room = room;
    state.forget_room();
    // Экран не меняем: занятый порт — обычное дело, и узнать об этом человек
    // должен там же, где его набрал, а не в пустой переписке без связи.
    if let Screen::Login(login) = &mut state.screen {
        login.error = None;
        login.busy = Some(format!("поднимаю сервер на порту {port}"));
    }
    // Подключение придёт следом: сервер сам скажет свой адрес, когда встанет.
    vec![Command::Host(port)]
}

/// Команда «спросить список комнат» для адреса, что сейчас на экране входа.
///
/// Адрес берём по тем же правилам, что и при входе: набранное в поле, иначе
/// уже известный сервер, иначе свой на этой же машине. `None` — если адрес
/// не разобрать: список тогда просто не обновляется, вход это не ломает.
fn rooms_fetch(field: &str, current_server: &str) -> Option<Command> {
    let typed = field.trim();
    let raw = if !typed.is_empty() {
        typed.to_string()
    } else if !current_server.is_empty() {
        current_server.to_string()
    } else {
        crate::net::DEFAULT_SERVER.to_string()
    };
    let ws = crate::net::normalize_server(&raw).ok()?;
    Some(Command::FetchRooms(crate::net::media_base(&ws)))
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
    // Комната, выбранная в списке, перевешивает поле: раз человек подсветил
    // строку и нажал Enter, он метил именно в неё.
    let room_raw = login
        .chosen_room()
        .unwrap_or_else(|| login.room.text.clone());
    let room = match validate::clean_room(&room_raw) {
        Ok(room) => room,
        Err(err) => {
            login.field = Field::Room;
            login.rooms_selected = None;
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

/// Похоже ли на путь к файлу, а не на обычную реплику.
///
/// Проверка нарочно грубая: настоящий ответ даёт файловая система, а это
/// лишь отсев, чтобы не дёргать диск на каждое сообщение.
fn looks_like_path(value: &str) -> bool {
    // Пробелы в пути бывают, а вот перевод строки — уже точно не путь.
    if value.contains('\n') {
        return false;
    }
    // Windows: «C:\…». Остальные: «/…» или «~/…».
    let windows = value.len() > 3
        && value.as_bytes()[1] == b':'
        && matches!(value.as_bytes()[2], b'\\' | b'/')
        && value.as_bytes()[0].is_ascii_alphabetic();
    windows || value.starts_with('/') || value.starts_with("~/") || value.starts_with(r"\\")
}

/// Какому сообщению принадлежит экранная строка.
///
/// Мышь знает только координаты окна, а лента к моменту щелчка уже разложена
/// по строкам и прокручена, поэтому пересчёт возможен только здесь.
fn entry_at_row(state: &State, row: u16) -> Option<usize> {
    let top = state.viewport.top;
    let height = state.viewport.height;
    if row < top || height == 0 {
        return None;
    }
    let offset = (row - top) as usize;
    if offset >= height {
        return None;
    }

    // Лента прижата к низу: видно последние `height` строк за вычетом того,
    // на сколько прокрутили вверх.
    let end = state.viewport.total_lines.saturating_sub(state.scrollback);
    let start = end.saturating_sub(height);
    let line = start + offset;
    if line >= end {
        return None;
    }

    // Запись, чья первая строка ближе всего сверху.
    state
        .entry_lines
        .iter()
        .rposition(|first| *first <= line)
        .filter(|index| *index < state.entries.len())
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
        KeyCode::Char('c' | 'd' | 'q') if ctrl => {
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
/// Годится ли запись для выбора в этом режиме.
fn suits(entry: &Entry, mode: PickMode) -> bool {
    match (entry, mode) {
        (Entry::Chat { .. }, PickMode::Reply) => true,
        (Entry::Chat { attachment, .. }, PickMode::Attachment) => attachment.is_some(),
        (Entry::System { .. }, _) => false,
    }
}

fn neighbour_chat(state: &State, from: usize, direction: i32, mode: PickMode) -> Option<usize> {
    let mut index = from as i64;
    loop {
        index += i64::from(direction);
        if index < 0 || index as usize >= state.entries.len() {
            return None;
        }
        if suits(&state.entries[index as usize], mode) {
            return Some(index as usize);
        }
    }
}

fn last_chat(state: &State, mode: PickMode) -> Option<usize> {
    state.entries.iter().rposition(|entry| suits(entry, mode))
}

/// Начинает выбор сообщения для ответа или подтверждает выбранное.
fn toggle_picking(state: &mut State) {
    match state.picking.take() {
        // Повторное нажатие подтверждает выбранное — но только если ходили
        // за ответом: у вложений своё действие, и молча отвечать на них
        // вместо проигрывания было бы неожиданно.
        Some(pick) if pick.mode == PickMode::Reply => confirm_reply(state, pick.index),
        Some(_) => start_picking(state, PickMode::Reply),
        None => start_picking(state, PickMode::Reply),
    }
}

/// Начинает ходить по ленте: подсвечивает последнюю подходящую запись.
fn start_picking(state: &mut State, mode: PickMode) {
    let Some(index) = last_chat(state, mode) else {
        let reason = match mode {
            PickMode::Reply => "отвечать пока не на что",
            PickMode::Attachment => "в этой комнате пока нет вложений",
        };
        state.picking = None;
        state.system(SystemKind::Error, reason);
        return;
    };
    state.picking = Some(Pick { index, mode });
    reveal_entry(state, Some(index));
}

/// Вложение выбранной записи.
fn picked_attachment(state: &State, index: usize) -> Option<Attachment> {
    match state.entries.get(index) {
        Some(Entry::Chat { attachment, .. }) => attachment.clone(),
        _ => None,
    }
}

/// Делает с выбранным вложением то, чего от него и ждут: картинку показывает,
/// голосовое проигрывает, остальное кладёт на диск.
fn act_on_picked(state: &mut State, index: usize) -> Vec<Command> {
    let Some(attachment) = picked_attachment(state, index) else {
        return Vec::new();
    };
    match attachment.kind {
        AttachmentKind::Image => view_attachment(state, attachment),
        AttachmentKind::Audio => play_attachment(state, attachment),
        AttachmentKind::File => save_attachment(state, attachment, ""),
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
        // Стрелки листают: список длиннее любого разумного окна. Всё
        // остальное закрывает — искать нужную клавишу, чтобы убрать
        // подсказку, отдельное издевательство.
        match key.code {
            KeyCode::Down => state.help_scroll = (state.help_scroll + 1).min(HELP.len()),
            KeyCode::Up => state.help_scroll = state.help_scroll.saturating_sub(1),
            KeyCode::PageDown => {
                state.help_scroll = (state.help_scroll + SCROLL_STEP).min(HELP.len());
            }
            KeyCode::PageUp => state.help_scroll = state.help_scroll.saturating_sub(SCROLL_STEP),
            _ => state.help = false,
        }
        return Vec::new();
    }

    if state.search.is_some() {
        return on_search_key(state, key);
    }

    // Выбор сообщения для ответа перехватывает стрелки и Enter: пока он открыт,
    // они значат «другое сообщение» и «это оно».
    if let Some(Pick { index, mode }) = state.picking {
        // Подтверждение делает то, зачем выбор и открывали: ответ — цитирует,
        // вложение — показывает, проигрывает или кладёт на диск.
        let confirm = |state: &mut State| -> Vec<Command> {
            state.picking = None;
            match mode {
                PickMode::Reply => {
                    confirm_reply(state, index);
                    Vec::new()
                }
                PickMode::Attachment => act_on_picked(state, index),
            }
        };

        match key.code {
            KeyCode::Esc => state.picking = None,
            KeyCode::Char('c' | 'd' | 'q') if ctrl => {
                state.should_quit = true;
                return vec![Command::Quit];
            }
            KeyCode::Enter => return confirm(state),
            // Повторный Ctrl+R подтверждает выбор — так же, как Enter.
            KeyCode::Char('r') if ctrl => return confirm(state),
            // Пока ходим по вложениям, действие можно назвать и явно:
            // картинку иногда нужно не посмотреть, а сохранить.
            KeyCode::F(3) if mode == PickMode::Attachment => {
                state.picking = None;
                if let Some(attachment) = picked_attachment(state, index) {
                    return play_attachment(state, attachment);
                }
            }
            KeyCode::F(5) if mode == PickMode::Attachment => {
                state.picking = None;
                if let Some(attachment) = picked_attachment(state, index) {
                    return save_attachment(state, attachment, "");
                }
            }
            KeyCode::F(6) if mode == PickMode::Attachment => {
                state.picking = None;
                if let Some(attachment) = picked_attachment(state, index) {
                    return open_attachment(state, attachment);
                }
            }
            KeyCode::Up => {
                if let Some(next) = neighbour_chat(state, index, -1, mode) {
                    state.picking = Some(Pick { index: next, mode });
                    reveal_entry(state, Some(next));
                }
            }
            KeyCode::Down => {
                if let Some(next) = neighbour_chat(state, index, 1, mode) {
                    state.picking = Some(Pick { index: next, mode });
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
        // Колонка с людьми: в узком окне она отнимает у переписки пятую часть
        // ширины, в широком — просто занимает пустое место справа. Кому как
        // удобнее, тот так и оставит.
        KeyCode::Char('p') if ctrl => {
            state.sidebar = !state.sidebar;
            return vec![Command::SaveConfig];
        }

        // Функциональный ряд — для тех, кто не собирается учить команды.
        // Всё, ради чего люди открывают чат, должно делаться одной подписанной
        // клавишей, а не строкой, начинающейся со слэша.
        KeyCode::F(1) => {
            state.help = true;
            state.help_scroll = 0;
            return Vec::new();
        }
        // Одна клавиша на запись и отправку: во время записи всё равно ничем
        // другим не занят, а помнить две — лишнее.
        KeyCode::F(2) => return vec![Command::ToggleRecording],
        // Играет — остановить, не играет — включить последнее голосовое.
        KeyCode::F(3) => {
            return if state.playing {
                vec![Command::StopVoice]
            } else {
                play_command(state)
            };
        }
        KeyCode::F(4) => return send_command(state, ""),
        // Ходьба по вложениям: стрелки прыгают только по ним, поэтому до
        // старого голосового или фотографии не приходится щёлкать через
        // весь разговор.
        KeyCode::F(7) => {
            start_picking(state, PickMode::Attachment);
            return Vec::new();
        }
        KeyCode::F(5) => return save_command(state, ""),
        KeyCode::F(6) => return open_command(state),
        // Пока ответ взведён, Esc снимает его, а не уводит из комнаты.
        KeyCode::Esc if state.replying.is_some() => {
            state.replying = None;
            return Vec::new();
        }
        // Esc — «назад»: из комнаты в меню, а не из программы наружу.
        KeyCode::Esc => return to_home(state),
        KeyCode::Char('c' | 'd' | 'q') if ctrl => {
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

    // Перетащенный в окно файл вставляется как путь, и человек жмёт Enter,
    // ожидая, что файл уйдёт. Отправлять вместо него строку с путём —
    // бесполезно: на том конце её открыть нечем. Поэтому строка, которая
    // целиком является путём к существующему файлу, отправляется файлом.
    let dropped = line.trim().trim_matches(['"', '\'']).trim();
    if !dropped.is_empty() && looks_like_path(dropped) {
        let path = std::path::PathBuf::from(dropped);
        if path.is_file() {
            state.remember_sent(&line);
            state.input.clear();
            return send_command(state, dropped);
        }
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
            state.help_scroll = 0;
            Vec::new()
        }
        "quit" | "exit" => {
            state.should_quit = true;
            vec![Command::Quit]
        }
        "menu" => to_home(state),
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
        "rooms" => rooms_command(state),
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
    match last_attachment(state) {
        Some(attachment) => view_attachment(state, attachment),
        None => {
            state.system(SystemKind::Error, "в этой комнате пока нет вложений");
            Vec::new()
        }
    }
}

/// Показывает конкретную картинку — последнюю или выбранную в ленте.
fn view_attachment(state: &mut State, attachment: Attachment) -> Vec<Command> {
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
    match last_attachment(state) {
        Some(attachment) => open_attachment(state, attachment),
        None => {
            state.system(SystemKind::Error, "в этой комнате пока нет вложений");
            Vec::new()
        }
    }
}

fn open_attachment(state: &mut State, attachment: Attachment) -> Vec<Command> {
    let Some(url) = attachment_url(state, &attachment) else {
        state.system(SystemKind::Error, "неизвестен адрес сервера");
        return Vec::new();
    };

    state.system(SystemKind::Info, format!("открываю {}", attachment.name));
    vec![Command::Open(url)]
}

/// Проигрывает последнее голосовое.
fn play_command(state: &mut State) -> Vec<Command> {
    match last_attachment(state) {
        Some(attachment) => play_attachment(state, attachment),
        None => {
            state.system(SystemKind::Error, "в этой комнате пока нет вложений");
            Vec::new()
        }
    }
}

/// Проигрывает конкретное голосовое — последнее или выбранное в ленте.
fn play_attachment(state: &mut State, attachment: Attachment) -> Vec<Command> {
    if attachment.kind != AttachmentKind::Audio {
        state.system(SystemKind::Error, "это вложение — не голосовое");
        return Vec::new();
    }
    let Some(url) = attachment_url(state, &attachment) else {
        state.system(SystemKind::Error, "неизвестен адрес сервера");
        return Vec::new();
    };

    state.busy = Some(format!("качаю {}", attachment.name));
    vec![Command::PlayVoice(attachment.id, url)]
}

/// Сохраняет последнее вложение на диск.
fn save_command(state: &mut State, arg: &str) -> Vec<Command> {
    match last_attachment(state) {
        Some(attachment) => save_attachment(state, attachment, arg),
        None => {
            state.system(SystemKind::Error, "в этой комнате пока нет вложений");
            Vec::new()
        }
    }
}

/// Кладёт на диск конкретное вложение — последнее или выбранное в ленте.
fn save_attachment(state: &mut State, attachment: Attachment, arg: &str) -> Vec<Command> {
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

/// Показывает, какие комнаты сейчас живут на сервере, чтобы перейти в них
/// командой /join, не выходя из чата.
fn rooms_command(state: &mut State) -> Vec<Command> {
    if state.media_base.is_empty() {
        state.system(SystemKind::Error, "неизвестен адрес сервера");
        return Vec::new();
    }
    state.busy = Some("спрашиваю комнаты".to_string());
    vec![Command::FetchRooms(state.media_base.clone())]
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
                error: Some(reason),
                rooms_note: Some("спрашиваю сервер о комнатах…".to_string()),
                ..Login::default()
            });
            state.users.clear();
            // Вернулись на экран входа — заодно освежаем список комнат: адрес
            // и так известен, а видеть, куда можно зайти, полезно сразу.
            return rooms_fetch(&state.server, &state.server)
                .map(|command| vec![command])
                .unwrap_or_default();
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
            upload_limit,
        } => {
            // Потолок задаёт сервер: у чужого он может быть и меньше нашего,
            // и больше, а гадать по своей константе — врать человеку.
            state.upload_limit = upload_limit as usize;
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
                if state.audio.chime {
                    commands.push(Command::Bell);
                }
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
            if state.push_chat(message) && state.audio.chime {
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
                upload_limit: validate::MAX_UPLOAD_BYTES as u64,
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
    fn arrows_walk_the_login_fields() {
        // Tab переключает вкладки, поэтому по полям ходят стрелки: у клавиши
        // не должно быть двух смыслов на одном экране.
        let (mut state, _) = State::new(None, "general".into());
        typed(&mut state, "alice");
        update(&mut state, key(KeyCode::Down));
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
                upload_limit: validate::MAX_UPLOAD_BYTES as u64,
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
                upload_limit: validate::MAX_UPLOAD_BYTES as u64,
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

    fn some_rooms() -> Vec<RoomSummary> {
        vec![
            RoomSummary {
                name: "rust".into(),
                users: 2,
            },
            RoomSummary {
                name: "talk".into(),
                users: 1,
            },
        ]
    }

    #[test]
    fn login_shows_the_rooms_it_is_given() {
        let (mut state, _) = State::new(None, "general".into());

        update(&mut state, Action::Rooms(Ok(some_rooms())));

        let Screen::Login(login) = &state.screen else {
            panic!("не экран входа");
        };
        assert_eq!(login.rooms.len(), 2);
        assert!(login.rooms_note.is_none());
    }

    #[test]
    fn picking_a_room_from_the_list_joins_it() {
        let (mut state, _) = State::new(None, "general".into());
        state.prefill_nickname("alice");
        update(&mut state, Action::Rooms(Ok(some_rooms())));

        // Стрелки идут по форме сверху вниз и с последнего поля спускаются
        // в список: три шага по полям, четвёртый — первая комната, пятый —
        // вторая.
        for _ in 0..5 {
            update(&mut state, key(KeyCode::Down));
        }
        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(
            matches!(
                commands.as_slice(),
                [Command::Connect { room, nickname, .. }]
                    if room == "talk" && nickname == "alice"
            ),
            "{commands:?}"
        );
        assert!(matches!(state.screen, Screen::Chat));
    }

    #[test]
    fn typing_a_room_beats_a_stale_selection() {
        let (mut state, _) = State::new(None, "general".into());
        state.prefill_nickname("alice");
        update(&mut state, Action::Rooms(Ok(some_rooms())));
        for _ in 0..4 {
            update(&mut state, key(KeyCode::Down)); // выбрали «rust»
        }

        // Возвращаемся в поле «комната» и дописываем — выбор снимается.
        for _ in 0..3 {
            update(&mut state, key(KeyCode::Up));
        }
        typed(&mut state, "x");
        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(
            matches!(
                commands.as_slice(),
                [Command::Connect { room, .. }] if room == "generalx"
            ),
            "{commands:?}"
        );
    }

    #[test]
    fn the_host_tab_raises_a_server_without_leaving_the_screen() {
        let (mut state, _) = State::new(None, "general".into());
        state.prefill_nickname("alice");
        update(&mut state, key(KeyCode::Tab)); // войти -> поднять

        let commands = update(&mut state, key(KeyCode::Enter));

        assert_eq!(commands, [Command::Host(DEFAULT_PORT)]);
        // Занятый порт — обычное дело: пока сервер не встал, человек остаётся
        // там, где набирал, а не оказывается в переписке без связи.
        let Screen::Login(login) = &state.screen else {
            panic!("ушли с главного экрана раньше времени");
        };
        assert!(login.busy.is_some());
    }

    #[test]
    fn a_busy_port_is_reported_on_the_form() {
        let (mut state, _) = State::new(None, "general".into());
        state.prefill_nickname("alice");
        update(&mut state, key(KeyCode::Tab));
        update(&mut state, key(KeyCode::Enter));

        update(&mut state, Action::Notice("порт занят".into()));

        let Screen::Login(login) = &state.screen else {
            panic!("не главный экран");
        };
        assert_eq!(login.error.as_deref(), Some("порт занят"));
        assert!(login.busy.is_none(), "спиннер должен погаснуть");
        // И повторный Enter снова пробует: экран не заперт.
        assert_eq!(
            update(&mut state, key(KeyCode::Enter)),
            [Command::Host(DEFAULT_PORT)]
        );
    }

    #[test]
    fn a_raised_server_takes_you_into_the_chat() {
        let (mut state, _) = State::new(None, "general".into());
        state.prefill_nickname("alice");
        update(&mut state, key(KeyCode::Tab));
        update(&mut state, key(KeyCode::Enter));

        let commands = update(
            &mut state,
            Action::Hosted {
                url: "ws://127.0.0.1:8080/ws".into(),
                lines: vec!["друг подключается: 192.168.1.5:8080".into()],
            },
        );

        assert!(matches!(state.screen, Screen::Chat));
        assert!(matches!(
            commands.as_slice(),
            [Command::Connect { nickname, room, .. }] if nickname == "alice" && room == "general"
        ));
    }

    #[test]
    fn a_bad_port_never_reaches_the_network() {
        let (mut state, _) = State::new(None, "general".into());
        state.prefill_nickname("alice");
        update(&mut state, key(KeyCode::Tab));
        // Стираем порт: пустое поле — не число.
        update(&mut state, key(KeyCode::Down));
        update(&mut state, key(KeyCode::Down));
        update(&mut state, ctrl('u'));

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(commands.is_empty());
        let Screen::Login(login) = &state.screen else {
            panic!("не главный экран");
        };
        assert_eq!(login.field, Field::Port);
        assert!(login.error.is_some());
    }

    #[test]
    fn the_port_field_takes_digits_only() {
        let (mut state, _) = State::new(None, "general".into());
        update(&mut state, key(KeyCode::Tab));
        update(&mut state, key(KeyCode::Down));
        update(&mut state, key(KeyCode::Down));
        update(&mut state, ctrl('u'));
        typed(&mut state, "80п8");

        let Screen::Login(login) = &state.screen else {
            panic!("не главный экран");
        };
        assert_eq!(login.port.text, "808");
    }

    #[test]
    fn the_look_tab_changes_the_theme_and_remembers_it() {
        let (mut state, _) = State::new(None, "general".into());
        for _ in 0..2 {
            update(&mut state, key(KeyCode::Tab)); // войти -> поднять -> вид
        }

        let commands = update(&mut state, key(KeyCode::Right));

        assert_eq!(state.theme, crate::theme::Theme::default().shift(false));
        // Выбор переживает перезапуск — значит, его надо записать на диск.
        assert_eq!(commands, [Command::SaveConfig]);
    }

    #[test]
    fn the_look_tab_walks_its_rows() {
        let (mut state, _) = State::new(None, "general".into());
        for _ in 0..2 {
            update(&mut state, key(KeyCode::Tab));
        }
        update(&mut state, key(KeyCode::Down));
        update(&mut state, key(KeyCode::Enter));

        // Вторая строка — картинки в ленте: Enter листает её так же, как ←→.
        assert_eq!(state.images_choice, Some(true));
    }

    #[test]
    fn arrow_up_from_the_top_returns_focus_to_the_form() {
        let (mut state, _) = State::new(None, "general".into());
        update(&mut state, Action::Rooms(Ok(some_rooms())));

        for _ in 0..3 {
            update(&mut state, key(KeyCode::Down)); // по полям и в список
        }
        update(&mut state, key(KeyCode::Up)); // с нулевой обратно на форму

        let Screen::Login(login) = &state.screen else {
            panic!("не экран входа");
        };
        assert_eq!(login.rooms_selected, None);
    }

    #[test]
    fn ctrl_r_on_login_asks_the_server_again() {
        let (mut state, _) = State::new(None, "general".into());
        state.set_server("ws://192.168.1.5:8080/ws".into());

        let commands = update(&mut state, ctrl('r'));

        assert!(
            matches!(
                commands.as_slice(),
                [Command::FetchRooms(base)] if base == "http://192.168.1.5:8080"
            ),
            "{commands:?}"
        );
    }

    #[test]
    fn a_failed_room_query_explains_itself() {
        let (mut state, _) = State::new(None, "general".into());
        update(&mut state, Action::Rooms(Ok(some_rooms())));

        update(&mut state, Action::Rooms(Err("сервер не ответил".into())));

        let Screen::Login(login) = &state.screen else {
            panic!("не экран входа");
        };
        assert!(login.rooms.is_empty());
        assert_eq!(login.rooms_note.as_deref(), Some("сервер не ответил"));
    }

    fn function(n: u8) -> Action {
        Action::Key(KeyEvent::new(KeyCode::F(n), KeyModifiers::NONE))
    }

    #[test]
    fn function_keys_do_what_the_commands_do() {
        // Смысл клавиш ровно в том, чтобы не заставлять человека помнить
        // команды: F2 должна делать то же, что «/rec».
        let (mut state, _) = connected();

        assert_eq!(
            update(&mut state, function(2)),
            vec![Command::ToggleRecording]
        );

        // F1 открывает справку, а не пишет что-то в комнату.
        let (mut state, _) = connected();
        let before = state.entries.len();
        assert!(update(&mut state, function(1)).is_empty());
        assert!(state.help, "F1 не открыла справку");
        assert_eq!(state.entries.len(), before);
    }

    #[test]
    fn f4_opens_the_file_browser() {
        let (mut state, _) = connected();
        // Отправка требует известного адреса сервера — иначе некуда лить.
        state.media_base = "http://127.0.0.1:8080".into();

        let commands = update(&mut state, function(4));

        assert!(state.browser.is_some(), "обзор файлов не открылся");
        assert!(
            matches!(commands.as_slice(), [Command::ReadDir(_)]),
            "{commands:?}"
        );
    }

    #[test]
    fn f3_plays_and_then_stops() {
        let (mut state, _) = with_voice();

        // Ничего не играет — F3 включает.
        let commands = update(&mut state, function(3));
        assert!(
            matches!(commands.as_slice(), [Command::PlayVoice(..)]),
            "{commands:?}"
        );

        // Играет — та же клавиша останавливает: две клавиши на это уже
        // инструкция, которую надо помнить.
        state.playing = true;
        assert_eq!(update(&mut state, function(3)), vec![Command::StopVoice]);
    }

    #[test]
    fn function_keys_do_not_leak_into_the_message() {
        // Незнакомая клавиша не должна оказаться символом в строке ввода.
        let (mut state, _) = connected();
        typed(&mut state, "привет");

        update(&mut state, function(2));

        assert_eq!(state.input.text, "привет");
    }

    /// Комната с картинкой, голосовым и обычной репликой между ними.
    fn with_mixed_attachments() -> State {
        let (mut state, _) = connected();
        state.media_base = "http://127.0.0.1:8080".into();

        let picture = Attachment {
            id: Uuid::new_v4(),
            kind: AttachmentKind::Image,
            name: "кот.png".into(),
            size: 10,
            mime: "image/png".into(),
        };
        let voice = Attachment {
            id: Uuid::new_v4(),
            kind: AttachmentKind::Audio,
            name: "голосовое.wav".into(),
            size: 20,
            mime: "audio/wav".into(),
        };
        for attachment in [Some(picture), None, Some(voice), None] {
            let mut message = chat_message(user("bob"), "текст");
            message.attachment = attachment;
            update(
                &mut state,
                Action::Net(NetEvent::Message(ServerMessage::Chat(message))),
            );
        }
        state
    }

    #[test]
    fn f7_walks_only_over_messages_with_attachments() {
        // Ради этого всё и затевалось: до старого голосового не должно
        // приходиться щёлкать через весь разговор.
        let mut state = with_mixed_attachments();

        update(&mut state, function(7));
        let first = state.picking.expect("выбор не начался");
        assert_eq!(first.mode, PickMode::Attachment);
        assert!(
            picked_attachment(&state, first.index).is_some(),
            "выбрана запись без вложения"
        );

        // Шаг вверх обязан перескочить обычную реплику и попасть на картинку.
        update(&mut state, key(KeyCode::Up));
        let second = state.picking.expect("выбор потерялся");
        assert!(second.index < first.index);
        let attachment = picked_attachment(&state, second.index).expect("нет вложения");
        assert_eq!(attachment.kind, AttachmentKind::Image);
    }

    #[test]
    fn enter_does_the_natural_thing_to_the_picked_attachment() {
        let mut state = with_mixed_attachments();

        // Последнее вложение — голосовое: Enter должен его проиграть.
        update(&mut state, function(7));
        let commands = update(&mut state, key(KeyCode::Enter));
        assert!(
            matches!(commands.as_slice(), [Command::PlayVoice(..)]),
            "{commands:?}"
        );
        assert!(state.picking.is_none(), "выбор не закрылся");

        // А картинку — показать.
        update(&mut state, function(7));
        update(&mut state, key(KeyCode::Up));
        let commands = update(&mut state, key(KeyCode::Enter));
        assert!(
            matches!(commands.as_slice(), [Command::Fetch(..)]),
            "{commands:?}"
        );
        assert!(state.viewer.is_some(), "просмотр не открылся");
    }

    #[test]
    fn a_picked_picture_can_be_saved_instead_of_shown() {
        // Картинку иногда нужно не посмотреть, а положить на диск.
        let mut state = with_mixed_attachments();
        update(&mut state, function(7));
        update(&mut state, key(KeyCode::Up));

        let commands = update(&mut state, function(5));

        assert!(
            matches!(
                commands.as_slice(),
                [Command::Save { destination, .. }]
                    if destination.ends_with("кот.png")
            ),
            "{commands:?}"
        );
    }

    #[test]
    fn picking_an_attachment_does_not_start_a_reply() {
        // Режимы не должны путаться: Ctrl+R цитирует, F7 — открывает.
        let mut state = with_mixed_attachments();

        update(&mut state, function(7));
        update(&mut state, key(KeyCode::Enter));

        assert!(state.replying.is_none(), "вложение превратилось в цитату");
    }

    #[test]
    fn reply_picking_still_walks_every_message() {
        // Старое поведение не должно пострадать: отвечать можно на любую
        // реплику, не только на ту, где есть файл.
        let mut state = with_mixed_attachments();

        update(&mut state, ctrl('r'));
        let pick = state.picking.expect("выбор не начался");
        assert_eq!(pick.mode, PickMode::Reply);
        // Последняя реплика — без вложения, и она годится для ответа.
        assert!(picked_attachment(&state, pick.index).is_none());

        update(&mut state, key(KeyCode::Enter));
        assert!(state.replying.is_some(), "цитата не взведена");
    }

    #[test]
    fn a_path_is_told_apart_from_a_message() {
        // Отсев грубый, но обязан пропускать то, что вставляет проводник,
        // и не трогать обычную речь.
        assert!(looks_like_path(r"C:\Users\egord\Downloads\голосовое.wav"));
        assert!(looks_like_path("C:/Users/egord/фото.png"));
        assert!(looks_like_path("/home/egor/фото.png"));
        assert!(looks_like_path(r"\\сервер\общая\файл.zip"));

        for message in [
            "привет",
            "смотри что нашёл",
            "1:2 счёт",
            r"путь C:\тут внутри фразы",
        ] {
            assert!(!looks_like_path(message), "принято за путь: {message}");
        }
    }

    #[test]
    fn a_dragged_file_is_sent_as_a_file_not_as_text() {
        // Перетаскивание в терминал вставляет путь. Отправить его строкой —
        // бесполезно: открыть её на том конце нечем.
        let (mut state, _) = connected();
        state.media_base = "http://127.0.0.1:8080".into();

        let file = std::env::current_exe().expect("нет пути к своему же exe");
        let typed_path = file.to_string_lossy().to_string();
        typed(&mut state, &typed_path);
        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(
            matches!(commands.as_slice(), [Command::Upload(path)] if *path == file),
            "{commands:?}"
        );
        assert!(state.input.is_empty(), "ввод не очистился");
    }

    #[test]
    fn a_path_to_nothing_stays_an_ordinary_message() {
        // Несуществующий путь — просто текст: молча проглотить сообщение
        // было бы хуже, чем отправить его как есть.
        let (mut state, _) = connected();
        state.media_base = "http://127.0.0.1:8080".into();
        typed(&mut state, r"C:\такого\файла\нет.txt");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(
            matches!(
                commands.as_slice(),
                [Command::Send(ClientMessage::Chat { .. })]
            ),
            "{commands:?}"
        );
    }

    #[test]
    fn clicking_a_voice_plays_it() {
        // Щелчок по голосовому — самый очевидный жест, который только может
        // быть: график нарисован, по нему и щёлкают.
        let mut state = with_mixed_attachments();
        // Раскладка ленты известна только после отрисовки, поэтому в тесте
        // задаём её руками: по строке на запись, лента с самого верха окна.
        state.entry_lines = (0..state.entries.len()).collect();
        state.viewport = Viewport {
            height: state.entries.len(),
            total_lines: state.entries.len(),
            top: 0,
        };
        state.scrollback = 0;

        // Последняя запись — реплика без вложения, перед ней голосовое.
        let voice_row = (state.entries.len() - 2) as u16;
        let commands = update(&mut state, Action::Click(0, voice_row));

        assert!(
            matches!(commands.as_slice(), [Command::PlayVoice(..)]),
            "{commands:?}"
        );
    }

    #[test]
    fn clicking_a_plain_message_does_nothing() {
        // Случайный щелчок не должен ничего менять: в чате мышью попадают
        // мимо постоянно.
        let mut state = with_mixed_attachments();
        state.entry_lines = (0..state.entries.len()).collect();
        state.viewport = Viewport {
            height: state.entries.len(),
            total_lines: state.entries.len(),
            top: 0,
        };

        let last = (state.entries.len() - 1) as u16;
        let commands = update(&mut state, Action::Click(0, last));

        assert!(commands.is_empty(), "{commands:?}");
        assert!(state.viewer.is_none());
    }

    #[test]
    fn clicking_outside_the_feed_is_ignored() {
        let mut state = with_mixed_attachments();
        state.entry_lines = (0..state.entries.len()).collect();
        state.viewport = Viewport {
            height: 3,
            total_lines: state.entries.len(),
            top: 5,
        };

        // Выше ленты и ниже неё — мимо.
        assert!(update(&mut state, Action::Click(0, 1)).is_empty());
        assert!(update(&mut state, Action::Click(0, 99)).is_empty());
    }

    #[test]
    fn f7_without_attachments_explains_itself() {
        let (mut state, _) = connected();

        update(&mut state, function(7));

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
    fn only_a_deliberate_shortcut_closes_the_program() {
        for shortcut in ['c', 'd', 'q'] {
            let (mut state, _) = connected();

            let commands = update(&mut state, ctrl(shortcut));

            assert_eq!(commands, [Command::Quit], "ctrl+{shortcut}");
            assert!(state.should_quit, "ctrl+{shortcut}");
        }
    }

    #[test]
    fn arrows_scroll_the_help_and_anything_else_closes_it() {
        let (mut state, _) = connected();
        update(&mut state, key(KeyCode::F(1)));

        update(&mut state, key(KeyCode::Down));
        update(&mut state, key(KeyCode::Down));

        // Список длиннее любого разумного окна: стрелки должны листать его,
        // а не закрывать.
        assert!(state.help);
        assert_eq!(state.help_scroll, 2);

        update(&mut state, key(KeyCode::Up));
        assert_eq!(state.help_scroll, 1);

        update(&mut state, key(KeyCode::Char('x')));
        assert!(!state.help);

        // Открытая заново справка начинается сначала.
        update(&mut state, key(KeyCode::F(1)));
        assert_eq!(state.help_scroll, 0);
    }

    #[test]
    fn esc_leaves_the_room_for_the_menu() {
        let (mut state, _) = connected();

        let commands = update(&mut state, key(KeyCode::Esc));

        // Esc — «назад», а не «выход»: программа остаётся открытой, а
        // соединение с комнатой закрывается.
        assert!(!state.should_quit);
        assert!(matches!(state.screen, Screen::Login(_)));
        assert!(commands.contains(&Command::Disconnect), "{commands:?}");
        // Поля заполнены тем, чем человек только что пользовался.
        let Screen::Login(login) = &state.screen else {
            panic!("не главный экран");
        };
        assert_eq!(login.nickname.text, "alice");
        assert_eq!(login.room.text, "general");
    }

    #[test]
    fn the_menu_command_does_the_same() {
        let (mut state, _) = connected();
        typed(&mut state, "/menu");

        let commands = update(&mut state, key(KeyCode::Enter));

        assert!(matches!(state.screen, Screen::Login(_)));
        assert!(commands.contains(&Command::Disconnect), "{commands:?}");
    }

    #[test]
    fn esc_on_the_home_screen_returns_to_the_first_tab() {
        let (mut state, _) = State::new(None, "general".into());
        update(&mut state, key(KeyCode::Tab));
        update(&mut state, key(KeyCode::Tab));

        let commands = update(&mut state, key(KeyCode::Esc));

        // Программа не закрывается: на главном экране Esc — тоже «назад».
        assert!(commands.is_empty());
        assert!(!state.should_quit);
        let Screen::Login(login) = &state.screen else {
            panic!("не главный экран");
        };
        assert_eq!(login.tab, HomeTab::Join);
    }

    #[test]
    fn leaving_for_the_menu_keeps_the_conversation() {
        let (mut state, _) = connected();
        let before = state.entries.len();

        update(&mut state, key(KeyCode::Esc));

        // Переписка остаётся в памяти: вернувшись в ту же комнату, человек
        // увидит, на чём остановились.
        assert_eq!(state.entries.len(), before);
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
            top: 0,
        };
        state.entry_lines = (0..state.entries.len()).map(|index| index * 2).collect();
        state
    }

    #[test]
    fn ctrl_r_picks_the_last_message_and_sends_a_reply() {
        let mut state = with_history();

        update(&mut state, ctrl('r'));
        let picked = state.picking.expect("выбор не начался").index;
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
        let last = state.picking.unwrap().index;

        update(&mut state, key(KeyCode::Up));
        assert!(state.picking.unwrap().index < last, "вверх не сработало");

        update(&mut state, key(KeyCode::Down));
        assert_eq!(state.picking.map(|pick| pick.index), Some(last));
    }

    #[test]
    fn picking_skips_system_lines() {
        let mut state = with_history();
        state.system(SystemKind::Info, "кто-то вошёл");
        update(&mut state, ctrl('r'));

        // Системная строка — не сообщение, отвечать на неё нечего.
        let picked = state.picking.expect("выбор не начался").index;
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
                upload_limit: validate::MAX_UPLOAD_BYTES as u64,
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
            [Command::PlayVoice(
                id,
                format!("http://127.0.0.1:8080/media/{id}")
            )]
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
            media: !is_dir,
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
        update(&mut state, key(KeyCode::Down));
        update(&mut state, key(KeyCode::Down));
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
        update(&mut state, key(KeyCode::Down));
        update(&mut state, key(KeyCode::Down));
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
            top: 0,
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
            top: 0,
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
