//! Отрисовка. Никакой логики, кроме переноса строк и подсветки.
//!
//! Рамок почти нет: воздух и цвет разделяют содержимое лучше, чем линии, а
//! каждая рамка съедает две колонки и две строки, которых в терминале мало.
//! Обведён только ввод — единственное место, где нужно показать границу поля.

use chrono::{DateTime, Local};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
};
use std::collections::HashMap;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{
        Browser, Entry, Field, Input, Login, Screen, Search, State, Status, SystemKind, Thumbnail,
        Viewer, ViewerState, Viewport,
    },
    images::Images,
    media,
};

/// Цвета заданы в RGB, а не именами: именованные восемь цветов терминалы
/// перекрашивают по своей теме, и оттенки разъезжаются от машины к машине.
mod palette {
    use ratatui::style::Color;

    pub const ACCENT: Color = Color::Rgb(217, 119, 87);
    pub const DIM: Color = Color::Rgb(128, 132, 138);
    pub const FAINT: Color = Color::Rgb(90, 94, 100);
    pub const OK: Color = Color::Rgb(126, 186, 128);
    pub const ERR: Color = Color::Rgb(226, 108, 108);
    pub const MENTION: Color = Color::Rgb(214, 173, 96);

    /// Палитра ников. Те же цвета живут в веб-клиенте, чтобы человек выглядел
    /// одинаково в терминале и в браузере.
    pub const NICKS: [Color; 6] = [
        Color::Rgb(94, 186, 176),
        Color::Rgb(126, 186, 128),
        Color::Rgb(214, 173, 96),
        Color::Rgb(178, 148, 214),
        Color::Rgb(114, 159, 207),
        Color::Rgb(214, 128, 128),
    ];
}

/// Кадры спиннера: точечные символы крутятся плавнее, чем палочка из `|/-\`.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Сообщения одного человека подряд объединяются в группу, если между ними
/// меньше двух минут: подпись над каждой репликой превращает переписку в
/// частокол из ников.
const GROUP_WINDOW_MS: i64 = 120_000;

/// Отступ содержимого от края экрана.
const GUTTER: &str = " ";

/// Метка свежей реплики в левом поле.
const FRESH_MARK: &str = "\u{258f}";

/// Высота миниатюры в строках и её предельная ширина в колонках.
///
/// Лента должна оставаться лентой: картинка во весь экран вытесняет разговор,
/// а разглядеть её целиком можно по `/view`.
const THUMB_ROWS: u16 = 10;
const THUMB_COLS: u16 = 46;

/// Сколько живёт вспышка у нового сообщения.
const FLASH: std::time::Duration = std::time::Duration::from_millis(900);

/// Ступени столбика громкости: восемь уровней плюс тишина.
const WAVE_STEPS: [&str; 9] = ["▁", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Рисует голосовое графиком: столбики громкости и длительность.
///
/// Пока запись играет, пройденная часть подсвечена — по ней видно, сколько
/// осталось, а заодно понятно, что звук вообще идёт: в терминале это иначе
/// никак не показать.
fn draw_voice(
    wave: &media::Waveform,
    playing: Option<std::time::Duration>,
    name: &str,
) -> Vec<Span<'static>> {
    let played = match playing {
        // Доля проигранного. Ноль длительности означает «ещё не знаем» —
        // тогда просто ничего не подсвечиваем.
        Some(elapsed) if wave.millis > 0 => {
            (elapsed.as_millis() as f32 / wave.millis as f32).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };
    let edge = (played * wave.bars.len() as f32).round() as usize;

    let mut spans = vec![Span::styled("♪ ", Style::new().fg(palette::ACCENT))];
    for (index, level) in wave.bars.iter().enumerate() {
        let step = WAVE_STEPS[(*level as usize).min(WAVE_STEPS.len() - 1)];
        // Пройденное — акцентом, остальное приглушено: граница между ними и
        // есть указатель воспроизведения.
        let colour = if index < edge {
            palette::ACCENT
        } else {
            palette::DIM
        };
        spans.push(Span::styled(step.to_string(), Style::new().fg(colour)));
    }

    let seconds = wave.millis / 1000;
    spans.push(Span::styled(
        format!("  {}:{:02}  {name}", seconds / 60, seconds % 60),
        Style::new().fg(palette::FAINT),
    ));
    spans
}

pub fn draw(frame: &mut Frame, state: &mut State, images: &mut Images) {
    if let Screen::Login(login) = &state.screen {
        draw_login(frame, login, frame.area());
        return;
    }

    draw_chat(frame, state, images);
    // Картинка рисуется поверх переписки: так не приходится пересчитывать
    // высоту сообщений и ломать прокрутку ради одного вложения.
    if let Some(viewer) = &state.viewer {
        draw_viewer(frame, viewer, frame.area(), images);
    }
    if let Some(browser) = &state.browser {
        draw_browser(frame, browser, frame.area());
    }
    if state.help {
        draw_help(frame, frame.area());
    }
}

fn draw_browser(frame: &mut Frame, browser: &Browser, area: Rect) {
    let area = centered(area, 74.min(area.width), 22.min(area.height));
    frame.render_widget(Clear, area);

    let title = shorten_left(
        &browser.dir.to_string_lossy(),
        area.width.saturating_sub(4) as usize,
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "))
        .title_bottom(" enter выбрать · ← наверх · esc отмена ")
        .border_style(Style::new().fg(palette::ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 {
        return;
    }

    let [list_area, filter_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    let visible = browser.visible();
    let lines: Vec<Line> = if browser.loading {
        vec![Line::from(Span::styled(
            format!("{GUTTER}читаю…"),
            Style::new().fg(palette::DIM),
        ))]
    } else if let Some(error) = &browser.error {
        vec![Line::from(Span::styled(
            format!("{GUTTER}{error}"),
            Style::new().fg(palette::ERR),
        ))]
    } else if visible.is_empty() {
        vec![Line::from(Span::styled(
            format!("{GUTTER}ничего не нашлось"),
            Style::new().fg(palette::DIM),
        ))]
    } else {
        // Окно списка едет за выбором: без этого в длинном каталоге выбранное
        // уезжает за край и стрелки перестают что-либо значить.
        let height = list_area.height as usize;
        let start = browser.selected.saturating_sub(height.saturating_sub(1));
        visible
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(index, entry)| {
                let chosen = index == browser.selected;
                // Приглушать нечего: отправить можно любой файл. Каталог
                // выделен цветом, всё остальное — обычным.
                let color = if entry.is_dir {
                    palette::ACCENT
                } else {
                    Color::Reset
                };
                let mut style = Style::new().fg(color);
                if chosen {
                    style = style.bg(palette::FAINT).fg(Color::Rgb(20, 20, 24));
                }

                // Метка говорит, что случится после отправки: картинка и звук
                // покажутся прямо в переписке, остальное придёт строкой.
                let mark = if entry.is_dir {
                    "▸ "
                } else if entry.media {
                    "◈ "
                } else {
                    "  "
                };
                let size = if entry.is_dir {
                    String::new()
                } else {
                    human_size(entry.size)
                };
                let room = list_area.width as usize;
                let name = format!("{GUTTER}{mark}{}", entry.name);
                let pad = room.saturating_sub(name.width() + size.width() + 1);
                Line::from(Span::styled(
                    format!("{name}{}{size} ", " ".repeat(pad)),
                    style,
                ))
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), list_area);

    draw_field(frame, &browser.filter, filter_area, " отсев: ");
}

/// Обрезает длинный путь слева: конец пути важнее начала.
fn shorten_left(text: &str, width: usize) -> String {
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

fn draw_help(frame: &mut Frame, area: Rect) {
    let width = 72.min(area.width);
    let height = (crate::app::HELP.len() as u16 + 7).min(area.height);
    let area = centered(area, width, height);
    frame.render_widget(Clear, area);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" команды ")
        .title_bottom(" любая клавиша — закрыть ")
        .border_style(Style::new().fg(palette::ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = crate::app::HELP
        .iter()
        .map(|line| {
            // Саму команду выделяем: глаз ищет в такой справке именно её.
            match line.split_once(" — ") {
                Some((command, what)) => Line::from(vec![
                    Span::styled(
                        // Пробел в конце обязателен: у длинной команды колонка
                        // кончается, и описание слиплось бы с ней.
                        format!("{GUTTER}{command:<28} "),
                        Style::new().fg(palette::ACCENT),
                    ),
                    Span::styled(what.to_string(), Style::new().fg(palette::DIM)),
                ]),
                None => Line::from(Span::styled(
                    format!("{GUTTER}{line}"),
                    Style::new().fg(palette::DIM),
                )),
            }
        })
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("{GUTTER}tab ник и команда · ctrl+o файл · ctrl+r ответ · ctrl+f поиск"),
        Style::new().fg(palette::FAINT),
    )));
    lines.push(Line::from(Span::styled(
        // Перехват мыши нужен ради прокрутки колесом и ломает выделение.
        // Об этом надо сказать: иначе выглядит как поломка.
        format!("{GUTTER}выделять текст мышью — с зажатым shift"),
        Style::new().fg(palette::FAINT),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_chat(frame: &mut Frame, state: &mut State, images: &mut Images) {
    // Строка с цитатой появляется, только когда ответ взведён: постоянно
    // держать под неё место жалко.
    let reply_height = u16::from(state.replying.is_some());
    let [header, messages, reply, input, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(reply_height),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, state, header);
    draw_messages(frame, state, messages, images);
    draw_reply_bar(frame, state, reply);
    draw_input(frame, state, input);
    draw_hint(frame, state, hint);
}

fn draw_reply_bar(frame: &mut Frame, state: &State, area: Rect) {
    let Some(target) = &state.replying else {
        return;
    };
    if area.height == 0 {
        return;
    }

    let line = Line::from(vec![
        Span::styled(format!("{GUTTER} ↩ "), Style::new().fg(palette::ACCENT)),
        Span::styled(
            format!("{}: ", target.nickname),
            Style::new().fg(nick_color(&state.colors, &target.nickname, false)),
        ),
        Span::styled(target.excerpt.clone(), Style::new().fg(palette::DIM)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_header(frame: &mut Frame, state: &State, area: Rect) {
    if area.width == 0 {
        return;
    }

    let room = Span::styled(
        format!("{GUTTER}#{}", state.room),
        Style::new()
            .fg(palette::ACCENT)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(Paragraph::new(Line::from(room)), area);

    // Справа — кто в комнате и состояние связи. Список участников уехал сюда
    // из отдельной панели: она отнимала пятую часть ширины ради столбика имён.
    let status = status_span(state);
    let people = people_summary(state, area.width.saturating_sub(status.width() as u16 + 12));
    let right = Line::from(vec![
        Span::styled(people, Style::new().fg(palette::FAINT)),
        Span::raw("  "),
        status,
        Span::raw(GUTTER),
    ]);
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

/// Имена участников, обрезанные под доступную ширину.
fn people_summary(state: &State, width: u16) -> String {
    if state.users.is_empty() {
        return String::new();
    }

    let names: Vec<&str> = state
        .users
        .iter()
        .map(|user| user.nickname.as_str())
        .collect();
    let full = names.join(", ");
    if full.width() <= width as usize {
        return full;
    }
    format!("{} человек", names.len())
}

fn status_span(state: &State) -> Span<'static> {
    match &state.status {
        Status::Connecting { .. } => {
            let frame = SPINNER[(state.tick as usize / 2) % SPINNER.len()];
            Span::styled(
                format!("{frame} подключаюсь"),
                Style::new().fg(palette::MENTION),
            )
        }
        Status::Online => Span::styled("● в сети", Style::new().fg(palette::OK)),
        Status::Reconnecting { retry_at, .. } => {
            let left = retry_at.saturating_duration_since(std::time::Instant::now());
            Span::styled(
                format!("● нет связи · {}с", left.as_secs() + 1),
                Style::new().fg(palette::ERR),
            )
        }
    }
}

fn draw_messages(frame: &mut Frame, state: &mut State, area: Rect, images: &mut Images) {
    let width = area.width.saturating_sub(2) as usize;
    let Rendered {
        lines,
        offsets,
        slots,
    } = render_entries(
        &state.entries,
        width,
        &Decor {
            search: state.search.as_ref(),
            picking: state.picking.map(|pick| pick.index),
            colors: &state.colors,
            thumbnails: &state.thumbnails,
            waveforms: &state.waveforms,
            playing: state.playing_voice,
        },
    );
    // Карта «запись -> строка» нужна поиску, чтобы прокрутить к найденному:
    // во сколько строк развернулось сообщение, известно только здесь.
    state.entry_lines = offsets;
    let height = area.height as usize;
    state.viewport = Viewport {
        height,
        total_lines: lines.len(),
        top: area.y,
    };

    // Прокрутка держится за низ истории: клампим здесь, потому что размеры
    // области известны только во время отрисовки.
    state.scrollback = state.scrollback.min(lines.len().saturating_sub(height));
    let end = lines.len() - state.scrollback;
    let start = end.saturating_sub(height);

    // Пока история короче окна, дополняем её сверху пустыми строками: свежие
    // сообщения должны появляться прямо над полем ввода, а не улетать вверх.
    let mut visible: Vec<Line> =
        std::iter::repeat_n(Line::default(), height.saturating_sub(end - start)).collect();
    let padding = height.saturating_sub(end - start);
    visible.extend_from_slice(&lines[start..end]);

    frame.render_widget(Paragraph::new(visible), area);
    draw_thumbnails(frame, state, area, images, &slots, start, end, padding);
}

/// Кладёт картинки в зарезервированные под них строки.
///
/// Отдельным проходом, потому что картинка рисуется виджетом в прямоугольник,
/// а не строкой текста: экранные координаты известны только после того, как
/// стало понятно, какой кусок истории виден.
#[allow(clippy::too_many_arguments)]
fn draw_thumbnails(
    frame: &mut Frame,
    state: &State,
    area: Rect,
    images: &mut Images,
    slots: &[Slot],
    start: usize,
    end: usize,
    padding: usize,
) {
    for slot in slots {
        // Рисуем только целиком поместившиеся: обрезанная картинка меняет
        // высоту на каждый шаг прокрутки, а от этого она перекодируется
        // заново каждый кадр — и лента начинает дёргаться.
        let bottom = slot.line + THUMB_ROWS as usize;
        if slot.line < start || bottom > end {
            continue;
        }

        let Some(Thumbnail::Ready(image)) = state.thumbnails.get(&slot.id) else {
            continue;
        };
        let rect = Rect {
            x: area.x + 2,
            y: area.y + (padding + slot.line - start) as u16,
            width: THUMB_COLS.min(area.width.saturating_sub(2)),
            height: THUMB_ROWS,
        };
        images.render(frame, rect, slot.id, image);
    }
}

fn draw_input(frame: &mut Frame, state: &State, area: Rect) {
    // Во время поиска поле ввода занято запросом: искать и писать
    // одновременно всё равно не получится.
    if let Some(search) = &state.search {
        draw_search(frame, search, area);
        return;
    }

    let border = if state.is_online() {
        palette::FAINT
    } else {
        palette::ERR
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    draw_field(frame, &state.input, inner, " > ");
}

fn draw_search(frame: &mut Frame, search: &Search, area: Rect) {
    let counter = if search.query.is_empty() {
        String::new()
    } else if search.matches.is_empty() {
        " ничего ".to_string()
    } else {
        format!(" {} из {} ", search.current + 1, search.matches.len())
    };

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(palette::MENTION))
        .title_bottom(Line::from(Span::styled(
            counter,
            Style::new().fg(palette::MENTION),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    draw_field(frame, &search.query, inner, " поиск: ");
}

fn draw_hint(frame: &mut Frame, state: &State, area: Rect) {
    // Своя работа важнее чужой: пока что-то качается, показываем именно это.
    if let Some(busy) = &state.busy {
        let frame_index = (state.tick as usize / 2) % SPINNER.len();
        let line = Line::from(vec![
            Span::raw(format!("{GUTTER} ")),
            Span::styled(SPINNER[frame_index], Style::new().fg(palette::ACCENT)),
            Span::styled(format!(" {busy}"), Style::new().fg(palette::DIM)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    // Пока кто-то печатает, подсказка уступает место живой информации:
    // она всё равно повторяется от кадра к кадру, а это — новость.
    let typing = state.typing_now();
    if !typing.is_empty() && state.viewer.is_none() && state.search.is_none() {
        // Точки бегут по тику — единственная анимация, которая тут уместна.
        let dots = ".".repeat(1 + (state.tick as usize / 3) % 3);
        let who = match typing.as_slice() {
            [one] => format!("{one} печатает"),
            [one, two] => format!("{one} и {two} печатают"),
            many => format!("{} человек печатают", many.len()),
        };
        let line = Line::from(vec![
            Span::raw(format!("{GUTTER} ")),
            Span::styled(who, Style::new().fg(palette::DIM)),
            Span::styled(dots, Style::new().fg(palette::ACCENT)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    let hint = if state.viewer.is_some() {
        "esc закрыть картинку"
    } else if let Some(pick) = state.picking {
        match pick.mode {
            crate::app::PickMode::Reply => {
                "стрелки — выбрать сообщение · enter ответить · esc отмена"
            }
            // Здесь важно назвать все три действия: человек пришёл сюда
            // именно потому, что ему нужно не последнее вложение.
            crate::app::PickMode::Attachment => {
                "стрелки — вложение · enter открыть · f3 играть · f5 сохранить · esc отмена"
            }
        }
    } else if state.replying.is_some() {
        "enter отправить ответ · esc снять цитату"
    } else if state.search.is_some() {
        "enter и стрелки — следующее совпадение · esc закрыть поиск"
    } else {
        // Подсказка называет то, ради чего чат и открывают, и называет
        // клавишами, а не командами: человеку, который не пишет код, строка
        // со слэшем ничего не говорит.
        "f2 голосовое · f4 файл · f7 выбрать вложение · f1 всё остальное"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{GUTTER} {hint}"),
            Style::new().fg(palette::FAINT),
        ))),
        area,
    );
}

/// Рисует однострочное поле и ставит в него курсор, прокручивая текст так,
/// чтобы курсор всегда оставался виден.
fn draw_field(frame: &mut Frame, input: &Input, area: Rect, prefix: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let prefix_width = prefix.width() as u16;
    let before: String = input.text.chars().take(input.cursor).collect();
    let cursor_w = before.width();
    let visible_w = area.width.saturating_sub(prefix_width + 1) as usize;
    let offset = cursor_w.saturating_sub(visible_w);
    let (_, shown) = split_at_width(&input.text, offset);

    let line = Line::from(vec![
        Span::styled(prefix.to_string(), Style::new().fg(palette::ACCENT)),
        Span::raw(shown.to_string()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
    frame.set_cursor_position(Position::new(
        area.x + prefix_width + (cursor_w - offset) as u16,
        area.y,
    ));
}

/// Сколько строк списка комнат показываем разом. Больше — список вытесняет
/// саму форму; выбранная строка держится в этом окне прокруткой.
const LOGIN_ROOMS_VISIBLE: usize = 8;

fn draw_login(frame: &mut Frame, login: &Login, area: Rect) {
    // Тело списка: под комнаты или под пояснение, почему их нет. Пустая
    // строка вместо списка выглядела бы как оборванная форма.
    let list_body = if login.rooms.is_empty() {
        1
    } else {
        login.rooms.len().min(LOGIN_ROOMS_VISIBLE)
    };
    // 9 строк формы + подсказка + отступ + заголовок списка + сам список.
    let wanted = 9 + 1 + 1 + 1 + list_body as u16;
    let form = centered(area, 54, wanted);
    if form.height < 5 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(2), // заголовок
        Constraint::Length(1), // подпись «ник»
        Constraint::Length(1), // поле ника
        Constraint::Length(1), // подпись «комната»
        Constraint::Length(1), // поле комнаты
        Constraint::Length(1), // подпись «сервер»
        Constraint::Length(1), // поле сервера
        Constraint::Length(1), // ошибка или подсказка
        Constraint::Length(1), // отступ
        Constraint::Length(1), // заголовок списка комнат
        Constraint::Min(0),    // список комнат
    ])
    .split(form);

    let label = Style::new().fg(palette::FAINT);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "чат",
            Style::new()
                .fg(palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );
    frame.render_widget(Paragraph::new(Span::styled("ник", label)), rows[1]);
    frame.render_widget(Paragraph::new(Span::styled("комната", label)), rows[3]);
    frame.render_widget(
        Paragraph::new(Span::styled("сервер — адрес или тикет друга", label)),
        rows[5],
    );

    // Активное поле помечено стрелкой, остальные просто нарисованы: курсор
    // в терминале один, и он должен стоять там, куда попадёт ввод. Пока
    // выбрана комната из списка, курсор в поле не мигаем — ввод уйдёт туда,
    // куда смотрит выделение.
    let field_focus = login.rooms_selected.is_none();
    let fields = [
        (Field::Nickname, &login.nickname, rows[2]),
        (Field::Room, &login.room, rows[4]),
        (Field::Server, &login.server, rows[6]),
    ];
    for (field, input, row) in fields {
        if field == login.field && field_focus {
            draw_field(frame, input, row, "> ");
        } else {
            frame.render_widget(Paragraph::new(format!("  {}", input.text)), row);
        }
    }

    let footer = match &login.error {
        Some(error) => Line::from(Span::styled(error.clone(), Style::new().fg(palette::ERR))),
        None => Line::from(Span::styled(
            "tab — поле · ↑↓ — комната · enter — войти · ctrl+r — обновить",
            label,
        )),
    };
    frame.render_widget(Paragraph::new(footer), rows[7]);

    draw_login_rooms(frame, login, rows[9], rows[10]);
}

/// Список живущих на сервере комнат под формой входа: выбрал стрелками —
/// и зашёл, ни у кого не спрашивая адрес.
fn draw_login_rooms(frame: &mut Frame, login: &Login, header: Rect, body: Rect) {
    let label = Style::new().fg(palette::FAINT);
    frame.render_widget(
        Paragraph::new(Span::styled("комнаты на сервере", label)),
        header,
    );

    if body.height == 0 {
        return;
    }

    // Список пуст: показываем не пустоту, а причину — «спрашиваю…», «сервер
    // не ответил» или «комнат пока нет».
    if login.rooms.is_empty() {
        let note = login
            .rooms_note
            .clone()
            .unwrap_or_else(|| "нажмите ctrl+r, чтобы обновить".to_string());
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("  {note}"),
                Style::new().fg(palette::DIM),
            )),
            body,
        );
        return;
    }

    let visible = (body.height as usize).min(LOGIN_ROOMS_VISIBLE);
    // Прокрутка держит выбранную строку в окне: без неё выбор ниже восьмой
    // комнаты уезжал бы за край.
    let start = match login.rooms_selected {
        Some(i) if i >= visible => i + 1 - visible,
        _ => 0,
    };

    let lines: Vec<Line> = login
        .rooms
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, room)| {
            let selected = login.rooms_selected == Some(i);
            let (marker, name_style) = if selected {
                (
                    "> ",
                    Style::new()
                        .fg(palette::ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("  ", Style::new())
            };
            let people = if room.users == 1 {
                "1 чел.".to_string()
            } else {
                format!("{} чел.", room.users)
            };
            Line::from(vec![
                Span::styled(marker, name_style),
                Span::styled(room.name.clone(), name_style),
                Span::styled(format!("  ·  {people}"), label),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), body);
}

fn draw_viewer(frame: &mut Frame, viewer: &Viewer, area: Rect, images: &mut Images) {
    let area = centered(
        area,
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    // Под окном просмотра переписки быть не должно — иначе сквозь картинку
    // просвечивает чужой текст.
    frame.render_widget(Clear, area);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {} ", viewer.name))
        // Заодно видно, чем рисуем: если вместо фотографии мозаика, сразу
        // понятно, что терминал графику не поддержал.
        .title_bottom(format!(" esc — закрыть · {} ", images.kind()))
        .border_style(Style::new().fg(palette::ACCENT));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    // Сначала пробуем настоящую графику терминала: kitty, iTerm2, sixel.
    // Полублоки остаются запасным путём — они работают везде, но фотография
    // в них узнаётся с трудом.
    if let ViewerState::Ready(image) = &viewer.state
        && images.render(frame, inner, viewer.id, image)
    {
        return;
    }

    let lines = match &viewer.state {
        ViewerState::Loading => vec![Line::from(Span::styled(
            "загружаю…",
            Style::new().fg(palette::DIM),
        ))],
        ViewerState::Failed(reason) => vec![Line::from(Span::styled(
            reason.clone(),
            Style::new().fg(palette::ERR),
        ))],
        ViewerState::Ready(image) => media::to_lines(image, inner.width, inner.height),
    };

    // Вертикальное центрирование: картинка посреди рамки выглядит опрятнее,
    // чем прижатая к верхнему краю.
    let padding = (inner.height as usize).saturating_sub(lines.len()) / 2;
    let mut padded: Vec<Line> = std::iter::repeat_n(Line::default(), padding).collect();
    padded.extend(lines);

    frame.render_widget(Paragraph::new(padded).alignment(Alignment::Center), inner);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Цвета ников, заданные человеком: ник в нижнем регистре -> цвет.
type Colors = HashMap<String, Color>;

/// Цвет ника: сначала выбор человека, иначе устойчивый цвет по хешу.
fn nick_color(colors: &Colors, nickname: &str, mine: bool) -> Color {
    if let Some(color) = colors.get(&nickname.to_lowercase()) {
        return *color;
    }
    hashed_nick_color(nickname, mine)
}

fn hashed_nick_color(nickname: &str, mine: bool) -> Color {
    if mine {
        return palette::ACCENT;
    }
    let hash = nickname.to_lowercase().bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(byte.into())
    });
    palette::NICKS[hash as usize % palette::NICKS.len()]
}

/// Возвращает строки и карту «номер записи -> номер её первой строки».
/// Место под картинку в ленте: с какой строки и какое вложение.
pub struct Slot {
    pub line: usize,
    pub id: uuid::Uuid,
}

struct Rendered {
    lines: Vec<Line<'static>>,
    offsets: Vec<usize>,
    slots: Vec<Slot>,
}

/// Всё, чем лента раскрашивается: подсветка поиска и выбора, цвета ников,
/// готовые миниатюры и формы волны.
///
/// Собрано в структуру, потому что список рос с каждой возможностью, а восемь
/// позиционных аргументов подряд перепутать проще, чем заметить.
pub struct Decor<'a> {
    pub search: Option<&'a Search>,
    pub picking: Option<usize>,
    pub colors: &'a Colors,
    pub thumbnails: &'a HashMap<uuid::Uuid, Thumbnail>,
    pub waveforms: &'a HashMap<uuid::Uuid, media::Waveform>,
    /// Что играет прямо сейчас и с какого момента — по этому закрашивается
    /// пройденная часть формы волны.
    pub playing: Option<(uuid::Uuid, std::time::Instant)>,
}

fn render_entries(entries: &[Entry], width: usize, decor: &Decor<'_>) -> Rendered {
    let Decor {
        search,
        picking,
        colors,
        thumbnails,
        waveforms,
        playing,
    } = *decor;
    let now = std::time::Instant::now();
    let mut lines = Vec::new();
    let mut offsets = Vec::with_capacity(entries.len());
    let mut slots = Vec::new();
    // Автор и время последней реплики: по ним решается, начинать ли новую
    // группу с подписью.
    let mut previous: Option<(&str, i64)> = None;

    for (index, entry) in entries.iter().enumerate() {
        // Найденное подсвечиваем фоном целиком: выделять подстроку внутри уже
        // перенесённого текста пришлось бы через пересчёт позиций после
        // переноса, а пользы от этого немного.
        let highlight = if picking == Some(index) {
            Some(palette::ACCENT)
        } else {
            match search {
                Some(search) if search.is_match(index) => {
                    if search.current_entry() == Some(index) {
                        Some(palette::MENTION)
                    } else {
                        Some(palette::FAINT)
                    }
                }
                _ => None,
            }
        };

        match entry {
            Entry::Chat {
                from,
                text,
                ts,
                mine,
                mentions_me,
                attachment,
                reply,
                arrived,
                ..
            } => {
                let same_author = previous
                    .is_some_and(|(author, at)| author == from && *ts - at < GROUP_WINDOW_MS);
                if !same_author {
                    if !lines.is_empty() {
                        lines.push(Line::default());
                    }
                    offsets.push(lines.len());
                    lines.push(Line::from(vec![
                        Span::raw(GUTTER),
                        Span::styled(
                            from.clone(),
                            Style::new()
                                .fg(nick_color(colors, from, *mine))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  {}", local_time(*ts)),
                            Style::new().fg(palette::FAINT),
                        ),
                    ]));
                }

                // Скачанную картинку показываем прямо здесь: под неё
                // резервируются пустые строки, а сам рисунок ляжет туда
                // поверх, когда станут известны экранные координаты.
                let inline = attachment.as_ref().and_then(|attachment| {
                    match thumbnails.get(&attachment.id) {
                        Some(Thumbnail::Ready(_)) => Some(attachment.id),
                        _ => None,
                    }
                });

                // Голосовое с разобранной формой волны рисуем графиком, а не
                // строкой: по столбикам видно, где в записи речь, а где пауза,
                // и сколько её осталось.
                let voice = attachment.as_ref().and_then(|attachment| {
                    let wave = waveforms.get(&attachment.id)?;
                    let elapsed = playing
                        .filter(|(id, _)| *id == attachment.id)
                        .map(|(_, since)| now.saturating_duration_since(since));
                    Some(draw_voice(wave, elapsed, &attachment.name))
                });

                // Подпись остаётся в любом случае: по ней видно имя и размер,
                // а полный адрес в ленте рвался бы переносом.
                let body = match attachment {
                    Some(attachment) => {
                        // Значок сразу говорит, что пришло и что с этим делать:
                        // картинку посмотреть, голосовое послушать, файл
                        // сохранить.
                        let mark = match attachment.kind {
                            common::AttachmentKind::Image => "◈",
                            common::AttachmentKind::Audio => "♪",
                            common::AttachmentKind::File => "▤",
                        };
                        let label = format!(
                            "[{mark} {} · {}]",
                            attachment.name,
                            human_size(attachment.size)
                        );
                        if text.is_empty() {
                            label
                        } else {
                            format!("{text} {label}")
                        }
                    }
                    None => text.clone(),
                };
                if same_author {
                    offsets.push(lines.len());
                }

                // Цитата над ответом: видно, к чему он относится, даже если
                // исходное сообщение уехало далеко вверх.
                if let Some(reply) = reply {
                    let quote = format!("▏ {}: {}", reply.nickname, reply.excerpt);
                    for chunk in wrap(&quote, width) {
                        lines.push(Line::from(vec![
                            Span::raw(format!("{GUTTER} ")),
                            Span::styled(chunk, Style::new().fg(palette::FAINT)),
                        ]));
                    }
                }
                let style = if *mentions_me {
                    Style::new().fg(palette::MENTION)
                } else {
                    Style::new()
                };
                match &voice {
                    // График вместо строки: переносить его нельзя, он и так
                    // укладывается в ширину ленты.
                    Some(spans) => {
                        let mut line = vec![Span::raw(format!("{GUTTER} "))];
                        line.extend(spans.iter().cloned());
                        let mut rendered = Line::from(line);
                        if let Some(colour) = highlight {
                            rendered = rendered.style(Style::new().bg(colour));
                        }
                        lines.push(rendered);
                        // Подпись под графиком нужна, только если человек
                        // что-то написал вместе с голосовым.
                        if !text.is_empty() {
                            push_wrapped(
                                &mut lines,
                                style,
                                text,
                                width,
                                highlight,
                                fade(*arrived, now),
                            );
                        }
                    }
                    None => push_wrapped(
                        &mut lines,
                        style,
                        &body,
                        width,
                        highlight,
                        fade(*arrived, now),
                    ),
                }
                if let Some(id) = inline {
                    slots.push(Slot {
                        line: lines.len(),
                        id,
                    });
                    lines.extend(std::iter::repeat_n(Line::default(), THUMB_ROWS as usize));
                }
                previous = Some((from, *ts));
            }
            Entry::System { text, kind } => {
                // Отбивка после чужой реплики: иначе «вошёл в комнату»
                // читается как продолжение чьего-то сообщения.
                if previous.is_some() {
                    lines.push(Line::default());
                }
                previous = None;
                offsets.push(lines.len());
                let style = match kind {
                    SystemKind::Error => Style::new().fg(palette::ERR),
                    SystemKind::Join => Style::new().fg(palette::OK),
                    _ => Style::new().fg(palette::FAINT),
                };
                push_wrapped(
                    &mut lines,
                    style,
                    &format!("· {text}"),
                    width,
                    highlight,
                    None,
                );
            }
        }
    }
    Rendered {
        lines,
        offsets,
        slots,
    }
}

/// Гаснущий цвет метки: чем свежее реплика, тем ярче.
///
/// Три ступени вместо плавного перехода: терминал всё равно перерисовывается
/// по тику, а взгляд ловит именно появление метки, а не её оттенок.
fn fade(arrived: std::time::Instant, now: std::time::Instant) -> Option<Color> {
    let age = now.saturating_duration_since(arrived);
    if age >= FLASH {
        return None;
    }
    let third = FLASH / 3;
    Some(if age < third {
        palette::ACCENT
    } else if age < third * 2 {
        palette::DIM
    } else {
        palette::FAINT
    })
}

/// Кладёт текст с переносом и отступом от края экрана.
fn push_wrapped(
    lines: &mut Vec<Line<'static>>,
    style: Style,
    text: &str,
    width: usize,
    highlight: Option<Color>,
    mark: Option<Color>,
) {
    let style = match highlight {
        Some(color) => style.bg(color).fg(Color::Rgb(20, 20, 24)),
        None => style,
    };
    for (index, chunk) in wrap(text, width).into_iter().enumerate() {
        // Метка стоит только у первой строки: у перенесённого продолжения ей
        // делать нечего, это то же самое сообщение.
        let gutter = match mark {
            Some(color) if index == 0 => {
                Span::styled(format!("{FRESH_MARK} "), Style::new().fg(color))
            }
            _ => Span::raw(format!("{GUTTER} ")),
        };
        lines.push(Line::from(vec![gutter, Span::styled(chunk, style)]));
    }
}

/// Размер файла в привычном виде: «240 КБ» читается, «245760» — нет.
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    match bytes {
        0..KB => format!("{bytes} Б"),
        KB..MB => format!("{} КБ", bytes / KB),
        _ => format!("{},{} МБ", bytes / MB, (bytes % MB) * 10 / MB),
    }
}

fn local_time(ts: i64) -> String {
    // Время приходит в UTC, показываем в часовом поясе того, кто смотрит.
    DateTime::from_timestamp_millis(ts)
        .map(|utc| utc.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_string())
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
fn split_at_width(text: &str, width: usize) -> (&str, &str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use common::{ChatMessage, ServerMessage, UserInfo};
    use ratatui::{Terminal, backend::TestBackend};
    use uuid::Uuid;

    use crate::app::{Action, NetEvent, update};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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

    fn render(state: &mut State, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, state, &mut crate::images::Images::disabled()))
            .unwrap();
        terminal.backend().to_string()
    }

    fn chat(from: &UserInfo, text: &str, ts: i64) -> ServerMessage {
        ServerMessage::Chat(ChatMessage {
            id: Uuid::new_v4(),
            from: from.clone(),
            text: text.into(),
            ts,
            attachment: None,
            reply: None,
        })
    }

    fn bob() -> UserInfo {
        UserInfo {
            id: Uuid::from_u128(1),
            nickname: "bob".into(),
        }
    }

    fn populated() -> State {
        let (mut state, _) = State::new(Some("alice".into()), "general".into());
        update(
            &mut state,
            Action::Net(NetEvent::Message(ServerMessage::Welcome {
                your_id: Uuid::nil(),
                room: "general".into(),
                nickname: "alice".into(),
                users: vec![bob()],
                history: vec![ChatMessage {
                    id: Uuid::from_u128(2),
                    from: bob(),
                    text: "привет".into(),
                    ts: 1_700_000_000_000,
                    attachment: None,
                    reply: None,
                }],
            })),
        );
        state.input = Input::new("ответ");
        state
    }

    #[test]
    fn draws_room_participants_and_input() {
        let mut state = populated();

        let screen = render(&mut state, 60, 16);

        assert!(screen.contains("#general"), "{screen}");
        assert!(screen.contains("привет"), "{screen}");
        // Участники живут в шапке, а не в панели на пятую часть ширины.
        assert!(screen.contains("alice, bob"), "{screen}");
        assert!(screen.contains("ответ"), "{screen}");
        assert!(screen.contains("в сети"), "{screen}");
    }

    #[test]
    fn messages_from_one_author_share_a_single_header() {
        let mut state = populated();
        for text in ["первое", "второе", "третье"] {
            update(
                &mut state,
                Action::Net(NetEvent::Message(chat(&bob(), text, 1_700_000_000_000))),
            );
        }

        let screen = render(&mut state, 60, 24);

        // Подпись над каждой репликой превратила бы переписку в частокол из
        // ников. Их ровно две: над историей и над новой группой — системные
        // строки между ними группу разрывают.
        assert_eq!(headers(&screen, "bob"), 2, "{screen}");
        for text in ["первое", "второе", "третье"] {
            assert!(screen.contains(text), "{screen}");
        }
    }

    /// Считает строки-подписи вида `bob  12:03`.
    fn headers(screen: &str, nickname: &str) -> usize {
        screen
            .lines()
            .filter(|row| {
                row.trim_start_matches(['"', ' '])
                    .starts_with(&format!("{nickname}  "))
            })
            .count()
    }

    #[test]
    fn a_pause_starts_a_new_group() {
        let mut state = populated();
        update(
            &mut state,
            Action::Net(NetEvent::Message(chat(&bob(), "давно", 1_700_000_000_000))),
        );
        update(
            &mut state,
            Action::Net(NetEvent::Message(chat(
                &bob(),
                "недавно",
                1_700_000_000_000 + GROUP_WINDOW_MS + 1,
            ))),
        );

        let screen = render(&mut state, 60, 24);

        // Без паузы обе реплики попали бы под одну подпись; после паузы время
        // снова важно, и подпись возвращается.
        assert_eq!(headers(&screen, "bob"), 3, "{screen}");
    }

    #[test]
    fn history_sticks_to_the_bottom() {
        let mut state = populated();

        let screen = render(&mut state, 60, 16);

        let rows: Vec<&str> = screen.lines().collect();
        let input_top = rows
            .iter()
            .position(|row| row.contains('╭'))
            .expect("рамка ввода не найдена");
        // Прямо над вводом должна быть свежая запись, а не пустота.
        assert!(rows[input_top - 1].contains("help"), "{screen}");
    }

    #[test]
    fn spinner_turns_while_connecting() {
        let (mut state, _) = State::new(Some("alice".into()), "general".into());

        let first = render(&mut state, 60, 16);
        for _ in 0..2 {
            update(&mut state, Action::Tick);
        }
        let second = render(&mut state, 60, 16);

        assert!(first.contains("подключаюсь"), "{first}");
        assert_ne!(first, second, "спиннер не крутится");
    }

    #[test]
    fn viewer_covers_the_chat_while_loading() {
        let mut state = populated();
        state.viewer = Some(Viewer {
            id: Uuid::nil(),
            name: "кот.png".into(),
            state: ViewerState::Loading,
        });

        let screen = render(&mut state, 60, 16);

        assert!(screen.contains("кот.png"), "{screen}");
        assert!(screen.contains("загружаю"), "{screen}");
        // Переписка под окном не должна просвечивать.
        assert!(!screen.contains("привет"), "{screen}");
    }

    #[test]
    fn viewer_explains_a_failure() {
        let mut state = populated();
        state.viewer = Some(Viewer {
            id: Uuid::nil(),
            name: "кот.png".into(),
            state: ViewerState::Failed("сервер ответил: HTTP/1.1 404 Not Found".into()),
        });

        let screen = render(&mut state, 70, 16);

        assert!(screen.contains("404"), "{screen}");
    }

    #[test]
    fn viewer_draws_the_picture() {
        let mut state = populated();
        let image = image::RgbImage::from_pixel(8, 8, image::Rgb([200, 30, 30]));
        state.viewer = Some(Viewer {
            id: Uuid::nil(),
            name: "кот.png".into(),
            state: ViewerState::Ready(Box::new(image)),
        });

        let screen = render(&mut state, 60, 16);

        // Полублоки — единственный способ, работающий во всех терминалах.
        assert!(screen.contains('\u{2580}'), "{screen}");
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
            // Экран входа, в том числе с ошибкой.
            let (mut login, _) = State::new(None, "general".into());
            render(&mut login, width, height);
            update(
                &mut login,
                Action::Net(NetEvent::Fatal {
                    reason: "ник занят".into(),
                }),
            );
            render(&mut login, width, height);

            // Переписка со всеми украшениями сразу.
            let mut chat = populated();
            chat.input = Input::new("длинная строка ввода, которая не влезает целиком");
            render(&mut chat, width, height);

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
    fn feed_reserves_room_for_a_ready_picture() {
        let id = Uuid::from_u128(21);
        let entries = vec![Entry::Chat {
            id,
            from: "bob".into(),
            text: String::new(),
            ts: 1_700_000_000_000,
            mine: false,
            mentions_me: false,
            attachment: Some(common::Attachment {
                id,
                kind: common::AttachmentKind::Image,
                name: "кот.png".into(),
                size: 1024,
                mime: "image/png".into(),
            }),
            reply: None,
            arrived: std::time::Instant::now(),
        }];

        let mut ready = HashMap::new();
        ready.insert(id, Thumbnail::Ready(Box::new(image::RgbImage::new(4, 4))));
        let with_picture = render_entries(&entries, 40, &plain_decor(&ready));

        assert_eq!(with_picture.slots.len(), 1);
        // Под картинку зарезервированы пустые строки: сам рисунок ложится
        // туда поверх, когда становятся известны экранные координаты.
        assert!(with_picture.lines.len() > THUMB_ROWS as usize);

        // Пока картинка не скачана, места под неё не занимаем.
        let mut loading = HashMap::new();
        loading.insert(id, Thumbnail::Loading);
        let without = render_entries(&entries, 40, &plain_decor(&loading));

        assert!(without.slots.is_empty());
        assert!(without.lines.len() < with_picture.lines.len());
    }

    /// Оформление без подсветки: интересны только миниатюры.
    fn plain_decor(thumbnails: &HashMap<uuid::Uuid, Thumbnail>) -> Decor<'_> {
        static EMPTY_COLORS: std::sync::LazyLock<Colors> = std::sync::LazyLock::new(Colors::new);
        static EMPTY_WAVES: std::sync::LazyLock<HashMap<uuid::Uuid, media::Waveform>> =
            std::sync::LazyLock::new(HashMap::new);
        Decor {
            search: None,
            picking: None,
            colors: &EMPTY_COLORS,
            thumbnails,
            waveforms: &EMPTY_WAVES,
            playing: None,
        }
    }

    #[test]
    fn browser_shows_files_and_marks_media() {
        let mut state = populated();
        state.browser = Some(Browser {
            dir: std::path::PathBuf::from("C:/фото"),
            entries: vec![
                crate::files::FileEntry {
                    name: "..".into(),
                    path: std::path::PathBuf::from("C:/"),
                    is_dir: true,
                    size: 0,
                    media: false,
                },
                crate::files::FileEntry {
                    name: "кот.png".into(),
                    path: std::path::PathBuf::from("C:/фото/кот.png"),
                    is_dir: false,
                    size: 240 * 1024,
                    media: true,
                },
            ],
            selected: 1,
            filter: Input::default(),
            loading: false,
            error: None,
        });

        let screen = render(&mut state, 80, 24);

        assert!(screen.contains("кот.png"), "{screen}");
        assert!(screen.contains("240 КБ"), "{screen}");
        assert!(screen.contains("отсев"), "{screen}");
        assert!(screen.contains("enter выбрать"), "{screen}");
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

    fn voice_line(playing: Option<std::time::Duration>) -> String {
        let wave = media::Waveform {
            bars: vec![1, 4, 8, 6, 2, 7, 3, 5],
            millis: 8_000,
        };
        draw_voice(&wave, playing, "голосовое.wav")
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    #[test]
    fn a_voice_is_drawn_as_a_graph_with_its_length() {
        let line = voice_line(None);

        // Столбики разной высоты — по ним и видно, где речь, а где пауза.
        assert!(line.contains('█'), "{line}");
        assert!(line.contains('▁'), "{line}");
        // Длительность из заголовка, а не «столько-то килобайт».
        assert!(line.contains("0:08"), "{line}");
        assert!(line.contains("голосовое.wav"), "{line}");
    }

    #[test]
    fn playback_paints_the_part_already_heard() {
        let wave = media::Waveform {
            bars: vec![4; 8],
            millis: 8_000,
        };

        // На середине записи ровно половина столбиков должна быть акцентной:
        // граница между цветами и есть указатель воспроизведения.
        let spans = draw_voice(&wave, Some(std::time::Duration::from_secs(4)), "г.wav");
        let accented = spans
            .iter()
            .skip(1) // первый — значок ♪
            .take(8)
            .filter(|span| span.style.fg == Some(palette::ACCENT))
            .count();

        assert_eq!(accented, 4, "закрашено не половина: {accented}");
    }

    #[test]
    fn a_voice_that_is_not_playing_is_not_painted() {
        let wave = media::Waveform {
            bars: vec![4; 8],
            millis: 8_000,
        };

        let spans = draw_voice(&wave, None, "г.wav");
        let accented = spans
            .iter()
            .skip(1)
            .take(8)
            .filter(|span| span.style.fg == Some(palette::ACCENT))
            .count();

        assert_eq!(accented, 0, "график закрашен, хотя ничего не играет");
    }

    #[test]
    fn draws_the_login_screen() {
        let (mut state, _) = State::new(None, "general".into());

        let screen = render(&mut state, 60, 16);

        assert!(screen.contains("чат"), "{screen}");
        assert!(screen.contains("ник"), "{screen}");
        assert!(screen.contains("general"), "{screen}");
        assert!(screen.contains("enter"), "{screen}");
    }

    #[test]
    fn login_screen_shows_the_error() {
        let (mut state, _) = State::new(None, "general".into());
        update(
            &mut state,
            Action::Net(NetEvent::Fatal {
                reason: "ник занят".into(),
            }),
        );

        let screen = render(&mut state, 60, 16);

        assert!(screen.contains("ник занят"), "{screen}");
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        // Окно можно ужать до чего угодно, и это не повод падать посреди сессии.
        for (width, height) in [(1, 1), (3, 2), (5, 4), (20, 5)] {
            let mut chat = populated();
            render(&mut chat, width, height);

            let (mut login, _) = State::new(None, "general".into());
            render(&mut login, width, height);
        }
    }

    #[test]
    fn long_input_scrolls_to_keep_the_cursor_visible() {
        let mut state = populated();
        state.input = Input::new("я".repeat(200));

        let screen = render(&mut state, 40, 10);

        // Курсор упёрся бы в правый край и уехал за рамку, если бы строка
        // не прокручивалась вместе с ним.
        assert!(screen.contains("я"), "{screen}");
    }
}
