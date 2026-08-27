//! Лента переписки: как реплики превращаются в строки экрана.
//!
//! Сообщения одного человека, сказанные подряд, объединяются в группу с одной
//! подписью и общей цветной стойкой слева. Стойка делает то, чего не делает
//! отступ: по ней видно, где начинается и кончается чужая речь, даже когда
//! экран забит текстом целиком.

use std::collections::HashMap;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::{
    app::{Entry, Search, SystemKind, Thumbnail},
    media,
    theme::{self, Theme},
    ui::{GUTTER, human_size, local_time, wrap},
};

/// Сообщения одного человека подряд объединяются в группу, если между ними
/// меньше двух минут: подпись над каждой репликой превращает переписку в
/// частокол из ников.
pub const GROUP_WINDOW_MS: i64 = 120_000;

/// Стойка группы: сплошная у подписи, тонкая у продолжения.
const BAR: &str = "▐";
const BAR_THIN: &str = "│";

/// Сколько колонок съедает стойка: отступ от края, сама полоса и пробел
/// перед текстом.
const BAR_WIDTH: usize = 3;

/// Высота миниатюры в строках и её предельная ширина в колонках.
///
/// Лента должна оставаться лентой: картинка во весь экран вытесняет разговор,
/// а разглядеть её целиком можно по `/view`.
pub const THUMB_ROWS: u16 = 10;
pub const THUMB_COLS: u16 = 46;

/// Сколько живёт вспышка у нового сообщения.
const FLASH: std::time::Duration = std::time::Duration::from_millis(900);

/// Цвета ников, заданные человеком: ник в нижнем регистре -> цвет.
pub type Colors = HashMap<String, Color>;

/// Место под картинку в ленте: с какой строки и какое вложение.
pub struct Slot {
    pub line: usize,
    pub id: uuid::Uuid,
}

pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub offsets: Vec<usize>,
    pub slots: Vec<Slot>,
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
    pub theme: Theme,
}

/// Цвет ника: сначала выбор человека, иначе устойчивый цвет по хешу.
pub fn nick_color(colors: &Colors, nickname: &str, mine: bool, theme: Theme) -> Color {
    if let Some(color) = colors.get(&nickname.to_lowercase()) {
        return *color;
    }
    if mine {
        return theme.primary();
    }
    let hash = nickname.to_lowercase().bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(byte.into())
    });
    theme::NICKS[hash as usize % theme::NICKS.len()]
}

/// Рисует голосовое графиком: столбики громкости и длительность.
///
/// Пока запись играет, пройденная часть подсвечена — по ней видно, сколько
/// осталось, а заодно понятно, что звук вообще идёт: в терминале это иначе
/// никак не показать.
pub fn draw_voice(
    wave: &media::Waveform,
    playing: Option<std::time::Duration>,
    name: &str,
    theme: Theme,
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

    let mut spans = vec![Span::styled("♪ ", Style::new().fg(theme.primary()))];
    for (index, level) in wave.bars.iter().enumerate() {
        let step = theme::STEPS[(*level as usize).min(theme::STEPS.len() - 1)];
        // Пройденное красим градиентом от начала записи к концу, остальное
        // приглушаем: граница между ними и есть указатель воспроизведения.
        let colour = if index < edge {
            let t = index as f32 / wave.bars.len().max(1) as f32;
            theme::mix(theme.primary(), theme.secondary(), t)
        } else {
            theme::LINE
        };
        spans.push(Span::styled(step.to_string(), Style::new().fg(colour)));
    }

    let seconds = wave.millis / 1000;
    spans.push(Span::styled(
        format!("  {}:{:02}  {name}", seconds / 60, seconds % 60),
        Style::new().fg(theme::MUTED),
    ));
    spans
}

/// Возвращает строки и карту «номер записи -> номер её первой строки».
pub fn render_entries(entries: &[Entry], width: usize, decor: &Decor<'_>) -> Rendered {
    let Decor {
        search,
        picking,
        colors,
        thumbnails,
        waveforms,
        playing,
        theme,
    } = *decor;
    let now = std::time::Instant::now();
    // Текст начинается за стойкой: ширину переноса считаем от неё, иначе
    // строки вылезут за край ровно на стойку.
    let text_width = width.saturating_sub(BAR_WIDTH).max(1);
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
            Some(theme.primary())
        } else {
            match search {
                Some(search) if search.is_match(index) => {
                    if search.current_entry() == Some(index) {
                        Some(theme::MENTION)
                    } else {
                        Some(theme::LINE)
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
                let color = nick_color(colors, from, *mine, theme);
                // Свежая реплика вспыхивает и гаснет к цвету автора: глаз
                // ловит именно появление, а не оттенок.
                let bar_color = match flash(*arrived, now) {
                    Some(t) => theme::mix(color, theme.glow(), t),
                    None => color,
                };

                let same_author = previous
                    .is_some_and(|(author, at)| author == from && *ts - at < GROUP_WINDOW_MS);
                if !same_author {
                    if !lines.is_empty() {
                        lines.push(Line::default());
                    }
                    offsets.push(lines.len());
                    let mut header = vec![
                        Span::raw(GUTTER),
                        Span::styled(BAR, Style::new().fg(bar_color)),
                        Span::raw(" "),
                        Span::styled(
                            from.clone(),
                            Style::new().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("  {}", local_time(*ts)),
                            Style::new().fg(theme::MUTED),
                        ),
                    ];
                    // Своё сообщение подписано явно: в плотной переписке
                    // видно, дошло ли отправленное, не сверяя ники.
                    if *mine {
                        header.push(Span::styled(
                            "  вы",
                            Style::new().fg(theme::mix(theme::MUTED, color, 0.5)),
                        ));
                    }
                    lines.push(Line::from(header));
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
                    Some(draw_voice(wave, elapsed, &attachment.name, theme))
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
                    let quote = format!("┆ {}: {}", reply.nickname, reply.excerpt);
                    for chunk in wrap(&quote, text_width) {
                        lines.push(Line::from(vec![
                            Span::raw(GUTTER),
                            Span::styled(BAR_THIN, Style::new().fg(theme::dim(color, 0.55))),
                            Span::raw(" "),
                            Span::styled(chunk, Style::new().fg(theme::MUTED)),
                        ]));
                    }
                }

                let style = if *mentions_me {
                    // Упоминание — единственная строка, адресованная лично:
                    // цвет у неё свой, чтобы не потерялась в потоке.
                    Style::new().fg(theme::MENTION).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme::TEXT)
                };
                let rail = theme::dim(color, 0.55);
                match &voice {
                    // График вместо строки: переносить его нельзя, он и так
                    // укладывается в ширину ленты.
                    Some(spans) => {
                        let mut line = vec![
                            Span::raw(GUTTER),
                            Span::styled(BAR_THIN, Style::new().fg(rail)),
                            Span::raw(" "),
                        ];
                        line.extend(spans.iter().cloned());
                        let mut rendered = Line::from(line);
                        if let Some(colour) = highlight {
                            rendered = rendered.style(Style::new().bg(colour));
                        }
                        lines.push(rendered);
                        // Подпись под графиком нужна, только если человек
                        // что-то написал вместе с голосовым.
                        if !text.is_empty() {
                            push_wrapped(&mut lines, style, text, text_width, highlight, rail);
                        }
                    }
                    None => push_wrapped(&mut lines, style, &body, text_width, highlight, rail),
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
                // Значок вместо слова: по нему видно, что случилось, ещё до
                // того, как строка прочитана.
                let (mark, color) = match kind {
                    SystemKind::Error => ("✕", theme::ERR),
                    SystemKind::Join => ("↳", theme::OK),
                    SystemKind::Leave => ("↰", theme::MUTED),
                    SystemKind::Info => ("·", theme::SUBTLE),
                };
                let style = Style::new().fg(color);
                for (at, chunk) in wrap(text, text_width).into_iter().enumerate() {
                    let mark = if at == 0 { mark } else { " " };
                    let mut line = Line::from(vec![
                        Span::raw(GUTTER),
                        Span::styled(mark.to_string(), style),
                        Span::raw(" "),
                        Span::styled(chunk, Style::new().fg(theme::mix(color, theme::MUTED, 0.4))),
                    ]);
                    if let Some(colour) = highlight {
                        line = line.style(Style::new().bg(colour));
                    }
                    lines.push(line);
                }
            }
        }
    }
    Rendered {
        lines,
        offsets,
        slots,
    }
}

fn push_wrapped(
    lines: &mut Vec<Line<'static>>,
    style: Style,
    text: &str,
    width: usize,
    highlight: Option<Color>,
    rail: Color,
) {
    let style = match highlight {
        Some(color) => style.bg(color).fg(theme::INK),
        None => style,
    };
    for chunk in wrap(text, width) {
        lines.push(Line::from(vec![
            Span::raw(GUTTER),
            Span::styled(BAR_THIN, Style::new().fg(rail)),
            Span::raw(" "),
            Span::styled(chunk, style),
        ]));
    }
}

/// Насколько ярко горит вспышка у свежей реплики: 1 — только что пришла,
/// 0 — вспышка догорела.
fn flash(arrived: std::time::Instant, now: std::time::Instant) -> Option<f32> {
    let age = now.saturating_duration_since(arrived);
    if age >= FLASH {
        return None;
    }
    // Гаснет с замедлением: резкий обрыв читается как мигание.
    Some(1.0 - theme::ease_out(age.as_secs_f32() / FLASH.as_secs_f32()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;
    use uuid::Uuid;

    /// Ширина строки в колонках — по ней проверяется, что лента не вылезает.
    fn line_width(line: &Line<'_>) -> usize {
        line.spans.iter().map(|span| span.content.width()).sum()
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
            theme: Theme::default(),
        }
    }

    fn said(from: &str, text: &str, ts: i64) -> Entry {
        Entry::Chat {
            id: Uuid::new_v4(),
            from: from.into(),
            text: text.into(),
            ts,
            mine: false,
            mentions_me: false,
            attachment: None,
            reply: None,
            arrived: std::time::Instant::now(),
        }
    }

    #[test]
    fn a_group_gets_one_header_and_a_rail() {
        let entries = vec![
            said("bob", "первое", 1_700_000_000_000),
            said("bob", "второе", 1_700_000_000_000),
        ];

        let rendered = render_entries(&entries, 40, &plain_decor(&HashMap::new()));

        let text: Vec<String> = rendered
            .lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        // Подпись одна на обе реплики, а стойка идёт вдоль всей группы.
        assert_eq!(text.iter().filter(|row| row.contains("bob")).count(), 1);
        assert!(text[0].contains(BAR), "{text:?}");
        assert!(text[1].contains(BAR_THIN), "{text:?}");
        assert!(text[2].contains(BAR_THIN), "{text:?}");
    }

    #[test]
    fn a_pause_starts_a_new_group() {
        let entries = vec![
            said("bob", "давно", 1_700_000_000_000),
            said("bob", "недавно", 1_700_000_000_000 + GROUP_WINDOW_MS + 1),
        ];

        let rendered = render_entries(&entries, 40, &plain_decor(&HashMap::new()));

        let headers = rendered
            .lines
            .iter()
            .filter(|line| line.spans.iter().any(|span| span.content.contains("bob")))
            .count();
        assert_eq!(headers, 2);
    }

    #[test]
    fn the_rail_does_not_push_text_off_the_edge() {
        let entries = vec![said("bob", &"я".repeat(200), 1_700_000_000_000)];

        let rendered = render_entries(&entries, 20, &plain_decor(&HashMap::new()));

        for line in &rendered.lines {
            assert!(line_width(line) <= 20, "строка шире ленты: {line:?}");
        }
    }

    #[test]
    fn system_lines_are_marked_by_kind() {
        let entries = vec![
            Entry::System {
                text: "bob вошёл".into(),
                kind: SystemKind::Join,
            },
            Entry::System {
                text: "всё сломалось".into(),
                kind: SystemKind::Error,
            },
        ];

        let rendered = render_entries(&entries, 40, &plain_decor(&HashMap::new()));

        let text: Vec<String> = rendered
            .lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(text.iter().any(|row| row.contains("↳")), "{text:?}");
        assert!(text.iter().any(|row| row.contains("✕")), "{text:?}");
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

    fn voice_line(playing: Option<std::time::Duration>) -> String {
        let wave = media::Waveform {
            bars: vec![1, 4, 8, 6, 2, 7, 3, 5],
            millis: 8_000,
        };
        draw_voice(&wave, playing, "голосовое.wav", Theme::default())
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

        // На середине записи ровно половина столбиков должна быть цветной:
        // граница между цветами и есть указатель воспроизведения.
        let spans = draw_voice(
            &wave,
            Some(std::time::Duration::from_secs(4)),
            "г.wav",
            Theme::default(),
        );
        let played = spans
            .iter()
            .skip(1) // первый — значок ♪
            .take(8)
            .filter(|span| span.style.fg != Some(theme::LINE))
            .count();

        assert_eq!(played, 4, "закрашено не половина: {played}");
    }

    #[test]
    fn a_voice_that_is_not_playing_is_not_painted() {
        let wave = media::Waveform {
            bars: vec![4; 8],
            millis: 8_000,
        };

        let spans = draw_voice(&wave, None, "г.wav", Theme::default());
        let played = spans
            .iter()
            .skip(1)
            .take(8)
            .filter(|span| span.style.fg != Some(theme::LINE))
            .count();

        assert_eq!(played, 0, "график закрашен, хотя ничего не играет");
    }

    #[test]
    fn a_fresh_message_flashes_and_fades() {
        let now = std::time::Instant::now();

        assert!(flash(now, now).is_some_and(|light| light > 0.9));
        assert_eq!(flash(now - FLASH, now), None);
    }
}
