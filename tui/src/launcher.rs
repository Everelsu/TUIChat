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
const TERMINALS: [Terminal; 2] = [
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

/// Перезапускает клиент в подходящем терминале.
///
/// Возвращает `true`, если новое окно открыто и этому процессу пора уходить.
/// Любая заминка — терминала нет, запуск не удался — означает `false`: тогда
/// клиент просто работает здесь. Остаться в кривом окне неприятно, а вот не
/// запуститься вовсе — уже поломка.
pub fn relaunch_if_needed(mode: Mode) -> bool {
    if !should_relaunch(mode) {
        return false;
    }
    let Some(terminal) = find_terminal() else {
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
fn find_terminal() -> Option<(Terminal, PathBuf)> {
    TERMINALS
        .iter()
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

    let local = std::env::var_os("LOCALAPPDATA")?;
    let candidate = PathBuf::from(local)
        .join("Microsoft")
        .join("WindowsApps")
        .join(exe);
    candidate.is_file().then_some(candidate)
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
    let mut command = Command::new(path);
    let cwd = std::env::current_dir().ok();

    match terminal.kind {
        Kind::WindowsTerminal => {
            // Каталог запуска задаём явно: иначе окно откроется в домашнем,
            // и относительные пути в /send перестанут находиться.
            if let Some(cwd) = &cwd {
                command.arg("-d").arg(cwd);
            }
            command.arg("--");
        }
        Kind::WezTerm => {
            if let Some(cwd) = &cwd {
                command.arg("--cwd").arg(cwd);
            }
            command.arg("start").arg("--");
        }
    }

    command.arg(exe).args(args).env(GUARD, "1");
    command.spawn()
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

    #[test]
    fn never_means_never() {
        assert!(!should_relaunch(Mode::Never));
    }

    #[test]
    fn a_relaunched_process_does_not_relaunch_again() {
        // Предохранитель важнее всех прочих проверок: без него перезапуск
        // мог бы уйти в бесконечный цикл окон.
        unsafe { std::env::set_var(GUARD, "1") };
        let auto = should_relaunch(Mode::Auto);
        let always = should_relaunch(Mode::Always);
        unsafe { std::env::remove_var(GUARD) };

        assert!(!auto);
        assert!(!always);
    }

    #[test]
    fn tests_run_without_a_console_and_so_do_not_relaunch() {
        // Под `cargo test` вывод перехвачен, то есть терминала нет. Проверка
        // на это стоит раньше всех догадок про окно — иначе сборочная машина
        // принялась бы открывать окна.
        assert!(!should_relaunch(Mode::Auto));
    }
}
