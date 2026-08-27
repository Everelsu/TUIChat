//! Главный экран: заголовок, вкладки и то, что на них.
//!
//! Вкладки собраны так, чтобы человеку не приходилось ничего знать заранее:
//! «войти» спрашивает ник и показывает живые комнаты, «поднять» заводит
//! сервер прямо здесь, «вид» меняет оформление, «справка» перечисляет
//! клавиши. Всё это раньше жило в командах со слэшем — то есть нигде.

use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Alignment, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{Field, HomeTab, Login, Setting, SoundSetting, State},
    theme::{self, Theme},
    ui::{
        caption, columns, field_view, hints, pad, shorten, strong,
        widgets::{self, Tab},
    },
};

/// Сколько едет содержимое при переключении вкладки.
///
/// Четверть секунды: меньше — переход не читается, больше — начинает
/// раздражать того, кто листает вкладки подряд.
const SLIDE: Duration = Duration::from_millis(260);

/// На сколько колонок содержимое сдвинуто в начале переезда.
const SLIDE_SHIFT: f32 = 5.0;

/// Ширина главного окна. Шире делать нечего: строки формы длиннее 80 колонок
/// читаются хуже, а на широком мониторе окно просто повисает посередине.
const PANEL: u16 = 82;

/// Сколько строк рамка вкладок берёт себе: две на сами вкладки и две на
/// верх и низ.
const FRAME: u16 = 4;

/// Самая высокая вкладка — «поднять»: три поля, три пояснения и кнопка.
/// По ней считается положение окна, чтобы при переключении разделов оно не
/// прыгало вверх-вниз.
const TALLEST: u16 = 14;

/// Высота всей полосы вкладок по самой длинной из них.
const BODY: u16 = TALLEST + FRAME;

/// Заголовок блочными буквами: пять строк логотипа и отбивка под ними.
const HEAD: u16 = 6;

pub fn draw(frame: &mut Frame, login: &Login, state: &State, area: Rect) {
    if area.width < 8 || area.height < 3 {
        // На таком экране не поместится и одна строка формы. Показываем
        // хотя бы, что программа жива и куда её растянуть.
        frame.render_widget(
            Paragraph::new(Line::from(theme::gradient_bold(
                "чат",
                state.theme.primary(),
                state.theme.secondary(),
            ))),
            area,
        );
        return;
    }

    let theme_ = state.theme;
    let foot = u16::from(area.height >= 8);
    // Сначала место для содержимого, заголовок — из остатка. Наоборот было бы
    // красиво ровно до того момента, когда буквы съедают кнопку «войти».
    let spare = area.height.saturating_sub(BODY + foot);
    let head = if spare >= HEAD && area.width >= widgets::LOGO_WIDTH + 6 {
        HEAD
    } else if spare >= 2 {
        2
    } else {
        0
    };
    let wanted = head + BODY + foot;
    let height = wanted.min(area.height);
    let width = PANEL.min(area.width);
    let panel = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };

    if head > 0 {
        draw_head(
            frame,
            state,
            Rect {
                height: head,
                ..panel
            },
            head == HEAD,
        );
    }
    let body = Rect {
        y: panel.y + head,
        height: panel.height - head - foot,
        ..panel
    };
    let tabs: Vec<Tab> = HomeTab::ALL
        .iter()
        .map(|tab| Tab {
            icon: tab.icon(),
            title: tab.title(),
        })
        .collect();

    // Содержимое собираем до рамки: по нему видно, какой высоты она должна
    // быть. Верх окна при этом не двигается — он считается по самой высокой
    // вкладке, — а низ подтягивается к последней строке: пять пустых строк
    // под короткой вкладкой выглядят как оборванная форма.
    let room = body.width.saturating_sub(4) as usize;
    let (lines, cursor) = tab_lines(login, state, room);
    let content = (lines.len() as u16).clamp(4, body.height.saturating_sub(FRAME).max(4));
    let body = Rect {
        height: (content + FRAME).min(body.height),
        ..body
    };

    let inner = widgets::tabbed(frame, body, &tabs, login.tab.index(), theme_);
    draw_tab(frame, login, inner, lines, cursor);

    if foot == 1 {
        // Подсказка идёт следом за рамкой, а не по нижнему краю окна: между
        // ними не должно зиять пустое место.
        let footer = Rect {
            y: (body.y + body.height).min(panel.y + panel.height - 1),
            height: 1,
            ..panel
        };
        frame.render_widget(
            Paragraph::new(footer_hints(login, theme_, footer.width)),
            footer,
        );
    }
}

/// Заголовок: блочные буквы с бегущим бликом или одна строка, когда экран мал.
fn draw_head(frame: &mut Frame, state: &State, area: Rect, big: bool) {
    let theme_ = state.theme;
    // Подписи под логотипом нет намеренно: она повторяла бы то, что и так
    // написано на вкладках, а место занимала.
    let lines = if big {
        widgets::logo(theme_, state.tick)
    } else {
        vec![Line::from(theme::gradient_bold(
            "null_terminal",
            theme_.primary(),
            theme_.secondary(),
        ))]
    };

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

fn footer_hints(login: &Login, theme_: Theme, width: u16) -> Line<'static> {
    // В узком окне подсказка всё равно обрежется на полуслове — лучше
    // назвать две клавиши целиком, чем пять кусками.
    if width < 60 {
        let mut line = hints(&[("tab", "раздел"), ("enter", "дальше")], theme_.primary());
        line.spans.insert(0, Span::raw(" "));
        return line;
    }
    let pairs: &[(&str, &str)] = match login.tab {
        HomeTab::Join => &[
            ("tab", "раздел"),
            ("↑↓", "выбор"),
            ("enter", "войти"),
            ("ctrl+r", "обновить"),
            ("ctrl+q", "выход"),
        ],
        HomeTab::Host => &[
            ("tab", "раздел"),
            ("↑↓", "поле"),
            ("enter", "поднять"),
            ("ctrl+q", "выход"),
        ],
        HomeTab::Look | HomeTab::Sound => &[
            ("tab", "раздел"),
            ("↑↓", "строка"),
            ("←→", "значение"),
            ("ctrl+q", "выход"),
        ],
        HomeTab::Help => &[("tab", "раздел"), ("ctrl+q", "выход")],
    };
    let mut line = hints(pairs, theme_.primary());
    line.spans.insert(0, Span::raw(" "));
    line
}

/// Строки вкладки и место курсора в них.
///
/// Собираются отдельно от рисования: по их числу решается, какой высоты
/// делать рамку, а рамку надо нарисовать раньше содержимого.
fn tab_lines(
    login: &Login,
    state: &State,
    width: usize,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let (mut lines, cursor) = match login.tab {
        HomeTab::Join => join_tab(login, state, width),
        HomeTab::Host => host_tab(login, state, width),
        HomeTab::Look => look_tab(login, state, width),
        HomeTab::Sound => sound_tab(login, state, width),
        HomeTab::Help => (help_tab(state, width), None),
    };
    // Пустая строка сверху: текст, начинающийся вплотную к шву вкладки,
    // читается как его продолжение.
    lines.insert(0, Line::default());
    (lines, cursor.map(|(row, column)| (row + 1, column)))
}

/// Рисует содержимое вкладки, проигрывая переезд.
///
/// Строки въезжают слева и появляются сверху вниз: так глаз успевает понять,
/// что сменился раздел, а не всё окно.
fn draw_tab(
    frame: &mut Frame,
    login: &Login,
    area: Rect,
    lines: Vec<Line<'static>>,
    cursor: Option<(u16, u16)>,
) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    // Небольшие поля внутри рамки: текст, прижатый к линии, читается тяжело.
    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width - 2,
        height: area.height,
    };

    let passed = login.switched.elapsed();
    let done = passed >= SLIDE;
    let progress = theme::ease_out(passed.as_secs_f32() / SLIDE.as_secs_f32());
    let shift = ((1.0 - progress) * SLIDE_SHIFT) as u16;
    // Строки проявляются сверху вниз — с запасом, чтобы к концу переезда
    // показались все.
    let shown = if done {
        lines.len()
    } else {
        (lines.len() as f32 * progress * 1.4).ceil() as usize
    };

    let visible: Vec<Line> = lines
        .into_iter()
        .take(shown.min(inner.height as usize))
        .collect();
    let body = Rect {
        x: inner.x + shift.min(inner.width.saturating_sub(1)),
        width: inner.width.saturating_sub(shift),
        ..inner
    };
    frame.render_widget(Paragraph::new(visible), body);

    // Курсор ставим только когда строка с полем уже приехала: мигающий
    // курсор посреди пустоты выглядит поломкой.
    if let Some((row, column)) = cursor
        && done
        && row < inner.height
    {
        frame.set_cursor_position(Position::new(
            body.x + column.min(body.width.saturating_sub(1)),
            body.y + row,
        ));
    }
}

/// Строка поля: цветная стойка, текст и курсор.
///
/// Стойка вместо рамки — по той же причине, по какой её нет в ленте: рамка
/// вокруг каждого поля превращает форму в решётку, а цвет говорит то же
/// самое одной колонкой.
fn field_row(
    label: &str,
    input: &crate::app::Input,
    focused: bool,
    width: usize,
    theme_: Theme,
) -> (Vec<Line<'static>>, Option<u16>) {
    let bar = if focused {
        theme_.primary()
    } else {
        theme::LINE
    };
    let label_style = if focused {
        Style::new()
            .fg(theme_.secondary())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme::MUTED)
    };

    let room = width.saturating_sub(2).max(1);
    let (text, cursor) = field_view(input, room);
    let value = if text.is_empty() && !focused {
        Span::styled("—", Style::new().fg(theme::LINE))
    } else {
        Span::styled(
            text,
            Style::new().fg(if focused { theme::TEXT } else { theme::SUBTLE }),
        )
    };

    let lines = vec![
        Line::from(Span::styled(label.to_string(), label_style)),
        Line::from(vec![Span::styled("▌ ", Style::new().fg(bar)), value]),
    ];
    (lines, focused.then_some(cursor as u16 + 2))
}

/// Вкладка «войти»: форма слева, живые комнаты справа.
fn join_tab(
    login: &Login,
    state: &State,
    width: usize,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let theme_ = state.theme;
    let focus_form = login.rooms_selected.is_none();
    // Две колонки только там, где обе не станут щелью.
    let split = width >= 54;
    let form_width = if split {
        (width * 5 / 11).max(24)
    } else {
        width
    };

    let fields = [
        (Field::Nickname, "ваш ник", &login.nickname),
        (Field::Room, "комната", &login.room),
        (
            Field::Server,
            "сервер — адрес или тикет друга",
            &login.server,
        ),
    ];

    let mut left = Vec::new();
    let mut cursor = None;
    for (field, label, input) in fields {
        let focused = focus_form && login.field == field;
        let (rows, column) = field_row(label, input, focused, form_width, theme_);
        if let Some(column) = column {
            // Курсор стоит на второй строке поля — там, где сам текст.
            cursor = Some((left.len() as u16 + 1, column));
        }
        left.extend(rows);
    }

    left.push(Line::default());
    left.push(Line::from(widgets::button(
        "enter — войти",
        focus_form,
        theme_,
    )));

    let mut right = Vec::new();
    if split {
        right = rooms_lines(login, state, width - form_width - 2);
    }

    let mut lines = if split {
        columns(left, right, form_width, 2)
    } else {
        left
    };

    // Ошибка важнее подсказки: она объясняет, почему предыдущая попытка
    // не удалась, и её место — прямо под формой. Пустую строку под неё
    // занимаем только когда есть что сказать.
    match &login.error {
        Some(error) => {
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled("✕ ", Style::new().fg(theme::ERR)),
                Span::styled(
                    shorten(error, width.saturating_sub(2)),
                    Style::new().fg(theme::ERR),
                ),
            ]));
        }
        // В узком окне списка комнат нет — про них надо сказать словами.
        None if !split => {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                shorten(
                    &login
                        .rooms_note
                        .clone()
                        .unwrap_or_else(|| format!("комнат на сервере: {}", login.rooms.len())),
                    width,
                ),
                Style::new().fg(theme::MUTED),
            )));
        }
        None => {}
    }

    (lines, cursor)
}

/// Список комнат: имя, кружки участников и число.
fn rooms_lines(login: &Login, state: &State, width: usize) -> Vec<Line<'static>> {
    let theme_ = state.theme;
    let mut lines = vec![
        Line::from(caption("комнаты", theme_.primary(), theme_.secondary())),
        Line::default(),
    ];

    if login.rooms.is_empty() {
        let note = login
            .rooms_note
            .clone()
            .unwrap_or_else(|| "ctrl+r — спросить сервер".to_string());
        for chunk in super::wrap(&note, width.max(4)) {
            lines.push(Line::from(Span::styled(
                chunk,
                Style::new().fg(theme::MUTED),
            )));
        }
        return lines;
    }

    // Окно списка едет за выбором: без этого выбранное уезжает за край
    // и стрелки перестают что-либо значить.
    const VISIBLE: usize = 7;
    let start = match login.rooms_selected {
        Some(at) if at >= VISIBLE => at + 1 - VISIBLE,
        _ => 0,
    };

    for (at, room) in login.rooms.iter().enumerate().skip(start).take(VISIBLE) {
        let chosen = login.rooms_selected == Some(at);
        let name_width = width.saturating_sub(10);
        let mut spans = vec![
            Span::styled(
                if chosen { "❯ " } else { "  " },
                Style::new().fg(theme_.primary()),
            ),
            if chosen {
                strong(shorten(&room.name, name_width), theme::TEXT)
            } else {
                Span::styled(
                    shorten(&room.name, name_width),
                    Style::new().fg(theme::SUBTLE),
                )
            },
        ];
        // Кружки идут сразу за именем, а не по правому краю: иначе между
        // комнатой и её людьми зияет полколонки пустоты.
        let line = pad(
            Line::from(std::mem::take(&mut spans)),
            width.saturating_sub(8).min(20),
        );
        let mut spans = line.spans;
        spans.extend(widgets::dots(
            room.users,
            if chosen {
                theme_.secondary()
            } else {
                theme::MUTED
            },
        ));
        lines.push(Line::from(spans));
    }

    if login.rooms.len() > VISIBLE {
        lines.push(Line::from(Span::styled(
            format!("  и ещё {}", login.rooms.len() - VISIBLE),
            Style::new().fg(theme::LINE),
        )));
    }
    lines
}

/// Вкладка «поднять»: свой сервер прямо в этом окне.
fn host_tab(
    login: &Login,
    state: &State,
    width: usize,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let theme_ = state.theme;
    let mut lines = Vec::new();

    let fields = [
        (Field::Nickname, "ваш ник", &login.nickname),
        (Field::Room, "название комнаты", &login.room),
        (Field::Port, "порт", &login.port),
    ];
    let form_width = width.min(38);
    let mut cursor = None;
    for (field, label, input) in fields {
        let focused = login.field == field;
        let (rows, column) = field_row(label, input, focused, form_width, theme_);
        if let Some(column) = column {
            cursor = Some((lines.len() as u16 + 1, column));
        }
        lines.extend(rows);
    }

    lines.push(Line::default());
    // Два способа позвать друга — и оба честно названы: тикет работает
    // откуда угодно, адрес в сети — только для тех, кто рядом.
    for (mark, color, text) in [
        (
            "✦",
            theme_.secondary(),
            "наружу поднимется туннель — другу хватит тикета из переписки",
        ),
        (
            "◆",
            theme::OK,
            "кто в той же сети — зайдёт по адресу, он появится в комнате",
        ),
        (
            "◇",
            theme::MUTED,
            "комната живёт, пока открыт этот терминал",
        ),
    ] {
        let mut first = true;
        for chunk in super::wrap(text, width.saturating_sub(2).max(8)) {
            lines.push(Line::from(vec![
                Span::styled(
                    if first {
                        format!("{mark} ")
                    } else {
                        "  ".into()
                    },
                    Style::new().fg(color),
                ),
                Span::styled(chunk, Style::new().fg(theme::MUTED)),
            ]));
            first = false;
        }
    }

    lines.push(Line::default());
    // Пока сервер встаёт, кнопка уступает место крутящемуся спиннеру: нажимать
    // второй раз не нужно, и это должно быть видно.
    lines.push(Line::from(match &login.busy {
        Some(what) => vec![
            Span::styled(
                theme::spinner(state.tick),
                Style::new().fg(theme_.primary()),
            ),
            Span::styled(format!(" {what}"), Style::new().fg(theme::SUBTLE)),
        ],
        None => widgets::button("enter — поднять и войти", true, theme_),
    }));
    if let Some(error) = &login.error {
        lines.push(Line::from(vec![
            Span::styled("✕ ", Style::new().fg(theme::ERR)),
            Span::styled(error.clone(), Style::new().fg(theme::ERR)),
        ]));
    }

    (lines, cursor)
}

/// Строка настройки: подпись, значение в уголках и метка выбора.
fn setting_row(
    title: &str,
    value: &str,
    chosen: bool,
    width: usize,
    theme_: Theme,
) -> Line<'static> {
    let label = width.min(26);
    let mut spans = vec![
        Span::styled(
            if chosen { "❯ " } else { "  " },
            Style::new().fg(theme_.primary()),
        ),
        if chosen {
            strong(title.to_string(), theme::TEXT)
        } else {
            Span::styled(title.to_string(), Style::new().fg(theme::SUBTLE))
        },
    ];
    let line = pad(Line::from(std::mem::take(&mut spans)), label);
    let mut spans = line.spans;
    // Имя устройства бывает длиной в предложение — режем по остатку строки.
    let room = width.saturating_sub(label + 4);
    spans.extend(widgets::chooser(&shorten(value, room), chosen, theme_));
    Line::from(spans)
}

/// Вкладка «звук»: куда играть, с чего писать и как громко звенеть.
fn sound_tab(
    login: &Login,
    state: &State,
    width: usize,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let theme_ = state.theme;
    let mut lines = Vec::new();

    for (at, setting) in SoundSetting::ALL.iter().enumerate() {
        lines.push(setting_row(
            setting.title(),
            &state.sound_value(*setting),
            at == login.setting,
            width,
            theme_,
        ));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        shorten(login.current_sound().hint(), width),
        Style::new().fg(theme::MUTED),
    )));

    lines.push(Line::default());
    lines.push(Line::from(caption(
        "что нашлось",
        theme_.primary(),
        theme_.secondary(),
    )));
    lines.push(Line::default());
    // Пустой список — не молчание, а ответ: звука на машине может не быть
    // вовсе, и человек должен понимать, почему выбирать не из чего.
    let found = format!(
        "динамиков: {} · микрофонов: {}",
        state.audio.outputs.len(),
        state.audio.inputs.len()
    );
    lines.push(Line::from(Span::styled(
        shorten(&found, width),
        Style::new().fg(theme::SUBTLE),
    )));
    if state.audio.outputs.is_empty() {
        lines.push(Line::from(Span::styled(
            shorten("звуковой карты не видно — чат будет работать молча", width),
            Style::new().fg(theme::WARN),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            shorten("выбрали динамики — звоночек проверит их сразу", width),
            Style::new().fg(theme::LINE),
        )));
    }

    (lines, None)
}

/// Вкладка «вид»: тема, картинки, колонка людей и запуск в терминале.
fn look_tab(
    login: &Login,
    state: &State,
    width: usize,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let theme_ = state.theme;
    let mut lines = Vec::new();

    for (at, setting) in Setting::ALL.iter().enumerate() {
        lines.push(setting_row(
            setting.title(),
            state.setting_value(*setting),
            at == login.setting,
            width,
            theme_,
        ));
    }

    lines.push(Line::default());
    // Пояснение к выбранной строке: подписывать все четыре сразу — стена
    // текста, а одна строка объясняет ровно то, на что смотрят.
    lines.push(Line::from(Span::styled(
        shorten(login.current_setting().hint(), width),
        Style::new().fg(theme::MUTED),
    )));

    lines.push(Line::default());
    lines.push(Line::from(caption(
        "как это выглядит",
        theme_.primary(),
        theme_.secondary(),
    )));
    lines.push(Line::default());
    // Живая проба темы: полоска градиента, плашка и цвета ников. Выбирать
    // цвет по названию — гадание, а так видно сразу.
    let ramp: Vec<Span> = (0..width.min(28))
        .map(|at| {
            let t = at as f32 / width.clamp(1, 28) as f32;
            Span::styled(
                "█",
                Style::new().fg(theme::mix(theme_.primary(), theme_.secondary(), t)),
            )
        })
        .collect();
    lines.push(Line::from(ramp));
    let mut sample = theme::pill(" ◆ general ", theme_.primary(), theme::INK);
    sample.push(Span::raw("  "));
    for (at, color) in theme::NICKS.iter().take(6).enumerate() {
        sample.push(Span::styled(
            if at == 0 { "●" } else { " ●" },
            Style::new().fg(*color),
        ));
    }
    lines.push(Line::from(sample));

    (lines, None)
}

/// Вкладка «справка»: клавиши слева, команды справа.
///
/// Полный список всё равно живёт в окне по F1 — здесь важно показать, что
/// учить ничего не надо: всё главное подписано на клавишах.
fn help_tab(state: &State, width: usize) -> Vec<Line<'static>> {
    let theme_ = state.theme;
    // Справка разбита пустой строкой: до неё клавиши, после — команды.
    let split = crate::app::HELP
        .iter()
        .position(|entry| entry.is_empty())
        .unwrap_or(crate::app::HELP.len());
    let (keys, commands) = crate::app::HELP.split_at(split);

    // Столько строк остаётся под список после подписи, отбивки и строки
    // «и ещё столько-то».
    const ROWS: usize = 10;
    let narrow = width < 60;
    let left_width = if narrow { width } else { width / 2 - 1 };
    let right_width = width.saturating_sub(left_width + 2);

    let entries = |source: &[&'static str], room: usize, column: usize| -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for entry in source.iter().filter(|entry| !entry.is_empty()).take(room) {
            match entry.split_once(" — ") {
                // Колонка под клавишу фиксированная: описания выстраиваются
                // в столбик, и список читается сверху вниз, а не по диагонали.
                Some((key, what)) if key.width() <= 12 => lines.push(Line::from(vec![
                    Span::styled(
                        format!("{key:<12} "),
                        Style::new()
                            .fg(theme_.primary())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        shorten(what, column.saturating_sub(13)),
                        Style::new().fg(theme::SUBTLE),
                    ),
                ])),
                // Длинная подпись вроде «щелчок по вложению» в колонку не
                // влезает — её печатаем строкой целиком.
                Some((key, what)) => lines.push(Line::from(vec![
                    Span::styled(format!("{key} "), Style::new().fg(theme_.primary())),
                    Span::styled(
                        shorten(what, column.saturating_sub(key.width() + 1)),
                        Style::new().fg(theme::SUBTLE),
                    ),
                ])),
                None => lines.push(Line::from(Span::styled(
                    shorten(entry, column),
                    Style::new().fg(theme::MUTED),
                ))),
            }
        }
        let all = source.iter().filter(|entry| !entry.is_empty()).count();
        if all > room {
            lines.push(Line::from(Span::styled(
                shorten(&format!("и ещё {} — F1 в переписке", all - room), column),
                Style::new().fg(theme::LINE),
            )));
        }
        lines
    };

    let mut left = vec![
        Line::from(caption("клавиши", theme_.primary(), theme_.secondary())),
        Line::default(),
    ];
    left.extend(entries(keys, ROWS, left_width));

    if narrow {
        return left;
    }

    let mut right = vec![
        Line::from(caption("команды", theme_.primary(), theme_.secondary())),
        Line::default(),
    ];
    right.extend(entries(commands, ROWS, right_width));

    columns(left, right, left_width, 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Action, NetEvent, update};
    use crate::ui::tests::{render, render_buffer};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Instant;

    fn home() -> State {
        let (state, _) = State::new(None, "general".into());
        state
    }

    fn press(state: &mut State, code: KeyCode) {
        update(state, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    /// Прокручивает переезд вкладки до конца: анимация не должна мешать
    /// проверять содержимое.
    fn settle(state: &mut State) {
        if let crate::app::Screen::Login(login) = &mut state.screen {
            login.switched = Instant::now() - SLIDE * 2;
        }
    }

    #[test]
    fn the_first_screen_asks_for_a_nickname() {
        let mut state = home();
        settle(&mut state);

        let screen = render(&mut state, 90, 30);

        assert!(screen.contains("ваш ник"), "{screen}");
        assert!(screen.contains("general"), "{screen}");
        assert!(screen.contains("войти"), "{screen}");
        // Заголовок блочными буквами — только на просторном экране.
        assert!(screen.contains("███"), "{screen}");
    }

    #[test]
    fn every_tab_is_reachable_by_tab() {
        let mut state = home();
        let mut screens = Vec::new();
        for _ in 0..crate::app::HomeTab::ALL.len() {
            press(&mut state, KeyCode::Tab);
            settle(&mut state);
            screens.push(render(&mut state, 96, 30));
        }

        let [host, look, sound, help, join] = screens.as_slice() else {
            panic!("вкладок стало другое число");
        };
        assert!(host.contains("порт"), "{host}");
        assert!(look.contains("тема"), "{look}");
        assert!(sound.contains("динамики"), "{sound}");
        assert!(help.contains("F2"), "{help}");
        // Пятый Tab замыкает круг обратно на вход.
        assert!(join.contains("ваш ник"), "{join}");
    }

    #[test]
    fn the_sound_tab_lists_what_it_found() {
        let mut state = home();
        state.audio.outputs = vec!["Наушники (Realtek)".into(), "HDMI".into()];
        state.audio.inputs = vec!["Микрофон гарнитуры".into()];
        for _ in 0..3 {
            press(&mut state, KeyCode::Tab);
        }
        settle(&mut state);

        let before = render(&mut state, 96, 30);
        press(&mut state, KeyCode::Right);
        let after = render(&mut state, 96, 30);

        assert!(before.contains("как в системе"), "{before}");
        assert!(before.contains("динамиков: 2"), "{before}");
        // Стрелка вправо листает на первое найденное устройство.
        assert!(after.contains("Наушники"), "{after}");
    }

    #[test]
    fn the_look_tab_shows_the_theme_it_will_apply() {
        let mut state = home();
        for _ in 0..2 {
            press(&mut state, KeyCode::Tab);
        }
        settle(&mut state);

        let before = render(&mut state, 90, 30);
        press(&mut state, KeyCode::Right);
        let after = render(&mut state, 90, 30);

        assert!(before.contains("сирень"), "{before}");
        assert!(after.contains("долли"), "{after}");
    }

    #[test]
    fn rooms_from_the_server_are_listed_with_their_people() {
        let mut state = home();
        update(
            &mut state,
            Action::Rooms(Ok(vec![
                common::RoomSummary {
                    name: "general".into(),
                    users: 3,
                },
                common::RoomSummary {
                    name: "rust".into(),
                    users: 1,
                },
            ])),
        );
        settle(&mut state);

        let screen = render(&mut state, 90, 30);

        assert!(screen.contains("К О М Н А Т Ы"), "{screen}");
        assert!(screen.contains("rust"), "{screen}");
        assert!(screen.contains("●●●"), "{screen}");
    }

    #[test]
    fn arrows_walk_from_the_form_into_the_room_list() {
        let mut state = home();
        update(
            &mut state,
            Action::Rooms(Ok(vec![common::RoomSummary {
                name: "rust".into(),
                users: 1,
            }])),
        );

        // Три поля вниз — и следующий шаг уводит в список.
        for _ in 0..3 {
            press(&mut state, KeyCode::Down);
        }
        settle(&mut state);
        let screen = render(&mut state, 90, 30);

        assert!(screen.contains("❯ rust"), "{screen}");
    }

    #[test]
    fn a_failure_is_shown_under_the_form() {
        let mut state = home();
        update(
            &mut state,
            Action::Net(NetEvent::Fatal {
                reason: "ник занят".into(),
            }),
        );
        settle(&mut state);

        let screen = render(&mut state, 90, 30);

        assert!(screen.contains("ник занят"), "{screen}");
    }

    #[test]
    fn a_narrow_window_drops_the_banner_but_keeps_the_form() {
        let mut state = home();
        settle(&mut state);

        let screen = render(&mut state, 46, 14);

        assert!(!screen.contains("███"), "{screen}");
        assert!(screen.contains("ваш ник"), "{screen}");
    }

    #[test]
    fn the_banner_shimmers_between_frames() {
        let mut state = home();

        // Блик меняет цвет, а не буквы: сравнивать надо буферы целиком.
        let first = render_buffer(&mut state, 90, 30);
        for _ in 0..6 {
            update(&mut state, Action::Tick);
        }
        let second = render_buffer(&mut state, 90, 30);

        assert_ne!(first, second, "блик по заголовку не бежит");
    }
}
