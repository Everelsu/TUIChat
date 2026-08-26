//! Показ картинок в терминале.
//!
//! Рисуем полублоками: символ `▀` делит ячейку пополам, верхняя половина
//! красится цветом символа, нижняя — цветом фона. Так одна строка терминала
//! даёт два ряда пикселей, а работает это везде, где есть 24-битный цвет —
//! включая Windows Terminal, VS Code, tmux и ssh.
//!
//! Протоколы вроде kitty или sixel дают настоящее разрешение, но живут в
//! считаных терминалах и требуют аккуратной перерисовки при прокрутке.

use std::io::{Read, Write};

use image::{RgbImage, imageops::FilterType};
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

/// Отделяет тело от заголовков и проверяет код ответа.
fn split_response(response: &[u8]) -> Result<Vec<u8>, String> {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "сервер ответил неполным заголовком".to_string())?;

    let head = String::from_utf8_lossy(&response[..separator]);
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200") {
        // Файл мог быть вытеснен из хранилища сервера — это самый частый случай.
        return Err(format!("сервер ответил: {}", status.trim()));
    }

    Ok(response[separator + 4..].to_vec())
}

pub fn decode(bytes: &[u8]) -> Result<RgbImage, String> {
    image::load_from_memory(bytes)
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
    fn body_is_separated_from_headers() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n\r\n\x89PNG";

        assert_eq!(split_response(response).unwrap(), b"\x89PNG");
    }
}
