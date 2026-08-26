//! Терминальный клиент чата.
//!
//! Разделён на три части: `app` — состояние и переходы (без ввода-вывода),
//! `net` — соединение с сервером, `ui` — отрисовка. Логика не знает ни про
//! терминал, ни про сокет, поэтому проверяется обычными тестами.

pub mod app;
pub mod config;
pub mod files;
pub mod host;
pub mod images;
pub mod media;
pub mod net;
pub mod sound;
pub mod ui;
