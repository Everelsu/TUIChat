//! Показ картинок средствами самого терминала.
//!
//! Если терминал умеет графику — kitty, iTerm2, sixel, — фотография выглядит
//! фотографией. Если нет, остаётся запасной путь из [`crate::media`]:
//! полублоки, которые работают везде, где есть 24-битный цвет.
//!
//! Реализация kitty здесь построена на unicode-плейсхолдерах: картинка
//! привязана к ячейкам буфера и исчезает сама, когда их перерисуют. Поэтому
//! закрытие просмотра не требует отдельной команды «сотри картинку», а
//! переписка под ней не превращается в мешанину.

use std::collections::HashMap;

use image::{DynamicImage, RgbImage};
use ratatui::{Frame, layout::Rect};
use ratatui_image::{
    FilterType, Resize, StatefulImage, picker::Picker, picker::ProtocolType,
    protocol::StatefulProtocol,
};
use uuid::Uuid;

/// Сколько закодированных картинок держим наготове.
///
/// В ленту одновременно попадает две-три, но при быстрой прокрутке кодировать
/// их заново на каждом кадре — заметная работа впустую.
const KEEP_PREPARED: usize = 8;

pub struct Images {
    picker: Option<Picker>,
    /// Уже закодированные картинки: кодирование не бесплатное, а кадров
    /// восемь в секунду.
    prepared: HashMap<Uuid, StatefulProtocol>,
    /// Порядок обращения — по нему вытесняем давно не нужные.
    order: Vec<Uuid>,
}

impl Images {
    /// Спрашивает у терминала, что он умеет.
    ///
    /// Вызывать строго до запуска чтения клавиатуры: запрос печатает
    /// escape-последовательность и ждёт ответ из того же stdin. Если читать
    /// его будет кто-то ещё, ответ пропадёт, а в переписку прилетит мусор.
    pub fn probe() -> Self {
        // Запрос делаем, только если по обе стороны настоящий терминал.
        // Иначе ответа не будет никогда: на Windows опрос в этом случае
        // уходит в бесконечный цикл и съедает ядро целиком.
        if !std::io::IsTerminal::is_terminal(&std::io::stdin())
            || !std::io::IsTerminal::is_terminal(&std::io::stdout())
        {
            return Self::disabled();
        }

        Self {
            picker: Picker::from_query_stdio().ok(),
            prepared: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Без графики: для тестов и для терминалов, которые не ответили.
    pub fn disabled() -> Self {
        Self {
            picker: None,
            prepared: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Умеет ли терминал настоящую графику.
    ///
    /// От этого зависит, показывать ли картинки прямо в ленте: полублоками
    /// миниатюра размером с несколько строк превращается в цветной шум.
    pub fn has_graphics(&self) -> bool {
        !matches!(
            self.picker.as_ref().map(Picker::protocol_type),
            None | Some(ProtocolType::Halfblocks)
        )
    }

    /// Название протокола — для строки состояния и для отчётов о проблемах.
    pub fn kind(&self) -> &'static str {
        match self.picker.as_ref().map(Picker::protocol_type) {
            Some(ProtocolType::Kitty) => "kitty",
            Some(ProtocolType::Sixel) => "sixel",
            Some(ProtocolType::Iterm2) => "iterm2",
            Some(ProtocolType::Halfblocks) => "полублоки",
            _ => "полублоки",
        }
    }

    /// Рисует картинку. `false` — терминал графики не умеет, рисовать нечем.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, id: Uuid, image: &RgbImage) -> bool {
        let Some(picker) = &self.picker else {
            return false;
        };
        if area.width == 0 || area.height == 0 {
            return true;
        }

        self.prepared.entry(id).or_insert_with(|| {
            let owned = DynamicImage::ImageRgb8(image.clone());
            picker.new_resize_protocol(owned)
        });
        self.order.retain(|known| *known != id);
        self.order.push(id);
        while self.order.len() > KEEP_PREPARED {
            let oldest = self.order.remove(0);
            self.prepared.remove(&oldest);
        }

        let Some(protocol) = self.prepared.get_mut(&id) else {
            return false;
        };

        // Fit, а не Crop: обрезать чужую фотографию по краям хуже, чем показать
        // её целиком и поменьше.
        let widget = StatefulImage::<StatefulProtocol>::default()
            .resize(Resize::Fit(Some(FilterType::Triangle)));
        frame.render_stateful_widget(widget, area, protocol);
        true
    }

    /// Забывает всё закодированное: например, когда очистили переписку.
    pub fn forget(&mut self) {
        self.prepared.clear();
        self.order.clear();
    }
}

impl Default for Images {
    fn default() -> Self {
        Self::disabled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_terminal_there_is_nothing_to_draw_with() {
        let mut images = Images::disabled();
        let image = RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3]));

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(10, 5)).unwrap();
        let mut drawn = true;
        terminal
            .draw(|frame| {
                drawn = images.render(frame, frame.area(), Uuid::nil(), &image);
            })
            .unwrap();

        // Отказ важен: по нему вызывающий код переходит на полублоки.
        assert!(!drawn);
        assert_eq!(images.kind(), "полублоки");
        // И миниатюры в ленте в таком терминале не показываем.
        assert!(!images.has_graphics());
    }

    #[test]
    fn probing_outside_a_terminal_returns_immediately() {
        // В тестах stdin не терминал. Запрос обязан даже не начинаться:
        // на Windows он в этом случае уходит в бесконечный цикл.
        let started = std::time::Instant::now();

        let images = Images::probe();

        assert_eq!(images.kind(), "полублоки");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "опрос терминала всё-таки состоялся"
        );
    }
}
