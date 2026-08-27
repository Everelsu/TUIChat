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

use common::Attachment;
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
/// Годится и картинка, и голосовое — как готовое с диска, так и только что
/// записанное с микрофона.
pub fn upload(base: &str, path: &std::path::Path, limit: usize) -> Result<Attachment, String> {
    // Размер узнаём до чтения: тянуть в память гигабайт, чтобы потом сказать
    // «не влезло», — худший способ отказать.
    if let Ok(meta) = std::fs::metadata(path) {
        fits(meta.len() as usize, limit)?;
    }
    let bytes = std::fs::read(path).map_err(|err| format!("не удалось прочитать файл: {err}"))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "файл".to_string());
    upload_bytes(base, &name, bytes, limit)
}

/// Влезает ли файл в потолок сервера.
///
/// Мегабайты, а не килобайты: на сотне мегабайт число в килобайтах человеку
/// уже ничего не говорит.
fn fits(size: usize, limit: usize) -> Result<(), String> {
    if size <= limit {
        return Ok(());
    }
    const MB: usize = 1024 * 1024;
    Err(format!(
        "файл слишком большой: {} МБ при потолке {} МБ у этого сервера",
        size.div_ceil(MB),
        limit / MB
    ))
}

/// То же самое, но для того, что и так уже в памяти: записанного голосового.
pub fn upload_bytes(
    base: &str,
    name: &str,
    bytes: Vec<u8>,
    limit: usize,
) -> Result<Attachment, String> {
    fits(bytes.len(), limit)?;

    let Some(rest) = base.strip_prefix("http://") else {
        return Err("отправка по https из терминала пока не поддерживается".into());
    };
    let authority = rest.trim_end_matches('/');

    let mut stream = std::net::TcpStream::connect(authority)
        .map_err(|err| format!("не удалось подключиться: {err}"))?;
    let head = format!(
        "POST /upload?name={} HTTP/1.1\r\nHost: {authority}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        escape(name),
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

/// Схема, которой помечен адрес, доступный только через туннель.
const TUNNEL: &str = "iroh://";

/// Разбирает `iroh://<тикет>/путь` на тикет и путь.
fn split_tunnel(url: &str) -> Option<(&str, String)> {
    let rest = url.strip_prefix(TUNNEL)?;
    match rest.split_once('/') {
        Some((ticket, path)) => Some((ticket, format!("/{path}"))),
        None => Some((rest, "/".to_string())),
    }
}

/// Качает вложение, откуда бы оно ни лежало: по обычному http или через
/// туннель.
///
/// Разделение спрятано здесь, а не разбросано по вызывающим: тем всё равно,
/// каким путём пришли байты.
pub async fn fetch_any(url: String) -> Result<Vec<u8>, String> {
    if let Some((ticket, path)) = split_tunnel(&url) {
        let request = format!(
            "GET {path} HTTP/1.1
Host: localhost
Accept: */*
Connection: close

"
        );
        return through_tunnel(ticket, request.into_bytes(), Vec::new()).await;
    }

    // Обычный http блокирует, поэтому уходит в отдельный поток: интерфейс
    // должен продолжать отвечать.
    tokio::task::spawn_blocking(move || fetch(&url))
        .await
        .unwrap_or_else(|err| Err(format!("скачивание сорвалось: {err}")))
}

/// Отправляет файл — по http или через туннель.
pub async fn upload_any(
    base: String,
    name: String,
    bytes: Vec<u8>,
    limit: usize,
) -> Result<Attachment, String> {
    fits(bytes.len(), limit)?;

    if let Some((ticket, _)) = split_tunnel(&base) {
        let head = format!(
            "POST /upload?name={} HTTP/1.1
Host: localhost
Content-Length: {}
Connection: close

",
            escape(&name),
            bytes.len()
        );
        let body = through_tunnel(ticket, head.into_bytes(), bytes).await?;
        return serde_json::from_slice(&body)
            .map_err(|err| format!("непонятный ответ сервера: {err}"));
    }

    tokio::task::spawn_blocking(move || upload_bytes(&base, &name, bytes, limit))
        .await
        .unwrap_or_else(|err| Err(format!("отправка сорвалась: {err}")))
}

/// Один HTTP-запрос через трубу: пишем запрос, дочитываем ответ до конца.
///
/// Каждый раз новый поток, а не общий с перепиской: смешивать в одном потоке
/// кадры WebSocket и HTTP нельзя, да и качать вложение параллельно разговору
/// иначе не вышло бы.
async fn through_tunnel(ticket: &str, head: Vec<u8>, body: Vec<u8>) -> Result<Vec<u8>, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut duplex = crate::tunnel::connect(ticket).await?;
    duplex
        .write_all(&head)
        .await
        .map_err(|err| format!("не удалось отправить запрос: {err}"))?;
    if !body.is_empty() {
        duplex
            .write_all(&body)
            .await
            .map_err(|err| format!("не удалось отправить файл: {err}"))?;
    }
    duplex
        .flush()
        .await
        .map_err(|err| format!("не удалось отправить запрос: {err}"))?;

    let mut response = Vec::new();
    duplex
        .read_to_end(&mut response)
        .await
        .map_err(|err| format!("не удалось прочитать ответ: {err}"))?;

    split_response(&response)
}

/// Форма волны голосового: столбики и длительность.
///
/// Считается по настоящим отсчётам, а не рисуется для красоты: показывать
/// выдуманный график там, где человек ждёт увидеть свой голос, — обман, по
/// которому потом нельзя понять, где в записи пауза, а где речь.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waveform {
    /// Громкость по столбикам, 0..=8 — ровно столько ступеней у символов
    /// от `▁` до `█`.
    pub bars: Vec<u8>,
    /// Длительность в миллисекундах, посчитанная по заголовку.
    pub millis: u64,
}

/// Сколько столбиков рисуем. Больше в ленту всё равно не влезает.
const WAVE_BARS: usize = 28;

/// Достаёт форму волны из wav.
///
/// Разбираем заголовок руками: тянуть ради двух полей разборщик форматов
/// незачем, а чужие форматы сюда и не попадают — в wav пишем мы сами.
/// Не wav (браузерные webm и ogg) вернут `None`: их без декодера не прочесть,
/// и это честнее, чем нарисовать что попало.
pub fn waveform(bytes: &[u8]) -> Option<Waveform> {
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }

    // Идём по кускам: между заголовком и данными бывает что угодно.
    let mut at = 12usize;
    let mut channels = 1u16;
    let mut rate = 16_000u32;
    let mut bits = 16u16;
    let mut data: Option<&[u8]> = None;

    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        let body = at + 8;
        let end = body.checked_add(size)?.min(bytes.len());

        if id == b"fmt " && end - body >= 16 {
            channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().ok()?).max(1);
            rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().ok()?).max(1);
            bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into().ok()?);
        } else if id == b"data" {
            data = Some(&bytes[body..end]);
        }

        // Куски выровнены по чётной границе.
        at = body + size + (size & 1);
    }

    let data = data?;
    if bits != 16 || data.len() < 2 {
        return None;
    }

    let frames = data.len() / 2 / channels as usize;
    let millis = (frames as u64 * 1000) / rate as u64;

    // По каждому окну берём пик: именно он виден на глаз как громкость.
    let per_bar = (data.len() / 2 / WAVE_BARS).max(1);
    let mut peaks: Vec<u16> = Vec::with_capacity(WAVE_BARS);
    for window in data.chunks(per_bar * 2).take(WAVE_BARS) {
        let peak = window
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]).unsigned_abs())
            .max()
            .unwrap_or(0);
        peaks.push(peak);
    }

    // Нормируем по самому громкому месту: тихая запись иначе выглядела бы
    // ровной полоской, хотя речь в ней слышна.
    let loudest = peaks.iter().copied().max().unwrap_or(0).max(1);
    let bars = peaks
        .iter()
        .map(|peak| ((*peak as u32 * 8) / loudest as u32).min(8) as u8)
        .collect();

    Some(Waveform { bars, millis })
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
mod wave_tests {
    use super::*;

    /// Собирает wav 16 кГц моно из готовых отсчётов.
    fn wav(samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // моно
        out.extend_from_slice(&16_000u32.to_le_bytes());
        out.extend_from_slice(&32_000u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn duration_comes_from_the_header() {
        // Секунда при 16 кГц — ровно 16000 отсчётов.
        let wave = waveform(&wav(&vec![0i16; 16_000])).expect("wav не разобран");

        assert_eq!(wave.millis, 1000);
        assert_eq!(wave.bars.len(), WAVE_BARS);
    }

    #[test]
    fn loud_places_are_taller_than_quiet_ones() {
        // Первая половина тихая, вторая громкая: график обязан это показать,
        // иначе по нему нельзя понять, где в записи речь.
        let mut samples = vec![100i16; 16_000];
        samples.extend(std::iter::repeat_n(20_000i16, 16_000));

        let wave = waveform(&wav(&samples)).expect("wav не разобран");

        let half = wave.bars.len() / 2;
        let quiet: u32 = wave.bars[..half].iter().map(|b| *b as u32).sum();
        let loud: u32 = wave.bars[half..].iter().map(|b| *b as u32).sum();
        assert!(
            loud > quiet * 2,
            "тихо {quiet}, громко {loud}: {:?}",
            wave.bars
        );
    }

    #[test]
    fn a_quiet_recording_still_shows_something() {
        // Нормировка по самому громкому: иначе тихая запись выглядела бы
        // ровной полоской, хотя речь в ней слышна.
        let mut samples = vec![0i16; 8_000];
        samples.extend(std::iter::repeat_n(300i16, 8_000));

        let wave = waveform(&wav(&samples)).expect("wav не разобран");

        assert_eq!(wave.bars.iter().copied().max(), Some(8));
    }

    #[test]
    fn what_is_not_a_wav_is_refused_rather_than_invented() {
        // Браузерные webm и ogg без декодера не прочесть. Нарисовать по ним
        // «что-нибудь» значило бы показать человеку выдуманный голос.
        assert!(waveform(b"OggS not a wav at all").is_none());
        assert!(waveform(&[]).is_none());
        assert!(waveform(&wav(&[])).is_none());
    }
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
        let error = upload(
            "http://127.0.0.1:1",
            std::path::Path::new("нет-такого.png"),
            common::validate::MAX_UPLOAD_BYTES,
        )
        .unwrap_err();

        assert!(error.contains("прочитать"), "невнятная причина: {error}");
    }

    #[test]
    fn body_is_separated_from_headers() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n\r\n\x89PNG";

        assert_eq!(split_response(response).unwrap(), b"\x89PNG");
    }
}
