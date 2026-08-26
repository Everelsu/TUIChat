//! Картинки и файлы: показ в терминале и отправка на сервер.
//!
//! Рисуем полублоками: символ `▀` делит ячейку пополам, верхняя половина
//! красится цветом символа, нижняя — цветом фона. Так одна строка терминала
//! даёт два ряда пикселей, а работает это везде, где есть 24-битный цвет —
//! включая Windows Terminal, VS Code, tmux и ssh.
//!
//! Протоколы вроде kitty или sixel дают настоящее разрешение, но живут в
//! считаных терминалах и требуют аккуратной перерисовки при прокрутке.

use std::io::{Cursor, Read, Write};

use common::{Attachment, validate};
use image::{ImageReader, Limits, RgbImage, imageops::FilterType};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

/// Верхняя половина ячейки.
const UPPER_HALF: &str = "\u{2580}";

/// Скачивает файл обычным HTTP-запросом.
///
/// Своя реализация вместо http-клиента: нужен ровно один GET к своему же
/// серверу, а тянуть ради этого зависимость с TLS-стеком незачем.
pub fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let Some(rest) = url.strip_prefix("http://") else {
        // Для https нужен TLS-клиент, а с самоподписанным сертификатом ещё и
        // решение, доверять ли ему. Пока честно говорим, что не умеем.
        return Err("показ по https из терминала пока не поддерживается, /open откроет во внешней программе".into());
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };

    let mut stream = std::net::TcpStream::connect(authority)
        .map_err(|err| format!("не удалось подключиться: {err}"))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: image/*\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("не удалось отправить запрос: {err}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|err| format!("не удалось прочитать ответ: {err}"))?;

    split_response(&response)
}

/// Отправляет файл на сервер и возвращает его описание.
///
/// Записать можно что угодно из поддерживаемых типов: картинку, чтобы её
/// увидели в браузере, или готовое голосовое. Запись с микрофона из терминала
/// не делается — по ssh микрофона всё равно нет, а тащить звуковой стек ради
/// локального случая слишком дорого.
pub fn upload(base: &str, path: &std::path::Path) -> Result<Attachment, String> {
    let bytes = std::fs::read(path).map_err(|err| format!("не удалось прочитать файл: {err}"))?;
    if bytes.len() > validate::MAX_UPLOAD_BYTES {
        return Err(format!(
            "файл слишком большой: {} КБ при потолке {} КБ",
            bytes.len() / 1024,
            validate::MAX_UPLOAD_BYTES / 1024
        ));
    }

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "файл".to_string());
    let Some(rest) = base.strip_prefix("http://") else {
        return Err("отправка по https из терминала пока не поддерживается".into());
    };
    let authority = rest.trim_end_matches('/');

    let mut stream = std::net::TcpStream::connect(authority)
        .map_err(|err| format!("не удалось подключиться: {err}"))?;
    let head = format!(
        "POST /upload?name={} HTTP/1.1\r\nHost: {authority}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        escape(&name),
        bytes.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(&bytes))
        .map_err(|err| format!("не удалось отправить файл: {err}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|err| format!("не удалось прочитать ответ: {err}"))?;

    let body = split_response(&response)?;
    serde_json::from_slice(&body).map_err(|err| format!("непонятный ответ сервера: {err}"))
}

/// Экранирует имя файла для строки запроса: в нём бывают пробелы и кириллица.
fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                escaped.push(*byte as char);
            }
            _ => escaped.push_str(&format!("%{byte:02X}")),
        }
    }
    escaped
}

/// Отделяет тело от заголовков и проверяет код ответа.
fn split_response(response: &[u8]) -> Result<Vec<u8>, String> {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "сервер ответил неполным заголовком".to_string())?;

    let head = String::from_utf8_lossy(&response[..separator]);
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200") {
        // Тело ответа сервер пишет по-человечески: «файл слишком большой»
        // понятнее, чем «HTTP/1.1 413».
        let body = String::from_utf8_lossy(&response[separator + 4..]);
        let body = body.trim();
        if !body.is_empty() && body.len() < 200 {
            return Err(body.to_string());
        }
        return Err(format!("сервер ответил: {}", status.trim()));
    }

    Ok(response[separator + 4..].to_vec())
}

/// Потолок стороны картинки. Снимок с любого телефона влезает с запасом.
const MAX_SIDE: u32 = 12_000;

/// Потолок памяти на разбор одной картинки.
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

pub fn decode(bytes: &[u8]) -> Result<RgbImage, String> {
    // Пять мегабайт сжатого файла разворачиваются в гигабайты пикселей:
    // одна такая «бомба» положила бы клиент по памяти, а это уже не падение
    // окна, а убитый процесс без всякого сообщения.
    decode_within(bytes, limits())
}

/// Ограничения на разбор.
///
/// Собираются по полям, а не литералом: `Limits` помечен `non_exhaustive`,
/// и синтаксис обновления структуры к нему неприменим.
#[allow(clippy::field_reassign_with_default)]
fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    limits
}

fn decode_within(bytes: &[u8], limits: Limits) -> Result<RgbImage, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| format!("не удалось определить формат: {err}"))?;
    reader.limits(limits);

    reader
        .decode()
        .map(|image| image.to_rgb8())
        .map_err(|err| format!("не удалось разобрать картинку: {err}"))
}

/// Готовит строки для вывода в область `cols` × `rows` ячеек.
///
/// Пропорции сохраняются: растянутая картинка выглядит хуже, чем маленькая.
pub fn to_lines(image: &RgbImage, cols: u16, rows: u16) -> Vec<Line<'static>> {
    if cols == 0 || rows == 0 || image.width() == 0 || image.height() == 0 {
        return Vec::new();
    }

    // В одной ячейке два пикселя по вертикали, поэтому доступная высота
    // в пикселях вдвое больше числа строк.
    let (width, height) = fit(
        image.width(),
        image.height(),
        u32::from(cols),
        u32::from(rows) * 2,
    );
    let resized = image::imageops::resize(image, width, height, FilterType::Triangle);

    let mut lines = Vec::new();
    for y in (0..height).step_by(2) {
        let mut spans = Vec::with_capacity(width as usize);
        for x in 0..width {
            let top = resized.get_pixel(x, y).0;
            // У картинки нечётной высоты нижней половины может не быть:
            // повторяем верхнюю, чтобы не рисовать чёрную полосу.
            let bottom = resized.get_pixel(x, (y + 1).min(height - 1)).0;
            spans.push(Span::styled(
                UPPER_HALF,
                Style::new()
                    .fg(Color::Rgb(top[0], top[1], top[2]))
                    .bg(Color::Rgb(bottom[0], bottom[1], bottom[2])),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Вписывает размеры в рамку, сохраняя пропорции и не увеличивая картинку.
fn fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let scale = f64::from(max_width) / f64::from(width);
    let scale = scale.min(f64::from(max_height) / f64::from(height));
    // Увеличение полублоками выглядит как мозаика, поэтому только уменьшаем.
    let scale = scale.min(1.0);
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, _| {
            image::Rgb([if x % 2 == 0 { 255 } else { 0 }, 128, 0])
        })
    }

    #[test]
    fn one_line_holds_two_rows_of_pixels() {
        let lines = to_lines(&image(4, 4), 4, 2);

        assert_eq!(lines.len(), 2, "четыре пикселя по высоте — две строки");
        assert_eq!(lines[0].spans.len(), 4);
    }

    #[test]
    fn proportions_are_kept() {
        // Широкая картинка в узкой области должна ужаться по ширине,
        // а не растянуться на всю высоту.
        let lines = to_lines(&image(100, 10), 20, 20);

        assert_eq!(lines[0].spans.len(), 20);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn small_image_is_not_blown_up() {
        let lines = to_lines(&image(4, 2), 80, 40);

        // Увеличение полублоками превращает картинку в мозаику.
        assert_eq!(lines[0].spans.len(), 4);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn empty_area_draws_nothing() {
        assert!(to_lines(&image(4, 4), 0, 10).is_empty());
        assert!(to_lines(&image(4, 4), 10, 0).is_empty());
    }

    #[test]
    fn colors_come_from_the_pixels() {
        let lines = to_lines(&image(2, 2), 2, 1);

        let first = &lines[0].spans[0];
        assert_eq!(first.style.fg, Some(Color::Rgb(255, 128, 0)));
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_pixel(width, height, image::Rgb([10, 20, 30]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn oversized_pictures_are_refused_before_they_are_decoded() {
        let bytes = png_bytes(64, 64);

        let mut tiny = Limits::default();
        tiny.max_image_width = Some(8);
        assert!(
            decode_within(&bytes, tiny).is_err(),
            "картинка прошла мимо лимита"
        );
        // Обычная картинка при этом разбирается как ни в чём не бывало.
        assert!(decode(&bytes).is_ok());
    }

    #[test]
    fn garbage_is_reported_not_panicked() {
        let error = decode("это не картинка вовсе".as_bytes()).unwrap_err();

        assert!(!error.is_empty());
    }

    #[test]
    fn https_is_reported_as_unsupported() {
        let error = fetch("https://example/media/1").unwrap_err();

        assert!(error.contains("https"), "невнятная причина: {error}");
    }

    #[test]
    fn error_response_is_explained() {
        let response = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";

        let error = split_response(response).unwrap_err();

        assert!(error.contains("404"), "невнятная причина: {error}");
    }

    #[test]
    fn server_explanation_wins_over_the_status_line() {
        let response =
            "HTTP/1.1 413 Payload Too Large\r\n\r\nфайл слишком большой: 9000 КБ".as_bytes();

        let error = split_response(response).unwrap_err();

        // «413» человеку ничего не говорит, а объяснение сервера — говорит.
        assert!(error.contains("слишком большой"), "пришло: {error}");
    }

    #[test]
    fn file_names_are_escaped_for_the_query() {
        assert_eq!(escape("cat.png"), "cat.png");
        // Пробелы и кириллица в имени не должны ломать строку запроса.
        assert!(!escape("мой кот.png").contains(' '));
        assert!(escape("мой кот.png").contains("%20"));
    }

    #[test]
    fn missing_file_is_reported() {
        let error =
            upload("http://127.0.0.1:1", std::path::Path::new("нет-такого.png")).unwrap_err();

        assert!(error.contains("прочитать"), "невнятная причина: {error}");
    }

    #[test]
    fn body_is_separated_from_headers() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n\r\n\x89PNG";

        assert_eq!(split_response(response).unwrap(), b"\x89PNG");
    }
}
