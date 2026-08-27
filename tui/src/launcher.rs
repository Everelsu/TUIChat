//! Перезапуск клиента в нормальном терминале.
//!
//! На Windows двойной клик по программе открывает её в conhost — древнем
//! окне, доставшемся от cmd: там нет 24-битного цвета в старых сборках, шрифт
//! рвёт юникод, а графики нет никакой. Интерфейс, собранный из цвета и
//! полублоков, в нём выглядит сломанным, и человек решает, что сломана
//! программа.
//!
//! Поэтому клиент, запущенный двойным кликом, сам открывается заново в том
//! терминале, который на машине лучший, и закрывает исходное окно. Своего
//! эмулятора терминала мы не пишем: движок Windows Terminal или WezTerm лучше
//! всего, что можно собрать сбоку, и обновляется без нас.
//!
//! Запуск из уже открытого терминала не трогаем: раз человек его выбрал, ему
//! виднее. Исключение — режим `always` в настройках.

use std::{ffi::OsString, io::IsTerminal, path::PathBuf, process::Command};

/// Когда перезапускаться в другом терминале.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Только когда программу запустили двойным кликом: из терминала человек
    /// пришёл сам, и уводить его в другое окно — самоуправство.
    #[default]
    Auto,
    /// Всегда, если текущее окно — не современный терминал. Пригодится тем,
    /// кто запускает из cmd, но хочет видеть цвет и картинки.
    Always,
    Never,
}

impl Mode {
    /// Разбирает значение из настроек или аргумента командной строки.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "auto" | "авто" => Some(Mode::Auto),
            "always" | "всегда" => Some(Mode::Always),
            "never" | "никогда" => Some(Mode::Never),
            _ => None,
        }
    }
}

/// Переменная-предохранитель: стоит у порождённого процесса, чтобы он не
/// принялся перезапускать себя снова и снова.
const GUARD: &str = "TUICHAT_LAUNCHED";

/// Терминалы в порядке предпочтения.
///
/// WezTerm впереди Windows Terminal намеренно: он умеет протокол картинок
/// iTerm2, а значит миниатюры в ленте показываются по-настоящему, а не
/// полублоками. Если его нет — Windows Terminal, он всё равно на голову выше
/// conhost.
const TERMINALS: [Terminal; 3] = [
    // Именно `wezterm-gui`, а не `wezterm`: второй собран как консольная
    // программа, и запуск через него мигает лишним чёрным окном.
    Terminal {
        exe: "wezterm-gui.exe",
        kind: Kind::WezTerm,
    },
    Terminal {
        exe: "wezterm.exe",
        kind: Kind::WezTerm,
    },
    Terminal {
        exe: "wt.exe",
        kind: Kind::WindowsTerminal,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Terminal {
    exe: &'static str,
    kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    WezTerm,
    WindowsTerminal,
}

impl Kind {
    /// Разбирает имя терминала из настроек. Пусто или незнакомое — `None`,
    /// то есть «выбирай сам по порядку предпочтения».
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "wezterm" => Some(Kind::WezTerm),
            "wt" | "windows-terminal" | "windows terminal" => Some(Kind::WindowsTerminal),
            _ => None,
        }
    }
}

/// Перезапускает клиент в подходящем терминале.
///
/// Возвращает `true`, если новое окно открыто и этому процессу пора уходить.
/// Любая заминка — терминала нет, запуск не удался — означает `false`: тогда
/// клиент просто работает здесь. Остаться в кривом окне неприятно, а вот не
/// запуститься вовсе — уже поломка.
pub fn relaunch_if_needed(mode: Mode, prefer: &str) -> bool {
    if !should_relaunch(mode) {
        return false;
    }
    let Some(terminal) = find_terminal(prefer) else {
        return false;
    };
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    spawn(terminal, &exe, &args).is_ok()
}

/// Обстановка, в которой запустили клиент.
///
/// Вынесена в структуру, чтобы решение проверялось тестами: сами по себе
/// консоль и переменные окружения под `cargo test` не воспроизводятся.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Situation {
    /// Это уже перезапущенный нами процесс.
    guarded: bool,
    /// И ввод, и вывод — настоящий терминал, а не файл или канал.
    tty: bool,
    /// Окно уже приличное: Windows Terminal, WezTerm, VS Code, ssh.
    modern: bool,
    /// Запустили двойным кликом, а не из оболочки.
    double_clicked: bool,
    windows: bool,
}

impl Situation {
    /// Снимает обстановку с настоящего окружения.
    fn detect() -> Self {
        Self {
            guarded: std::env::var_os(GUARD).is_some(),
            tty: std::io::stdout().is_terminal() && std::io::stdin().is_terminal(),
            modern: in_modern_terminal(),
            double_clicked: double_clicked(),
            windows: cfg!(windows),
        }
    }
}

/// Решает, нужен ли перезапуск, ничего при этом не запуская.
fn should_relaunch(mode: Mode) -> bool {
    decide(mode, Situation::detect())
}

/// Само решение — отдельно от способов всё это выяснить.
fn decide(mode: Mode, at: Situation) -> bool {
    // Перезапускать некуда: на остальных системах человек и так запускает
    // программу из терминала, который выбрал сам.
    if !at.windows || mode == Mode::Never {
        return false;
    }
    // Предохранитель важнее всего: без него перезапуск ушёл бы в бесконечную
    // череду окон.
    if at.guarded {
        return false;
    }
    // Вывод перенаправлен в файл или канал: окно тут вообще ни при чём.
    if !at.tty {
        return false;
    }
    // Раз окно уже приличное, трогать его незачем — и в режиме `always` тоже.
    if at.modern {
        return false;
    }
    match mode {
        Mode::Always => true,
        Mode::Auto => at.double_clicked,
        Mode::Never => false,
    }
}

/// Мы уже в терминале, который всё умеет?
///
/// Смотрим на переменные, которые такие терминалы ставят сами. Список
/// заведомо неполный, и это осознанно: незнакомый терминал лучше принять за
/// хороший и не трогать, чем утащить человека из окна, которое он выбрал.
fn in_modern_terminal() -> bool {
    const MARKERS: [&str; 8] = [
        "WT_SESSION",          // Windows Terminal
        "WEZTERM_PANE",        // WezTerm
        "ALACRITTY_WINDOW_ID", // Alacritty
        "KITTY_WINDOW_ID",     // kitty
        "TERM_PROGRAM",        // VS Code, iTerm2 и другие
        "SSH_TTY",             // сюда пришли по ssh — окно чужое
        "SSH_CONNECTION",
        "MSYSTEM", // git bash в mintty
    ];
    if MARKERS
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    {
        return true;
    }
    // conhost переменную TERM не ставит: если она есть, окно завёл кто-то,
    // кто про терминалы знает.
    std::env::var("TERM").is_ok_and(|term| !term.is_empty() && term != "dumb")
}

/// Программу запустили двойным кликом?
///
/// У окна, созданного проводником, к консоли привязан один процесс — наш
/// собственный. Запуск из оболочки даёт как минимум два: саму оболочку и нас.
/// Так двойной клик отличается от осознанного запуска, и разница эта надёжнее
/// любых догадок по переменным окружения.
#[cfg(windows)]
fn double_clicked() -> bool {
    // kernel32 линкуется стандартной библиотекой, отдельная зависимость ради
    // одного вызова не нужна.
    unsafe extern "system" {
        fn GetConsoleProcessList(process_list: *mut u32, count: u32) -> u32;
    }

    let mut buffer = [0u32; 4];
    // Возвращается общее число процессов, даже если буфер мал; ноль означает
    // «консоли нет» — тогда и перезапускать нечего.
    let count = unsafe { GetConsoleProcessList(buffer.as_mut_ptr(), buffer.len() as u32) };
    count == 1
}

#[cfg(not(windows))]
fn double_clicked() -> bool {
    false
}

/// Первый из установленных терминалов по порядку предпочтения.
fn find_terminal(prefer: &str) -> Option<(Terminal, PathBuf)> {
    let wanted = Kind::parse(prefer);
    TERMINALS
        .iter()
        // Названный в настройках терминал — единственный, который годится:
        // раз человек его выбрал, молча уходить в другой нельзя. Не найдётся —
        // останемся в текущем окне, это честнее подмены.
        .filter(|terminal| wanted.is_none_or(|kind| terminal.kind == kind))
        .find_map(|terminal| which(terminal.exe).map(|path| (*terminal, path)))
}

/// Ищет программу в `PATH`, а на Windows — ещё и в `WindowsApps`.
///
/// Windows Terminal ставится из магазина, и его псевдоним лежит именно там;
/// в `PATH` он есть не у всех.
fn which(exe: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // Дальше — места, куда терминалы ставятся, но которые в `PATH` попадают
    // не всегда. Windows Terminal приезжает из магазина и живёт в WindowsApps;
    // WezTerm ставится в Program Files, а `PATH` в уже открытой сессии об этом
    // не узнает до перезапуска — то есть ровно тогда, когда мы и ищем.
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Microsoft").join("WindowsApps"));
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(variable) {
            roots.push(PathBuf::from(root).join("WezTerm"));
        }
    }

    roots
        .into_iter()
        .map(|root| root.join(exe))
        .find(|candidate| candidate.is_file())
}

/// Открывает клиент в выбранном терминале.
///
/// Аргументы отделены от аргументов самого терминала двойным дефисом: без него
/// `--nick` разбирал бы уже терминал, а не мы.
fn spawn(
    (terminal, path): (Terminal, PathBuf),
    exe: &std::path::Path,
    args: &[OsString],
) -> std::io::Result<std::process::Child> {
    let cwd = std::env::current_dir().ok();
    let mut command = Command::new(path);
    command
        .args(arguments(terminal.kind, cwd.as_deref(), exe, args))
        .env(GUARD, "1");
    command.spawn()
}

/// Собирает строку запуска для выбранного терминала.
///
/// Отдельно от самого запуска, потому что порядок здесь имеет значение и
/// молча ломается: у каждого терминала свои правила, а проверить их иначе
/// как открыв окно — никак.
fn arguments(
    kind: Kind,
    cwd: Option<&std::path::Path>,
    exe: &std::path::Path,
    args: &[OsString],
) -> Vec<OsString> {
    let mut line: Vec<OsString> = Vec::new();
    match kind {
        Kind::WindowsTerminal => {
            // Каталог запуска задаём явно: иначе окно откроется в домашнем,
            // и относительные пути в /send перестанут находиться.
            if let Some(cwd) = cwd {
                line.push("-d".into());
                line.push(cwd.into());
            }
        }
        Kind::WezTerm => {
            // Порядок важен: `--cwd` принадлежит подкоманде `start`, а не
            // самому wezterm. Поставь его раньше — запуск просто не поймут.
            line.push("start".into());
            if let Some(cwd) = cwd {
                line.push("--cwd".into());
                line.push(cwd.into());
            }
        }
    }
    // Двойной дефис отделяет наши аргументы от аргументов терминала: без него
    // `--nick` разбирал бы уже терминал, а не мы.
    line.push("--".into());
    line.push(exe.into());
    line.extend(args.iter().cloned());
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_are_parsed_in_both_languages() {
        assert_eq!(Mode::parse("auto"), Some(Mode::Auto));
        assert_eq!(Mode::parse(" ALWAYS "), Some(Mode::Always));
        assert_eq!(Mode::parse("никогда"), Some(Mode::Never));
        assert_eq!(Mode::parse("иногда"), None);
    }

    /// Двойной клик по программе в Windows: ровно тот случай, ради которого
    /// всё и затевалось.
    fn double_click() -> Situation {
        Situation {
            guarded: false,
            tty: true,
            modern: false,
            double_clicked: true,
            windows: true,
        }
    }

    #[test]
    fn a_double_click_into_conhost_is_relaunched() {
        assert!(decide(Mode::Auto, double_click()));
    }

    #[test]
    fn a_shell_launch_is_left_alone_on_auto() {
        // Человек сам открыл cmd и запустил клиент — уводить его в другое
        // окно самоуправство. А вот `always` для того и заведён.
        let from_shell = Situation {
            double_clicked: false,
            ..double_click()
        };

        assert!(!decide(Mode::Auto, from_shell));
        assert!(decide(Mode::Always, from_shell));
    }

    #[test]
    fn a_modern_terminal_is_never_disturbed() {
        // Windows Terminal, WezTerm, VS Code, ssh: перезапуск здесь только
        // мигнул бы лишним окном.
        let modern = Situation {
            modern: true,
            ..double_click()
        };

        for mode in [Mode::Auto, Mode::Always] {
            assert!(!decide(mode, modern), "{mode:?} тронул хорошее окно");
        }
    }

    #[test]
    fn a_relaunched_process_does_not_relaunch_again() {
        // Без предохранителя перезапуск ушёл бы в бесконечную череду окон.
        let guarded = Situation {
            guarded: true,
            ..double_click()
        };

        for mode in [Mode::Auto, Mode::Always] {
            assert!(!decide(mode, guarded), "{mode:?} перезапустился повторно");
        }
    }

    #[test]
    fn a_redirected_run_is_left_alone() {
        // Вывод в файл или канал: окна нет вовсе, и открывать его нельзя —
        // иначе сборочная машина принялась бы плодить терминалы.
        let piped = Situation {
            tty: false,
            ..double_click()
        };

        for mode in [Mode::Auto, Mode::Always] {
            assert!(!decide(mode, piped), "{mode:?} открыл окно без терминала");
        }
    }

    #[test]
    fn never_means_never() {
        assert!(!decide(Mode::Never, double_click()));
        assert!(!should_relaunch(Mode::Never));
    }

    #[test]
    fn other_systems_are_not_touched() {
        // На macOS и Linux двойного клика по консольной программе нет, а
        // терминал человек выбирает сам.
        let elsewhere = Situation {
            windows: false,
            ..double_click()
        };

        for mode in [Mode::Auto, Mode::Always] {
            assert!(!decide(mode, elsewhere), "{mode:?} полез не в свою систему");
        }
    }

    /// Строка запуска в виде, удобном для сравнения.
    fn line(kind: Kind) -> Vec<String> {
        arguments(
            kind,
            Some(std::path::Path::new(r"C:\work")),
            std::path::Path::new(r"C:\chat\tui.exe"),
            &["--nick".into(), "alice".into()],
        )
        .iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect()
    }

    #[test]
    fn our_arguments_are_separated_from_the_terminals_own() {
        // Без двойного дефиса `--nick` разобрал бы сам терминал и запуск
        // развалился бы — причём молча.
        for kind in [Kind::WindowsTerminal, Kind::WezTerm] {
            let line = line(kind);
            let dashes = line.iter().position(|arg| arg == "--").expect("нет «--»");
            let nick = line.iter().position(|arg| arg == "--nick").unwrap();
            assert!(dashes < nick, "{line:?}");
            assert_eq!(line.last().unwrap(), "alice", "{line:?}");
        }
    }

    #[test]
    fn wezterm_gets_the_working_directory_after_its_subcommand() {
        // `--cwd` принадлежит подкоманде `start`. Поставленный раньше, он
        // просто не понимается — а окно при этом откроется не там, где надо.
        let line = line(Kind::WezTerm);

        let start = line
            .iter()
            .position(|arg| arg == "start")
            .expect("нет start");
        let cwd = line
            .iter()
            .position(|arg| arg == "--cwd")
            .expect("нет --cwd");
        assert!(start < cwd, "{line:?}");
        assert_eq!(line[cwd + 1], r"C:\work", "{line:?}");
    }

    #[test]
    fn windows_terminal_gets_the_working_directory_before_the_dashes() {
        // Без каталога окно откроется в домашнем, и относительные пути в
        // /send перестанут находиться.
        let line = line(Kind::WindowsTerminal);

        assert_eq!(line[0], "-d", "{line:?}");
        assert_eq!(line[1], r"C:\work", "{line:?}");
        assert_eq!(line[2], "--", "{line:?}");
    }

    #[test]
    fn a_named_terminal_is_the_only_one_considered() {
        // Человек написал в настройках «wt» — значит, уводить его в WezTerm
        // нельзя, даже если тот стоит и в списке идёт раньше.
        assert_eq!(Kind::parse("wt"), Some(Kind::WindowsTerminal));
        assert_eq!(Kind::parse(" WezTerm "), Some(Kind::WezTerm));
        assert_eq!(Kind::parse("windows-terminal"), Some(Kind::WindowsTerminal));

        // Пусто и незнакомое означают «выбирай сам»: отказываться запускаться
        // из-за опечатки в настройках — худшее, что можно сделать.
        assert_eq!(Kind::parse(""), None);
        assert_eq!(Kind::parse("ghostty"), None);
    }

    #[test]
    fn wezterm_is_preferred_over_windows_terminal() {
        // У WezTerm есть протокол картинок iTerm2 — миниатюры в ленте
        // показываются по-настоящему. Порядок списка это и означает.
        let kinds: Vec<Kind> = TERMINALS.iter().map(|terminal| terminal.kind).collect();
        let wezterm = kinds.iter().position(|kind| *kind == Kind::WezTerm);
        let windows = kinds.iter().position(|kind| *kind == Kind::WindowsTerminal);

        assert!(wezterm < windows, "{kinds:?}");
        // Консольный `wezterm.exe` мигает лишним окном, поэтому GUI-бинарь
        // должен проверяться раньше.
        assert_eq!(TERMINALS[0].exe, "wezterm-gui.exe");
    }

    #[test]
    fn tests_run_without_a_console_and_so_do_not_relaunch() {
        // Под `cargo test` вывод перехвачен: настоящая проверка обстановки
        // должна это увидеть и промолчать.
        assert!(!should_relaunch(Mode::Auto));
        assert!(!should_relaunch(Mode::Always));
    }
}
