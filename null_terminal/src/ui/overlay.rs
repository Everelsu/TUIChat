//! Окна поверх переписки: справка, обзор файлов и просмотр картинки.
//!
//! Все три перекрывают ленту целиком, а не раздвигают её. Раздвинутая лента
//! теряет то место, ради которого окно и открыли: пока выбираешь файл, важно
//! видеть файлы, а не сообщения.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{Browser, Viewer, ViewerState},
    images::Images,
    media,
    theme::{self, Theme},
    ui::{GUTTER, centered, draw_field, human_size, shorten_left, strong},
};

/// Рамка окна: скруглённая, с подписью сверху и подсказкой снизу.
///
/// Подсказка внизу рамки, а не отдельной строкой: она относится к окну и
/// уезжает вместе с ним, а места не занимает вовсе.
fn window(frame: &mut Frame, area: Rect, title: &str, footer: &str, theme_: Theme) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::mix(theme_.primary(), theme::LINE, 0.3)))
        .title(Line::from(theme::gradient_bold(
            &format!(" {title} "),
            theme_.primary(),
            theme_.secondary(),
        )))
        .title_bottom(Line::from(Span::styled(
            format!(" {footer} "),
            Style::new().fg(theme::MUTED),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

pub fn help(frame: &mut Frame, theme_: Theme, scroll: usize, area: Rect) {
    let width = 78.min(area.width);
    let height = (crate::app::HELP.len() as u16 + 6).min(area.height);
    let area = centered(area, width, height);

    let mut lines: Vec<Line> = crate::app::HELP
        .iter()
        .map(|entry| {
            // Саму команду выделяем: глаз ищет в такой справке именно её.
            match entry.split_once(" — ") {
                Some((key, what)) => Line::from(vec![
                    Span::styled(
                        // Пробел в конце обязателен: у длинной команды колонка
                        // кончается, и описание слиплось бы с ней.
                        format!("{GUTTER}{key:<24} "),
                        Style::new()
                            .fg(theme_.primary())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(what.to_string(), Style::new().fg(theme::SUBTLE)),
                ]),
                None => Line::from(Span::styled(
                    format!("{GUTTER}{entry}"),
                    Style::new().fg(theme::MUTED),
                )),
            }
        })
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("{GUTTER}tab ник и команда · ctrl+o файл · ctrl+r ответ · ctrl+f поиск"),
        Style::new().fg(theme::LINE),
    )));
    lines.push(Line::from(Span::styled(
        // Перехват мыши нужен ради прокрутки колесом и ломает выделение.
        // Об этом надо сказать: иначе выглядит как поломка.
        format!("{GUTTER}выделять текст мышью — с зажатым shift"),
        Style::new().fg(theme::LINE),
    )));

    // Сколько строк не влезло. Клавиш и команд вместе больше тридцати, а окно
    // терминала бывает и в двадцать строк: обрезать список молча нельзя —
    // человек решит, что остального просто нет.
    let inner_height = area.height.saturating_sub(2) as usize;
    let hidden = lines.len().saturating_sub(inner_height);
    let from = scroll.min(hidden);
    let footer = if hidden == 0 {
        "любая клавиша — закрыть".to_string()
    } else if from == hidden {
        "↑ выше · любая клавиша — закрыть".to_string()
    } else {
        format!("↓ ещё {} · любая клавиша — закрыть", hidden - from)
    };

    let inner = window(frame, area, "что умеет чат", &footer, theme_);
    if inner.height == 0 {
        return;
    }
    frame.render_widget(Paragraph::new(lines.split_off(from)), inner);
}

pub fn browser(frame: &mut Frame, browser: &Browser, theme_: Theme, area: Rect) {
    let area = centered(area, 74.min(area.width), 22.min(area.height));
    let title = shorten_left(
        &browser.dir.to_string_lossy(),
        area.width.saturating_sub(6) as usize,
    );
    let inner = window(
        frame,
        area,
        &title,
        "enter выбрать · ← наверх · esc отмена",
        theme_,
    );
    if inner.height < 2 {
        return;
    }

    let [list_area, filter_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    let visible = browser.visible();
    let lines: Vec<Line> = if browser.loading {
        vec![Line::from(Span::styled(
            format!("{GUTTER}читаю…"),
            Style::new().fg(theme::MUTED),
        ))]
    } else if let Some(error) = &browser.error {
        vec![Line::from(Span::styled(
            format!("{GUTTER}{error}"),
            Style::new().fg(theme::ERR),
        ))]
    } else if visible.is_empty() {
        vec![Line::from(Span::styled(
            format!("{GUTTER}ничего не нашлось"),
            Style::new().fg(theme::MUTED),
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
                    theme_.secondary()
                } else {
                    theme::TEXT
                };
                let mut style = Style::new().fg(color);
                if chosen {
                    style = style.bg(theme_.primary()).fg(theme::INK);
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

    draw_field(
        frame,
        &browser.filter,
        filter_area,
        &[
            Span::raw(" "),
            strong("⌕", theme_.primary()),
            Span::raw(" "),
        ],
    );
}

pub fn viewer(frame: &mut Frame, viewer: &Viewer, theme_: Theme, area: Rect, images: &mut Images) {
    let area = centered(
        area,
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    let inner = window(
        frame,
        area,
        &viewer.name,
        // Заодно видно, чем рисуем: если вместо фотографии мозаика, сразу
        // понятно, что терминал графику не поддержал.
        &format!("esc — закрыть · {}", images.kind()),
        theme_,
    );
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
        ViewerState::Loading => vec![Line::from(vec![
            Span::styled(theme::spinner(0), Style::new().fg(theme_.primary())),
            Span::styled(" загружаю…", Style::new().fg(theme::SUBTLE)),
        ])],
        ViewerState::Failed(reason) => vec![Line::from(Span::styled(
            reason.clone(),
            Style::new().fg(theme::ERR),
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

#[cfg(test)]
mod tests {
    use crate::app::{Browser, Input, Viewer, ViewerState};
    use crate::ui::chat::tests::populated;
    use crate::ui::tests::render;
    use uuid::Uuid;

    #[test]
    fn help_lists_the_keys() {
        let mut state = populated();
        state.help = true;

        let screen = render(&mut state, 80, 24);

        assert!(screen.contains("что умеет чат"), "{screen}");
        assert!(screen.contains("F2"), "{screen}");
        assert!(screen.contains("закрыть"), "{screen}");
    }

    #[test]
    fn help_says_how_much_is_left_below() {
        let mut state = populated();
        state.help = true;

        // В низкое окно список не влезает — об этом должно быть сказано,
        // иначе человек решит, что остального просто нет.
        let short = render(&mut state, 80, 20);
        assert!(short.contains("ещё"), "{short}");

        state.help_scroll = 100;
        let bottom = render(&mut state, 80, 20);
        assert!(bottom.contains("выше"), "{bottom}");
        // Долистав до конца, видно последнюю строку справки.
        assert!(bottom.contains("shift"), "{bottom}");
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
        assert!(screen.contains("enter выбрать"), "{screen}");
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
}
