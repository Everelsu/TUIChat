//! Мелкие детали, из которых собраны оба экрана: вкладки, полоски, значки.
//!
//! Всё рисуется обычными символами рамок и полублоками. Ни один из них не
//! требует особого шрифта: интерфейс должен выглядеть одинаково и в Windows
//! Terminal, и в голом xterm, а не разваливаться на квадраты там, где нет
//! шрифта с иконками.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::theme::{self, Theme};

/// Название программы блочными буквами.
///
/// Ровно то, что написано на коробке, и ничего больше: подпись под логотипом
/// повторяла бы то, что и так видно по вкладкам.
pub const LOGO: [&str; 5] = [
    "██   ██        ██ ██         ██  ",
    "███  ██        ██ ██        █████",
    "██ █ ██ ██  ██ ██ ██         ██  ",
    "██  ███ ██  ██ ██ ██         ██  ",
    "██   ██  ████  ██ ██ ██████  ███ ",
];

/// Ширина логотипа в колонках.
pub const LOGO_WIDTH: u16 = 33;

/// Логотип с бегущим по нему бликом.
///
/// Блик едет слева направо и уезжает за край: надпись выглядит подсвеченной
/// сбоку, а не мигающей. Фаза считается от номера кадра, поэтому анимация
/// идёт сама и ничего не помнит между кадрами.
pub fn logo(theme: Theme, tick: u64) -> Vec<Line<'static>> {
    // Полный проход за 60 кадров: примерно четыре секунды при кадре в 60 мс.
    // Быстрее — рябит, медленнее — кажется, что интерфейс подвис.
    const PERIOD: u64 = 60;
    let phase = (tick % PERIOD) as f32 / PERIOD as f32 * 1.6 - 0.3;
    let width = LOGO_WIDTH as usize;

    LOGO.iter()
        .map(|row| {
            Line::from(theme::shimmer(
                row,
                theme.primary(),
                theme.secondary(),
                theme.glow(),
                phase,
                0,
                width,
            ))
        })
        .collect()
}

/// Вкладка: значок и подпись.
pub struct Tab {
    pub icon: &'static str,
    pub title: &'static str,
}

/// Рисует полосу вкладок и рамку под ней, отдавая место под содержимое.
///
/// Активная вкладка внизу открыта — она перетекает в рамку с содержимым, и
/// глазу не нужно объяснять, какая из четырёх сейчас показана. Так же это
/// сделано в lipgloss, и не зря: цветом одним такую связь не передать.
///
/// Возвращает прямоугольник под содержимое. Если вкладки в ширину не влезли,
/// они схлопываются в одну строку — рамка при этом остаётся.
pub fn tabbed(frame: &mut Frame, area: Rect, tabs: &[Tab], active: usize, theme: Theme) -> Rect {
    if area.height < 3 || area.width < 8 {
        return compact_tabs(frame, area, tabs, active, theme);
    }

    // Ширина каждой вкладки вместе с рамкой: « ◆ войти » плюс две стойки.
    let labels: Vec<String> = tabs
        .iter()
        .map(|tab| format!(" {} {} ", tab.icon, tab.title))
        .collect();
    let boxes: Vec<usize> = labels.iter().map(|label| label.width() + 2).collect();
    // Слева отступ в две колонки, между вкладками — одна.
    let needed: usize = boxes.iter().sum::<usize>() + boxes.len().saturating_sub(1) + 3;
    if needed > area.width as usize {
        return compact_tabs(frame, area, tabs, active, theme);
    }

    let body = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height - 2,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::LINE));
    let inner = block.inner(body);
    frame.render_widget(block, body);

    let line = Style::new().fg(theme::LINE);
    let mut tops = vec![Span::styled("  ", line)];
    let mut names = vec![Span::styled("  ", line)];
    // Верхняя граница рамки уже нарисована блоком — перерисовываем её целиком,
    // вставляя вырез под активной вкладкой.
    let mut seam = vec![Span::styled("╭─", line)];

    for (at, label) in labels.iter().enumerate() {
        let width = label.width();
        let chosen = at == active;
        let edge = if chosen {
            Style::new().fg(theme.primary())
        } else {
            line
        };

        if at > 0 {
            tops.push(Span::styled(" ", line));
            names.push(Span::styled(" ", line));
            seam.push(Span::styled("─", line));
        }

        tops.push(Span::styled(format!("╭{}╮", "─".repeat(width)), edge));
        names.push(Span::styled("│", edge));
        if chosen {
            // Подпись активной вкладки переливается: единственное место, где
            // градиент оправдан — он показывает, куда смотреть.
            names.extend(theme::gradient_bold(
                label,
                theme.primary(),
                theme.secondary(),
            ));
            seam.push(Span::styled("╯", edge));
            seam.push(Span::styled(" ".repeat(width), line));
            seam.push(Span::styled("╰", edge));
        } else {
            names.push(Span::styled(label.clone(), Style::new().fg(theme::MUTED)));
            seam.push(Span::styled(format!("┴{}┴", "─".repeat(width)), line));
        }
        names.push(Span::styled("│", edge));
    }

    let used: usize = seam.iter().map(|span| span.content.width()).sum();
    let rest = (area.width as usize).saturating_sub(used + 1);
    seam.push(Span::styled("─".repeat(rest), line));
    seam.push(Span::styled("╮", line));

    frame.render_widget(
        Paragraph::new(vec![Line::from(tops), Line::from(names)]),
        Rect { height: 2, ..area },
    );
    frame.render_widget(
        Paragraph::new(Line::from(seam)),
        Rect {
            y: area.y + 2,
            height: 1,
            ..area
        },
    );

    inner
}

/// Запасные вкладки для узкого окна: одна строка без рамок.
fn compact_tabs(frame: &mut Frame, area: Rect, tabs: &[Tab], active: usize, theme: Theme) -> Rect {
    if area.height == 0 || area.width == 0 {
        return area;
    }

    // Активная вкладка названа словом, остальные — значками: место есть
    // ровно на одно название, и оно должно достаться тому разделу, который
    // человек сейчас видит.
    let named = format!(" {} {} ", tabs[active].icon, tabs[active].title);
    let room = area.width as usize;
    let full = named.width() + 2 + (tabs.len() - 1) * 2;

    let mut spans = Vec::new();
    for (at, tab) in tabs.iter().enumerate() {
        if at > 0 {
            spans.push(Span::styled(" ", Style::new().fg(theme::LINE)));
        }
        if at == active && full <= room {
            spans.extend(theme::pill(&named, theme.primary(), theme::INK));
        } else if at == active {
            spans.extend(theme::pill(tab.icon, theme.primary(), theme::INK));
        } else {
            spans.push(Span::styled(
                tab.icon.to_string(),
                Style::new().fg(theme::MUTED),
            ));
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect { height: 1, ..area },
    );
    Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height - 1,
    }
}

/// Кружки участников: сколько людей в комнате, видно не считая цифру.
///
/// Больше пяти не рисуем — дальше кружки сливаются в кашу, и число полезнее.
pub fn dots(count: usize, color: Color) -> Vec<Span<'static>> {
    if count == 0 {
        return vec![Span::styled("—", Style::new().fg(theme::LINE))];
    }
    let shown = count.min(5);
    let mut spans = vec![Span::styled("●".repeat(shown), Style::new().fg(color))];
    if count > shown {
        spans.push(Span::styled(
            format!("+{}", count - shown),
            Style::new().fg(theme::MUTED),
        ));
    }
    spans
}

/// Значение настройки в уголках: «‹ авто ›» сразу говорит, что его листают
/// стрелками, а не набирают.
pub fn chooser(value: &str, chosen: bool, theme: Theme) -> Vec<Span<'static>> {
    let (arrows, text) = if chosen {
        (
            Style::new().fg(theme.primary()),
            Style::new().fg(theme::TEXT).add_modifier(Modifier::BOLD),
        )
    } else {
        (Style::new().fg(theme::LINE), Style::new().fg(theme::SUBTLE))
    };
    vec![
        Span::styled("‹ ", arrows),
        Span::styled(value.to_string(), text),
        Span::styled(" ›", arrows),
    ]
}

/// Кнопка: подпись в скобках-уголках, подсвеченная, когда её ждут.
pub fn button(label: &str, ready: bool, theme: Theme) -> Vec<Span<'static>> {
    if ready {
        theme::pill(&format!(" {label} "), theme.primary(), theme::INK)
    } else {
        vec![Span::styled(
            format!("  {label}  "),
            Style::new().fg(theme::MUTED),
        )]
    }
}

/// Точка состояния связи: горит ровно, дышит или гаснет.
///
/// Дыхание — не украшение: пока клиент переподключается, ровно горящая точка
/// неотличима от «всё хорошо», и человек не понимает, почему не доходят
/// сообщения.
pub fn status_dot(color: Color, alive: bool, tick: u64) -> Span<'static> {
    let color = if alive {
        color
    } else {
        theme::mix(theme::dim(color, 0.6), color, theme::pulse(tick, 16))
    };
    Span::styled("●", Style::new().fg(color))
}

/// Полоска-бегунок для долгой работы, длину которой мы не знаем.
///
/// Обычный процент здесь соврал бы: сколько осталось качать файл, клиент
/// узнаёт только по факту. Бегущая волна честно означает «идёт работа».
pub fn runner(width: usize, theme: Theme, tick: u64) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let head = (tick as usize / 2) % (width + 6);
    (0..width)
        .map(|at| {
            // Хвост тянется за головой и гаснет: так видно направление.
            let distance = (head as isize - at as isize).clamp(-6, 6).abs() as f32;
            let light = (1.0 - distance / 6.0).max(0.0);
            Span::styled(
                "─",
                Style::new().fg(theme::mix(theme::LINE, theme.primary(), light * light)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn tabs() -> Vec<Tab> {
        vec![
            Tab {
                icon: "◆",
                title: "войти",
            },
            Tab {
                icon: "✦",
                title: "поднять",
            },
        ]
    }

    fn render(width: u16, height: u16, active: usize) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                tabbed(frame, frame.area(), &tabs(), active, Theme::default());
            })
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn the_active_tab_opens_into_the_frame() {
        let screen = render(48, 8, 0);
        let rows: Vec<&str> = screen.lines().collect();
        // TestBackend печатает строки в кавычках — край рамки ищем внутри.
        let seam = rows[2].trim_matches(['"', ' ']);

        // Под активной вкладкой — вырез, под соседней — сплошная линия.
        assert!(seam.contains('╯'), "{screen}");
        assert!(seam.contains('┴'), "{screen}");
        assert!(seam.ends_with('╮'), "{screen}");
    }

    #[test]
    fn every_tab_is_named() {
        let screen = render(48, 8, 1);

        assert!(screen.contains("войти"), "{screen}");
        assert!(screen.contains("поднять"), "{screen}");
    }

    #[test]
    fn narrow_window_falls_back_to_icons() {
        // Вкладки в двадцать колонок не влезают — остаются значки, а рамка
        // с содержимым не должна из-за этого пропасть.
        let screen = render(20, 8, 0);

        assert!(!screen.contains("поднять"), "{screen}");
        assert!(screen.contains("✦"), "{screen}");
    }

    #[test]
    fn tiny_area_survives() {
        for (width, height) in [(1, 1), (3, 2), (8, 3), (12, 1)] {
            render(width, height, 0);
        }
    }

    #[test]
    fn dots_count_people_and_stop_at_five() {
        let few: String = dots(3, theme::OK)
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        let many: String = dots(12, theme::OK)
            .iter()
            .map(|span| span.content.to_string())
            .collect();

        assert_eq!(few, "●●●");
        assert_eq!(many, "●●●●●+7");
        assert_eq!(
            dots(0, theme::OK)[0].content.to_string(),
            "—",
            "пустая комната должна выглядеть пустой"
        );
    }

    #[test]
    fn the_runner_keeps_its_width() {
        let spans = runner(10, Theme::default(), 4);

        assert_eq!(spans.len(), 10);
    }
}
