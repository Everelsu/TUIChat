//! Экран переписки: полоса состояния, лента, поле ввода и подсказка.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{PickMode, Search, State, Status, Thumbnail, Viewport},
    images::Images,
    theme,
    ui::{GUTTER, caption, draw_field, feed, hints, shorten, strong, widgets},
};

/// Ширина колонки с людьми и та ширина окна, начиная с которой она уместна.
const SIDEBAR: u16 = 22;
const SIDEBAR_NEEDS: u16 = 72;

pub fn draw(frame: &mut Frame, state: &mut State, images: &mut Images) {
    // Строка с цитатой появляется, только когда ответ взведён: постоянно
    // держать под неё место жалко.
    let reply_height = u16::from(state.replying.is_some());
    let [header, body, reply, input, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(reply_height),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Колонку с людьми показываем, только когда ленте есть чем поделиться:
    // в узком окне столбик имён отнимает у разговора треть ширины.
    let (messages, people) = if state.sidebar && body.width >= SIDEBAR_NEEDS {
        let [messages, people] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(SIDEBAR)]).areas(body);
        (messages, Some(people))
    } else {
        (body, None)
    };

    draw_header(frame, state, header);
    draw_messages(frame, state, messages, images);
    if let Some(area) = people {
        draw_people(frame, state, area);
    }
    draw_reply_bar(frame, state, reply);
    draw_input(frame, state, input);
    draw_hint(frame, state, hint);
}

/// Полоса состояния: комната, кто в ней и есть ли связь.
///
/// Единственное место, где мы заливаем фон: полоса должна читаться как
/// отдельная часть окна, а не как первая строка переписки.
fn draw_header(frame: &mut Frame, state: &State, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Block::new().style(Style::new().bg(theme::SURFACE)), area);

    // Справа собираем первым: по его ширине понятно, сколько осталось слева.
    let mut right = Vec::new();
    right.extend(status_spans(state));
    right.push(Span::styled("  ", Style::new()));
    right.push(Span::styled(
        format!("{} ", chrono::Local::now().format("%H:%M")),
        Style::new().fg(theme::MUTED),
    ));
    let right_width: usize = right.iter().map(|span| span.content.width()).sum();

    let room = shorten(&state.room, 24);
    let mut left = vec![Span::raw(GUTTER)];
    left.extend(theme::pill(
        &format!(" ◆ {room} "),
        state.theme.primary(),
        theme::INK,
    ));

    // Люди в шапке, а не в отдельной панели: панель отнимала пятую часть
    // ширины ради столбика имён, а он нужен не всегда.
    let used: usize = left.iter().map(|span| span.content.width()).sum();
    let free = (area.width as usize).saturating_sub(used + right_width + 2);
    if free > 4 && !state.users.is_empty() {
        left.push(Span::raw("  "));
        left.extend(widgets::dots(
            state.users.len(),
            theme::mix(theme::OK, theme::SUBTLE, 0.3),
        ));
        let names = people_summary(state, free.saturating_sub(8));
        if !names.is_empty() {
            left.push(Span::styled(
                format!("  {names}"),
                Style::new().fg(theme::MUTED),
            ));
        }
    }

    frame.render_widget(Paragraph::new(Line::from(left)), area);
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        area,
    );
}

/// Имена участников, обрезанные под доступную ширину.
fn people_summary(state: &State, width: usize) -> String {
    if state.users.is_empty() || width < 6 {
        return String::new();
    }

    let names: Vec<&str> = state
        .users
        .iter()
        .map(|user| user.nickname.as_str())
        .collect();
    let full = names.join(", ");
    if full.width() <= width {
        return full;
    }
    let short = format!("{} человек", names.len());
    if short.width() <= width {
        return short;
    }
    String::new()
}

fn status_spans(state: &State) -> Vec<Span<'static>> {
    match &state.status {
        Status::Connecting { .. } => vec![
            Span::styled(theme::spinner(state.tick), Style::new().fg(theme::WARN)),
            Span::styled(" подключаюсь", Style::new().fg(theme::WARN)),
        ],
        Status::Online => vec![
            widgets::status_dot(theme::OK, true, state.tick),
            Span::styled(" в сети", Style::new().fg(theme::OK)),
        ],
        Status::Reconnecting { retry_at, .. } => {
            let left = retry_at.saturating_duration_since(std::time::Instant::now());
            vec![
                widgets::status_dot(theme::ERR, false, state.tick),
                Span::styled(
                    format!(" нет связи · {}с", left.as_secs() + 1),
                    Style::new().fg(theme::ERR),
                ),
            ]
        }
    }
}

/// Колонка справа: кто в комнате и куда мы подключены.
///
/// Держится на ctrl+p и по умолчанию спрятана: разговору ширина нужнее, чем
/// столбик имён, — но когда людей много, без списка непонятно, кому пишешь.
fn draw_people(frame: &mut Frame, state: &State, area: Rect) {
    if area.width < 6 || area.height == 0 {
        return;
    }
    let theme_ = state.theme;
    // Две колонки на разделитель и воздух за ним: имя, прижатое к черте,
    // читается как её продолжение.
    let inner = Rect {
        x: area.x + 2,
        width: area.width - 2,
        ..area
    };

    let typing = state.typing_now();
    let mut lines = vec![Line::from(caption(
        "люди",
        theme_.primary(),
        theme_.secondary(),
    ))];

    for user in &state.users {
        let mine = state.me == Some(user.id);
        let color = feed::nick_color(&state.colors, &user.nickname, mine, theme_);
        let mut spans = vec![
            Span::styled("● ", Style::new().fg(color)),
            Span::styled(
                shorten(&user.nickname, inner.width as usize - 5),
                Style::new().fg(if mine { theme::TEXT } else { theme::SUBTLE }),
            ),
        ];
        // Печатающий помечен прямо в списке: строка внизу называет одного-двух,
        // а здесь видно всех сразу.
        if typing.contains(&user.nickname.as_str()) {
            spans.push(Span::styled(" ✎", Style::new().fg(theme_.secondary())));
        } else if mine {
            spans.push(Span::styled(" вы", Style::new().fg(theme::MUTED)));
        }
        lines.push(Line::from(spans));
    }
    if state.users.is_empty() {
        lines.push(Line::from(Span::styled(
            "пока никого",
            Style::new().fg(theme::MUTED),
        )));
    }

    lines.push(Line::default());
    lines.push(Line::from(caption(
        "сервер",
        theme_.primary(),
        theme_.secondary(),
    )));
    let where_ = server_label(&state.server);
    lines.push(Line::from(Span::styled(
        if where_.is_empty() {
            "не подключены".to_string()
        } else {
            shorten(where_, inner.width as usize)
        },
        Style::new().fg(theme::MUTED),
    )));

    // Разделитель вертикальной чертой: без него колонка сливается с лентой.
    let rail: Vec<Line> = (0..area.height)
        .map(|_| Line::from(Span::styled("│", Style::new().fg(theme::LINE))))
        .collect();
    frame.render_widget(Paragraph::new(rail), Rect { width: 1, ..area });
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Адрес сервера без служебных частей: `ws://` и `/ws` человеку ничего не
/// говорят, а место занимают.
fn server_label(server: &str) -> &str {
    server
        .trim_start_matches("wss://")
        .trim_start_matches("ws://")
        .trim_end_matches("/ws")
}

fn draw_messages(frame: &mut Frame, state: &mut State, area: Rect, images: &mut Images) {
    let width = area.width.saturating_sub(2) as usize;
    let feed::Rendered {
        lines,
        offsets,
        slots,
    } = feed::render_entries(
        &state.entries,
        width,
        &feed::Decor {
            search: state.search.as_ref(),
            picking: state.picking.map(|pick| pick.index),
            colors: &state.colors,
            thumbnails: &state.thumbnails,
            waveforms: &state.waveforms,
            playing: state.playing_voice,
            theme: state.theme,
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
    let padding = height.saturating_sub(end - start);
    let mut visible: Vec<Line> = std::iter::repeat_n(Line::default(), padding).collect();
    visible.extend_from_slice(&lines[start..end]);

    frame.render_widget(Paragraph::new(visible), area);
    draw_thumbnails(frame, state, area, images, &slots, start, end, padding);

    // Прокрутка вверх — состояние, из которого надо уметь выйти: пока лента
    // не внизу, об этом говорит бегунок у правого края.
    if state.scrollback > 0 && area.width > 6 && area.height > 2 {
        let hint = format!(" ↓ {} строк ниже ", state.scrollback);
        let mark = Rect {
            x: area.x + area.width.saturating_sub(hint.width() as u16 + 1),
            y: area.y,
            width: hint.width() as u16,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(theme::pill(
                hint.trim(),
                theme::SURFACE,
                theme::SUBTLE,
            ))),
            mark,
        );
    }
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
    slots: &[feed::Slot],
    start: usize,
    end: usize,
    padding: usize,
) {
    for slot in slots {
        // Рисуем только целиком поместившиеся: обрезанная картинка меняет
        // высоту на каждый шаг прокрутки, а от этого она перекодируется
        // заново каждый кадр — и лента начинает дёргаться.
        let bottom = slot.line + feed::THUMB_ROWS as usize;
        if slot.line < start || bottom > end {
            continue;
        }

        let Some(Thumbnail::Ready(image)) = state.thumbnails.get(&slot.id) else {
            continue;
        };
        let rect = Rect {
            x: area.x + 3,
            y: area.y + (padding + slot.line - start) as u16,
            width: feed::THUMB_COLS.min(area.width.saturating_sub(3)),
            height: feed::THUMB_ROWS,
        };
        images.render(frame, rect, slot.id, image);
    }
}

fn draw_reply_bar(frame: &mut Frame, state: &State, area: Rect) {
    let Some(target) = &state.replying else {
        return;
    };
    if area.height == 0 {
        return;
    }

    let line = Line::from(vec![
        Span::styled(
            format!("{GUTTER} ↩ "),
            Style::new().fg(state.theme.primary()),
        ),
        Span::styled(
            format!("{}: ", target.nickname),
            Style::new().fg(feed::nick_color(
                &state.colors,
                &target.nickname,
                false,
                state.theme,
            )),
        ),
        Span::styled(target.excerpt.clone(), Style::new().fg(theme::MUTED)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_input(frame: &mut Frame, state: &State, area: Rect) {
    // Во время поиска поле ввода занято запросом: искать и писать
    // одновременно всё равно не получится.
    if let Some(search) = &state.search {
        draw_search(frame, search, state, area);
        return;
    }

    // Рамка окрашена по связи: пока её нет, набранное никуда не уйдёт, и
    // сказать об этом лучше рамкой, чем строчкой, которую надо прочитать.
    let border = if state.is_online() {
        theme::mix(state.theme.primary(), theme::LINE, 0.35)
    } else {
        theme::ERR
    };
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border));

    // Счётчик появляется, только когда до потолка недалеко: постоянный
    // «0/2000» ничего не сообщает.
    let limit = common::validate::MAX_TEXT_CHARS;
    if state.input.len() * 10 > limit * 6 {
        let left = limit.saturating_sub(state.input.len());
        let color = if left < 40 { theme::ERR } else { theme::MUTED };
        block = block.title_bottom(
            Line::from(Span::styled(format!(" {left} "), Style::new().fg(color)))
                .alignment(Alignment::Right),
        );
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let prompt = vec![
        Span::raw(" "),
        strong("❯", state.theme.primary()),
        Span::raw(" "),
    ];
    // Пустое поле подписано: человеку, открывшему чат впервые, должно быть
    // видно, куда писать.
    if state.input.is_empty() && state.replying.is_none() {
        let mut spans = prompt.clone();
        spans.push(Span::styled(
            "напишите сообщение",
            Style::new().fg(theme::LINE),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), inner);
        frame.set_cursor_position(ratatui::layout::Position::new(inner.x + 3, inner.y));
        return;
    }
    draw_field(frame, &state.input, inner, &prompt);
}

fn draw_search(frame: &mut Frame, search: &Search, state: &State, area: Rect) {
    let counter = if search.query.is_empty() {
        String::new()
    } else if search.matches.is_empty() {
        " ничего ".to_string()
    } else {
        format!(" {} из {} ", search.current + 1, search.matches.len())
    };

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::MENTION))
        .title_bottom(Line::from(Span::styled(
            counter,
            Style::new().fg(theme::MENTION),
        )));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let _ = state;
    draw_field(
        frame,
        &search.query,
        inner,
        &[Span::raw(" "), strong("⌕", theme::MENTION), Span::raw(" ")],
    );
}

fn draw_hint(frame: &mut Frame, state: &State, area: Rect) {
    if area.height == 0 {
        return;
    }
    // Своя работа важнее чужой: пока что-то качается, показываем именно это.
    if let Some(busy) = &state.busy {
        let mut spans = vec![
            Span::raw(format!("{GUTTER} ")),
            Span::styled(
                theme::spinner(state.tick),
                Style::new().fg(state.theme.primary()),
            ),
            Span::styled(format!(" {busy} "), Style::new().fg(theme::SUBTLE)),
        ];
        // Бегунок за текстом: по нему видно, что клиент занят, а не завис.
        let used: usize = spans.iter().map(|span| span.content.width()).sum();
        let rest = (area.width as usize).saturating_sub(used + 2);
        spans.extend(widgets::runner(rest.min(24), state.theme, state.tick));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    // Пока кто-то печатает, подсказка уступает место живой информации:
    // она всё равно повторяется от кадра к кадру, а это — новость.
    let typing = state.typing_now();
    if !typing.is_empty() && state.viewer.is_none() && state.search.is_none() {
        let who = match typing.as_slice() {
            [one] => format!("{one} печатает"),
            [one, two] => format!("{one} и {two} печатают"),
            many => format!("{} человек печатают", many.len()),
        };
        // Точки бегут волной: одна яркая, соседние гаснут — так видно, что
        // строка живая, а не примёрзла с прошлого сообщения.
        let mut spans = vec![
            Span::raw(format!("{GUTTER} ")),
            Span::styled(who, Style::new().fg(theme::SUBTLE)),
            Span::raw(" "),
        ];
        let head = (state.tick / 3) % 3;
        for at in 0..3u64 {
            let light = if at == head { 1.0 } else { 0.25 };
            spans.push(Span::styled(
                "·",
                Style::new().fg(theme::mix(theme::LINE, state.theme.secondary(), light)),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let pairs: &[(&str, &str)] = if state.viewer.is_some() {
        &[("esc", "закрыть картинку")]
    } else if let Some(pick) = state.picking {
        match pick.mode {
            PickMode::Reply => &[
                ("↑↓", "сообщение"),
                ("enter", "ответить"),
                ("esc", "отмена"),
            ],
            // Здесь важно назвать все действия: человек пришёл сюда именно
            // потому, что ему нужно не последнее вложение.
            PickMode::Attachment => &[
                ("↑↓", "вложение"),
                ("enter", "открыть"),
                ("f3", "играть"),
                ("f5", "сохранить"),
            ],
        }
    } else if state.replying.is_some() {
        &[("enter", "отправить ответ"), ("esc", "снять цитату")]
    } else if state.search.is_some() {
        &[("enter", "следующее"), ("esc", "закрыть поиск")]
    } else {
        // Подсказка называет то, ради чего чат и открывают, и называет
        // клавишами, а не командами: человеку, который не пишет код, строка
        // со слэшем ничего не говорит.
        &[
            ("f2", "голосовое"),
            ("f4", "файл"),
            ("esc", "меню"),
            ("f1", "всё остальное"),
        ]
    };

    let mut line = hints(pairs, state.theme.primary());
    line.spans.insert(0, Span::raw(format!("{GUTTER} ")));
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::{Action, Input, NetEvent, update};
    use crate::ui::tests::render;
    use common::{ChatMessage, ServerMessage, UserInfo};
    use uuid::Uuid;

    pub(crate) fn bob() -> UserInfo {
        UserInfo {
            id: Uuid::from_u128(1),
            nickname: "bob".into(),
        }
    }

    /// Состояние с одной чужой репликой и набранным ответом.
    pub(crate) fn populated() -> State {
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
                upload_limit: common::validate::MAX_UPLOAD_BYTES as u64,
            })),
        );
        state.input = Input::new("ответ");
        state
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

    #[test]
    fn draws_the_room_the_people_and_the_input() {
        let mut state = populated();

        let screen = render(&mut state, 60, 16);

        assert!(screen.contains("general"), "{screen}");
        assert!(screen.contains("привет"), "{screen}");
        // Участники живут в шапке, а не в панели на пятую часть ширины.
        assert!(screen.contains("alice, bob"), "{screen}");
        assert!(screen.contains("ответ"), "{screen}");
        assert!(screen.contains("в сети"), "{screen}");
    }

    #[test]
    fn an_empty_input_says_what_to_do_with_it() {
        let mut state = populated();
        state.input = Input::default();

        let screen = render(&mut state, 60, 16);

        assert!(screen.contains("напишите сообщение"), "{screen}");
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
    fn the_people_column_lists_everyone() {
        let mut state = populated();
        state.sidebar = true;
        update(
            &mut state,
            Action::Net(NetEvent::Message(chat(&bob(), "тут", 1_700_000_000_000))),
        );

        let screen = render(&mut state, 90, 16);

        assert!(screen.contains("Л Ю Д И"), "{screen}");
        assert!(screen.contains("alice"), "{screen}");
        assert!(screen.contains("bob"), "{screen}");
    }

    #[test]
    fn the_people_column_stays_away_in_a_narrow_window() {
        let mut state = populated();
        state.sidebar = true;

        let screen = render(&mut state, 60, 16);

        // Ленте ширина нужнее: в узком окне колонка не показывается вовсе.
        assert!(!screen.contains("Л Ю Д И"), "{screen}");
    }

    #[test]
    fn spinner_turns_while_connecting() {
        let (mut state, _) = State::new(Some("alice".into()), "general".into());
        state.screen = crate::app::Screen::Chat;

        let first = render(&mut state, 60, 16);
        for _ in 0..2 {
            update(&mut state, Action::Tick);
        }
        let second = render(&mut state, 60, 16);

        assert!(first.contains("подключаюсь"), "{first}");
        assert_ne!(first, second, "спиннер не крутится");
    }

    #[test]
    fn scrolled_history_says_how_far_it_went() {
        let mut state = populated();
        for at in 0..40 {
            update(
                &mut state,
                Action::Net(NetEvent::Message(chat(
                    &bob(),
                    &format!("реплика {at}"),
                    1_700_000_000_000 + at * 200_000,
                ))),
            );
        }
        // Рисуем один раз, чтобы стали известны размеры ленты.
        render(&mut state, 60, 16);
        state.scrollback = 12;

        let screen = render(&mut state, 60, 16);

        assert!(screen.contains("строк ниже"), "{screen}");
    }

    #[test]
    fn long_input_scrolls_to_keep_the_cursor_visible() {
        let mut state = populated();
        state.input = Input::new("я".repeat(700));

        let screen = render(&mut state, 40, 10);

        // Курсор упёрся бы в правый край и уехал за рамку, если бы строка
        // не прокручивалась вместе с ним.
        assert!(screen.contains("я"), "{screen}");
        // У длинной строки виден остаток до потолка: 1000 минус набранное.
        assert!(screen.contains("300"), "{screen}");
    }
}
