//! Отрисовка. Никакой логики, кроме переноса строк и подсветки.
//!
//! Экранов два: главный — с вкладками «войти», «поднять», «вид», «справка» —
//! и переписка. Всё остальное (справка, обзор файлов, просмотр картинки)
//! ложится поверх окном, а не занимает место в ленте.
//!
//! Рамок ровно столько, сколько нужно, чтобы отделить одно от другого: каждая
//! съедает две колонки и две строки, которых в терминале мало. Границы между
//! частями держатся цветом и воздухом, а не линиями.

use chrono::{DateTime, Local};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{Input, Screen, State},
    images::Images,
    theme,
};

mod chat;
mod feed;
mod home;
mod overlay;
mod widgets;

pub use feed::{Decor, Slot};

/// Отступ содержимого от края экрана.
pub(crate) const GUTTER: &str = " ";

pub fn draw(frame: &mut Frame, state: &mut State, images: &mut Images) {
    if let Screen::Login(login) = &state.screen {
        home::draw(frame, login, state, frame.area());
        return;
    }

    chat::draw(frame, state, images);
    // Картинка рисуется поверх переписки: так не приходится пересчитывать
    // высоту сообщений и ломать прокрутку ради одного вложения.
    if let Some(viewer) = &state.viewer {
        overlay::viewer(frame, viewer, state.theme, frame.area(), images);
    }
    if let Some(browser) = &state.browser {
        overlay::browser(frame, browser, state.theme, frame.area());
    }
    if state.help {
        overlay::help(frame, state.theme, state.help_scroll, frame.area());
    }
}

/// Рисует однострочное поле и ставит в него курсор, прокручивая текст так,
/// чтобы курсор всегда оставался виден.
pub(crate) fn draw_field(frame: &mut Frame, input: &Input, area: Rect, prefix: &[Span<'static>]) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let prefix_width: usize = prefix.iter().map(|span| span.content.width()).sum();
    let before: String = input.text.chars().take(input.cursor).collect();
    let cursor_w = before.width();
    let visible_w = area.width.saturating_sub(prefix_width as u16 + 1) as usize;
    let offset = cursor_w.saturating_sub(visible_w);
    let (_, shown) = split_at_width(&input.text, offset);

    let mut spans = prefix.to_vec();
    spans.push(Span::styled(
        shown.to_string(),
        Style::new().fg(theme::TEXT),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    frame.set_cursor_position(Position::new(
        area.x + prefix_width as u16 + (cursor_w - offset) as u16,
        area.y,
    ));
}

/// Текст поля, влезающий в отведённую ширину, и колонка курсора внутри него.
///
/// Нужен там, где строка собирается заранее, а курсор ставится потом: на
/// главном экране содержимое вкладки едет при переключении, и настоящие
/// координаты известны только после анимации.
pub(crate) fn field_view(input: &Input, width: usize) -> (String, usize) {
    let width = width.max(1);
    let before: String = input.text.chars().take(input.cursor).collect();
    let cursor_w = before.width();
    let offset = cursor_w.saturating_sub(width.saturating_sub(1));
    let (_, shown) = split_at_width(&input.text, offset);
    let (visible, _) = split_at_width(shown, width);
    (visible.to_string(), cursor_w - offset)
}

/// Обрезает длинный путь слева: конец пути важнее начала.
pub(crate) fn shorten_left(text: &str, width: usize) -> String {
    if text.width() <= width || width < 2 {
        return text.to_string();
    }
    let mut tail = String::new();
    for ch in text.chars().rev() {
        if tail.width() + ch.width().unwrap_or(0) + 1 > width {
            break;
        }
        tail.insert(0, ch);
    }
    format!("…{tail}")
}

/// Обрезает строку справа многоточием: имена и адреса в узкой колонке.
pub(crate) fn shorten(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let (head, _) = split_at_width(text, width - 1);
    format!("{head}…")
}

pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Размер файла в привычном виде: «240 КБ» читается, «245760» — нет.
pub(crate) fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    match bytes {
        0..KB => format!("{bytes} Б"),
        KB..MB => format!("{} КБ", bytes / KB),
        _ => format!("{},{} МБ", bytes / MB, (bytes % MB) * 10 / MB),
    }
}

pub(crate) fn local_time(ts: i64) -> String {
    // Время приходит в UTC, показываем в часовом поясе того, кто смотрит.
    DateTime::from_timestamp_millis(ts)
        .map(|utc| utc.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_string())
}

/// Подпись раздела: короткая, разрядкой и градиентом.
///
/// Заглавные буквы вразрядку читаются как заголовок даже без рамки — этим
/// и держится структура экрана там, где линий нет.
pub(crate) fn caption(text: &str, from: Color, to: Color) -> Vec<Span<'static>> {
    let spaced: String = text
        .to_uppercase()
        .chars()
        // После пробела второй не нужен: между словами и так получается
        // двойной, а тройной уже разваливает подпись на куски.
        .flat_map(|ch| if ch == ' ' { vec![ch] } else { vec![ch, ' '] })
        .collect();
    theme::gradient_bold(spaced.trim_end(), from, to)
}

/// Строка из подсказок «клавиша — что делает».
pub(crate) fn hints(pairs: &[(&str, &str)], accent: Color) -> Line<'static> {
    let mut spans = Vec::new();
    for (at, (key, what)) in pairs.iter().enumerate() {
        if at > 0 {
            spans.push(theme::separator());
        }
        spans.extend(theme::key_hint(key, what, accent));
    }
    Line::from(spans)
}

/// Заполняет строку до нужной ширины пробелами.
pub(crate) fn pad(line: Line<'static>, width: usize) -> Line<'static> {
    let have = line.width();
    if have >= width {
        return line;
    }
    let mut spans = line.spans;
    spans.push(Span::raw(" ".repeat(width - have)));
    Line::from(spans)
}

/// Ставит две колонки рядом, выравнивая левую по ширине.
pub(crate) fn columns(
    left: Vec<Line<'static>>,
    right: Vec<Line<'static>>,
    left_width: usize,
    gap: usize,
) -> Vec<Line<'static>> {
    let rows = left.len().max(right.len());
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut line = pad(left.get(row).cloned().unwrap_or_default(), left_width + gap);
        if let Some(right) = right.get(row) {
            line.spans.extend(right.spans.iter().cloned());
        }
        lines.push(line);
    }
    lines
}

/// Перенос по словам с учётом ширины символов, а не их количества:
/// кириллица, эмодзи и иероглифы занимают разное число ячеек терминала.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        let mut rest = word;
        loop {
            let separator = usize::from(!current.is_empty());
            let free = width.saturating_sub(current.width() + separator);

            if rest.width() <= free {
                if separator == 1 {
                    current.push(' ');
                }
                current.push_str(rest);
                break;
            }

            if rest.width() > width {
                // Слово длиннее строки целиком (ссылка, «ааааа») — режем силой,
                // иначе оно вылезет за край.
                if free == 0 {
                    lines.push(std::mem::take(&mut current));
                    continue;
                }
                if separator == 1 {
                    current.push(' ');
                }
                let (head, tail) = split_at_width(rest, free);
                current.push_str(head);
                lines.push(std::mem::take(&mut current));
                rest = tail;
            } else {
                lines.push(std::mem::take(&mut current));
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Делит строку по видимой ширине.
pub(crate) fn split_at_width(text: &str, width: usize) -> (&str, &str) {
    let mut used = 0;
    for (at, ch) in text.char_indices() {
        let char_width = ch.width().unwrap_or(0);
        if used + char_width > width {
            // Символ шире всей колонки — откусываем его целиком, иначе перенос
            // не сдвинется с места. При нулевой ширине откусывать нечего.
            if at == 0 && width > 0 {
                return text.split_at(ch.len_utf8());
            }
            return text.split_at(at);
        }
        used += char_width;
    }
    (text, "")
}

/// Жирный акцентный текст — им подписаны выбранные строки списков.
pub(crate) fn strong(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(
        text.into(),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Action, Entry, Input, NetEvent, Viewer, ViewerState, update};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    };
    use uuid::Uuid;

    /// Рисует состояние в буфер нужного размера и отдаёт его текстом.
    pub(crate) fn render(state: &mut State, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, state, &mut crate::images::Images::disabled()))
            .unwrap();
        terminal.backend().to_string()
    }

    /// То же, но вместе с цветами: текстом их не видно, а половина
    /// оформления держится именно на них.
    pub(crate) fn render_buffer(
        state: &mut State,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, state, &mut crate::images::Images::disabled()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn wraps_on_word_boundaries() {
        assert_eq!(wrap("одна две три", 8), ["одна две", "три"]);
    }

    #[test]
    fn long_word_is_split_by_force() {
        // Ссылка без пробелов не должна вылезать за край.
        assert_eq!(wrap("ааааааааа", 4), ["аааа", "аааа", "а"]);
    }

    #[test]
    fn wide_characters_count_as_two_cells() {
        // Иероглифы занимают две ячейки: по символам влезло бы четыре.
        assert_eq!(wrap("水水水水", 4), ["水水", "水水"]);
    }

    #[test]
    fn narrow_column_with_wide_character_terminates() {
        // Символ шире всей колонки — перенос обязан продвигаться, а не зациклиться.
        assert_eq!(wrap("水水", 1), ["水", "水"]);
    }

    #[test]
    fn empty_text_gives_one_empty_line() {
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(512), "512 Б");
        assert_eq!(human_size(240 * 1024), "240 КБ");
        assert_eq!(human_size(3 * 1024 * 1024 + 512 * 1024), "3,5 МБ");
    }

    #[test]
    fn long_path_is_cut_from_the_left() {
        let short = shorten_left("C:/фото", 20);
        let long = shorten_left("C:/очень/длинный/путь/куда-то/вглубь/фото", 20);

        assert_eq!(short, "C:/фото");
        // Конец пути важнее начала: по нему понятно, где ты.
        assert!(long.starts_with('…'), "{long}");
        assert!(long.ends_with("фото"), "{long}");
        assert!(long.width() <= 20, "{long}");
    }

    #[test]
    fn names_are_cut_from_the_right() {
        assert_eq!(shorten("общий", 10), "общий");
        let cut = shorten("очень-длинное-имя-комнаты", 10);
        assert!(cut.ends_with('…'), "{cut}");
        assert!(cut.width() <= 10, "{cut}");
    }

    #[test]
    fn columns_line_up_at_the_given_width() {
        let left = vec![Line::from("ник")];
        let right = vec![Line::from("комнаты")];

        let joined = columns(left, right, 10, 2);

        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].width(), 12 + "комнаты".width());
    }

    /// Прогон отрисовки по всем состояниям и множеству размеров окна.
    ///
    /// Ловит ровно тот класс падений, из-за которого клиент «вылетает»:
    /// вычитание с переполнением и выход за границы при узком окне.
    #[test]
    fn every_screen_survives_every_size() {
        let sizes = [
            (1, 1),
            (2, 3),
            (4, 2),
            (7, 5),
            (12, 4),
            (20, 6),
            (40, 10),
            (80, 24),
            (200, 60),
        ];

        for (width, height) in sizes {
            // Главный экран целиком: каждая вкладка, в том числе с ошибкой.
            let (mut home, _) = State::new(None, "general".into());
            for _ in 0..crate::app::HomeTab::ALL.len() {
                render(&mut home, width, height);
                update(
                    &mut home,
                    Action::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
                );
            }
            update(
                &mut home,
                Action::Net(NetEvent::Fatal {
                    reason: "ник занят".into(),
                }),
            );
            render(&mut home, width, height);

            // Переписка со всеми украшениями сразу.
            let mut chat = chat::tests::populated();
            chat.input = Input::new("длинная строка ввода, которая не влезает целиком");
            render(&mut chat, width, height);

            // Колонка с людьми — и в узком окне, где ей не место.
            chat.sidebar = true;
            render(&mut chat, width, height);
            chat.sidebar = false;

            // Прокрутка вверх и вниз до упора.
            chat.scrollback = usize::MAX / 2;
            render(&mut chat, width, height);
            chat.scrollback = 0;

            // Поиск, выбор ответа и взведённая цитата.
            update(
                &mut chat,
                Action::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            );
            render(&mut chat, width, height);
            update(
                &mut chat,
                Action::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            );
            update(
                &mut chat,
                Action::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            );
            render(&mut chat, width, height);
            update(
                &mut chat,
                Action::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            );
            render(&mut chat, width, height);

            // Просмотр картинки во всех трёх состояниях.
            for state in [
                ViewerState::Loading,
                ViewerState::Failed("сервер ответил: HTTP/1.1 404 Not Found".into()),
                ViewerState::Ready(Box::new(image::RgbImage::from_pixel(
                    40,
                    30,
                    image::Rgb([200, 30, 30]),
                ))),
            ] {
                chat.viewer = Some(Viewer {
                    id: Uuid::nil(),
                    name: "кот.png".into(),
                    state,
                });
                render(&mut chat, width, height);
            }
            chat.viewer = None;

            // Справка и обзор файлов — тоже поверх переписки.
            chat.help = true;
            render(&mut chat, width, height);
            chat.help = false;

            chat.browser = Some(crate::app::Browser {
                dir: std::path::PathBuf::from("C:/очень/длинный/путь/куда-то/вглубь"),
                entries: vec![crate::files::FileEntry {
                    name: "кот.png".into(),
                    path: std::path::PathBuf::from("C:/кот.png"),
                    is_dir: false,
                    size: 1024,
                    media: true,
                }],
                selected: 0,
                filter: Input::default(),
                loading: false,
                error: None,
            });
            render(&mut chat, width, height);
            chat.browser = None;

            // Обрыв связи: в шапке появляется обратный отсчёт.
            update(
                &mut chat,
                Action::Net(NetEvent::Disconnected {
                    reason: "обрыв".into(),
                    retry_at: std::time::Instant::now() + std::time::Duration::from_secs(5),
                }),
            );
            render(&mut chat, width, height);
        }
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        // Окно можно ужать до чего угодно, и это не повод падать посреди сессии.
        for (width, height) in [(1, 1), (3, 2), (5, 4), (20, 5)] {
            let mut chat = chat::tests::populated();
            render(&mut chat, width, height);

            let (mut home, _) = State::new(None, "general".into());
            render(&mut home, width, height);
        }
    }

    /// Переписка не должна остаться пустой из-за нового оформления: тест
    /// сторожит, что записи обоих видов доезжают до экрана.
    #[test]
    fn feed_reaches_the_screen() {
        let mut state = chat::tests::populated();
        assert!(
            state
                .entries
                .iter()
                .any(|entry| matches!(entry, Entry::System { .. }))
        );

        let screen = render(&mut state, 60, 16);

        assert!(screen.contains("привет"), "{screen}");
    }
}
