//! Оформление: палитра, градиенты и мелкая графика, из которой собран экран.
//!
//! Цвета заданы в RGB, а не именами: именованные восемь цветов терминалы
//! перекрашивают по своей теме, и оттенки разъезжаются от машины к машине.
//!
//! Палитра холодная и контрастная — так интерфейс читается и на чёрном фоне,
//! и на тёмно-синем. Фон почти нигде не заливается: заливка спорит с обоями
//! терминала, а границы лучше держать цветом текста и полублоками.

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

/// Основной текст. Не чисто белый: белым остаются только вспышки, иначе
/// подсветке нечем выделиться.
pub const TEXT: Color = Color::Rgb(228, 228, 239);
/// Второй по важности текст: время, размеры, пояснения.
pub const SUBTLE: Color = Color::Rgb(150, 150, 178);
/// Всё, что должно быть видно, но не должно ловить взгляд.
pub const MUTED: Color = Color::Rgb(101, 101, 130);
/// Линии рамок в спокойном состоянии.
pub const LINE: Color = Color::Rgb(64, 64, 88);
/// Подложка полос и плашек. Единственный фон, который мы себе позволяем.
pub const SURFACE: Color = Color::Rgb(32, 32, 46);
/// Текст поверх яркой плашки: на насыщенном фоне светлый текст слепнет.
pub const INK: Color = Color::Rgb(16, 16, 26);

pub const OK: Color = Color::Rgb(18, 199, 143);
pub const WARN: Color = Color::Rgb(255, 194, 75);
pub const ERR: Color = Color::Rgb(255, 92, 122);
/// Упоминание — тот же жёлтый, что и предупреждение: в обоих случаях строка
/// адресована лично, и цвет должен быть один.
pub const MENTION: Color = WARN;

/// Палитра ников. Те же цвета живут в веб-клиенте, чтобы человек выглядел
/// одинаково в терминале и в браузере.
pub const NICKS: [Color; 8] = [
    Color::Rgb(0, 211, 242),   // малибу
    Color::Rgb(18, 199, 143),  // гуакамоле
    Color::Rgb(255, 194, 75),  // цедра
    Color::Rgb(180, 140, 255), // сирень
    Color::Rgb(255, 122, 190), // долли
    Color::Rgb(107, 208, 255), // лёд
    Color::Rgb(255, 141, 106), // коралл
    Color::Rgb(126, 226, 168), // мята
];

/// Тема: три цвета акцента, которыми красится всё остальное.
///
/// Меняется прямо на вкладке «вид» и переживает перезапуск. Смысл не в
/// украшательстве: на разных обоях терминала один и тот же фиолетовый
/// читается по-разному, и человеку нужно уметь выбрать различимый.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// Фиолетовый в розовый — так выглядит семейство charm.
    #[default]
    Charple,
    /// Розовый в янтарь: теплее и заметнее на синем фоне.
    Dolly,
    /// Бирюза в зелень: холодная тема для светлых обоев.
    Mint,
    /// Огонь: янтарь в красный, самая контрастная на чёрном.
    Ember,
}

impl Theme {
    pub const ALL: [Theme; 4] = [Theme::Charple, Theme::Dolly, Theme::Mint, Theme::Ember];

    /// Имя для файла настроек.
    pub fn name(self) -> &'static str {
        match self {
            Theme::Charple => "charple",
            Theme::Dolly => "dolly",
            Theme::Mint => "mint",
            Theme::Ember => "ember",
        }
    }

    /// Имя для экрана: в настройках человек выбирает глазами, а не по коду.
    pub fn title(self) -> &'static str {
        match self {
            Theme::Charple => "сирень",
            Theme::Dolly => "долли",
            Theme::Mint => "мята",
            Theme::Ember => "уголь",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_lowercase();
        Theme::ALL.into_iter().find(|theme| theme.name() == value)
    }

    /// Следующая тема по кругу: стрелки в настройках листают список, и
    /// упираться в край незачем.
    pub fn shift(self, back: bool) -> Self {
        let at = Theme::ALL.iter().position(|&t| t == self).unwrap_or(0);
        let len = Theme::ALL.len();
        let next = if back { at + len - 1 } else { at + 1 };
        Theme::ALL[next % len]
    }

    /// Главный цвет: рамки в фокусе, активная вкладка, свои сообщения.
    pub fn primary(self) -> Color {
        match self {
            Theme::Charple => Color::Rgb(122, 92, 255),
            Theme::Dolly => Color::Rgb(255, 106, 193),
            Theme::Mint => Color::Rgb(0, 202, 190),
            Theme::Ember => Color::Rgb(255, 148, 61),
        }
    }

    /// Второй конец градиента.
    pub fn secondary(self) -> Color {
        match self {
            Theme::Charple => Color::Rgb(255, 122, 190),
            Theme::Dolly => Color::Rgb(255, 178, 92),
            Theme::Mint => Color::Rgb(126, 226, 168),
            Theme::Ember => Color::Rgb(255, 92, 122),
        }
    }

    /// Самый светлый оттенок: вспышки и бегущий блик.
    pub fn glow(self) -> Color {
        match self {
            Theme::Charple => Color::Rgb(214, 200, 255),
            Theme::Dolly => Color::Rgb(255, 214, 235),
            Theme::Mint => Color::Rgb(198, 255, 238),
            Theme::Ember => Color::Rgb(255, 226, 178),
        }
    }
}

/// Раскладывает цвет на составляющие. Не-RGB сюда почти не попадает, но на
/// всякий случай отдаём серый, а не паникуем: цвет ника человек задаёт руками.
fn parts(color: Color) -> (f32, f32, f32) {
    match color {
        Color::Rgb(r, g, b) => (r as f32, g as f32, b as f32),
        _ => (128.0, 128.0, 128.0),
    }
}

/// Смешивает два цвета. `t = 0` — первый, `t = 1` — второй.
pub fn mix(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (r1, g1, b1) = parts(from);
    let (r2, g2, b2) = parts(to);
    Color::Rgb(
        (r1 + (r2 - r1) * t) as u8,
        (g1 + (g2 - g1) * t) as u8,
        (b1 + (b2 - b1) * t) as u8,
    )
}

/// Гасит цвет к темноте: приглушённый вариант любого акцента.
pub fn dim(color: Color, amount: f32) -> Color {
    mix(color, Color::Rgb(18, 18, 26), amount)
}

/// Замедление к концу: движение, которое тормозит, читается живым, а
/// равномерное — механическим.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
}

/// Треугольная волна 0 -> 1 -> 0 по номеру кадра: ею дышат точки состояния.
pub fn pulse(tick: u64, period: u64) -> f32 {
    let period = period.max(2);
    let at = (tick % period) as f32 / period as f32;
    if at < 0.5 { at * 2.0 } else { (1.0 - at) * 2.0 }
}

/// Кадры спиннера: точечные символы крутятся плавнее, чем палочка из четырёх
/// косых.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick as usize / 2) % SPINNER.len()]
}

/// Ступени столбика: восемь уровней плюс тишина. Ими рисуются и голосовые,
/// и полоска громкости.
pub const STEPS: [&str; 9] = ["▁", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Текст с градиентом посимвольно.
///
/// Каждая буква — свой Span: дороже, чем одна строка, но заголовков на экране
/// единицы, а переливающаяся надпись сразу говорит, где здесь главное.
pub fn gradient(text: &str, from: Color, to: Color) -> Vec<Span<'static>> {
    gradient_styled(text, from, to, Style::new())
}

pub fn gradient_bold(text: &str, from: Color, to: Color) -> Vec<Span<'static>> {
    gradient_styled(text, from, to, Style::new().add_modifier(Modifier::BOLD))
}

fn gradient_styled(text: &str, from: Color, to: Color, base: Style) -> Vec<Span<'static>> {
    let count = text.chars().count();
    if count == 0 {
        return Vec::new();
    }
    text.chars()
        .enumerate()
        .map(|(at, ch)| {
            let t = if count == 1 {
                0.0
            } else {
                at as f32 / (count - 1) as f32
            };
            Span::styled(ch.to_string(), base.fg(mix(from, to, t)))
        })
        .collect()
}

/// Ширина бегущего блика в долях строки.
const BAND: f32 = 0.22;

/// Тот же градиент, но по нему бежит светлое пятно.
///
/// `phase` — где сейчас центр пятна, от -0.3 до 1.3: с запасом по краям,
/// чтобы блик выезжал из-за границы, а не вспыхивал посередине. `offset`
/// и `total` сдвигают позицию буквы — ими многострочная надпись красится
/// как одна.
pub fn shimmer(
    text: &str,
    from: Color,
    to: Color,
    glow: Color,
    phase: f32,
    offset: usize,
    total: usize,
) -> Vec<Span<'static>> {
    let total = total.max(1);
    text.chars()
        .enumerate()
        .map(|(at, ch)| {
            let t = (offset + at) as f32 / total as f32;
            let base = mix(from, to, t);
            // Блеск спадает к краям пятна: резкая граница выглядит как
            // испорченный кадр, а не как отсвет.
            let distance = (t - phase).abs() / BAND;
            let light = (1.0 - distance).max(0.0);
            Span::styled(
                ch.to_string(),
                Style::new().fg(mix(base, glow, light * light)),
            )
        })
        .collect()
}

/// Плашка со скруглёнными краями: половинки блоков подделывают закругление
/// в любом шрифте, а не только там, где есть символы powerline.
pub fn pill(label: &str, background: Color, foreground: Color) -> Vec<Span<'static>> {
    vec![
        Span::styled("▐", Style::new().fg(background)),
        Span::styled(
            label.to_string(),
            Style::new()
                .bg(background)
                .fg(foreground)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("▌", Style::new().fg(background)),
    ]
}

/// Клавиша и то, что она делает: из таких пар собрана нижняя подсказка.
///
/// Клавиша яркая, пояснение приглушённое — глаз бежит по клавишам, а читает
/// только там, где остановился.
pub fn key_hint(key: &str, what: &str, accent: Color) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            key.to_string(),
            Style::new().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {what}"), Style::new().fg(MUTED)),
    ]
}

/// Разделитель между подсказками.
pub fn separator() -> Span<'static> {
    Span::styled("  ·  ", Style::new().fg(LINE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixing_reaches_both_ends() {
        let from = Color::Rgb(0, 0, 0);
        let to = Color::Rgb(255, 255, 255);

        assert_eq!(mix(from, to, 0.0), from);
        assert_eq!(mix(from, to, 1.0), to);
        // За пределами отрезка цвет не улетает: доля зажимается.
        assert_eq!(mix(from, to, 5.0), to);
    }

    #[test]
    fn gradient_keeps_every_character() {
        let spans = gradient("привет", Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255));

        assert_eq!(spans.len(), 6);
        let text: String = spans.iter().map(|span| span.content.to_string()).collect();
        assert_eq!(text, "привет");
        assert_ne!(spans[0].style.fg, spans[5].style.fg);
    }

    #[test]
    fn empty_gradient_is_empty() {
        assert!(gradient("", TEXT, TEXT).is_empty());
    }

    #[test]
    fn shimmer_lights_where_the_phase_points() {
        let theme = Theme::Charple;
        let lit = shimmer(
            "ааааааааа",
            theme.primary(),
            theme.secondary(),
            theme.glow(),
            0.0,
            0,
            9,
        );

        // Блик в начале строки: первая буква светлее последней.
        let brightness = |span: &Span| match span.style.fg {
            Some(Color::Rgb(r, g, b)) => u32::from(r) + u32::from(g) + u32::from(b),
            _ => 0,
        };
        assert!(brightness(&lit[0]) > brightness(&lit[8]));
    }

    #[test]
    fn themes_cycle_both_ways() {
        assert_eq!(Theme::Charple.shift(false), Theme::Dolly);
        assert_eq!(Theme::Charple.shift(true), Theme::Ember);
        // По кругу: с последней вперёд — снова первая.
        assert_eq!(Theme::Ember.shift(false), Theme::Charple);
    }

    #[test]
    fn themes_survive_a_round_trip() {
        for theme in Theme::ALL {
            assert_eq!(Theme::parse(theme.name()), Some(theme));
        }
        assert_eq!(Theme::parse("нет такой"), None);
    }

    #[test]
    fn pulse_breathes_between_the_ends() {
        let values: Vec<f32> = (0..8).map(|tick| pulse(tick, 8)).collect();

        assert!(values[0] < 0.01, "{values:?}");
        assert!(values[4] > 0.99, "{values:?}");
        assert!(values[7] < 0.5, "{values:?}");
    }
}
