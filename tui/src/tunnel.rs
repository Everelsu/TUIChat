//! Соединение через интернет, когда у обеих сторон обычный домашний роутер.
//!
//! Проблема, ради которой всё это: два домашних компьютера не могут соединиться
//! напрямую. Друг стучится на публичный адрес, пакет доходит до роутера, а тот
//! не знает, кому из устройств в квартире его отдать, — правила нет, пакет в
//! мусор. Это NAT, и обойти его «более простым протоколом» нельзя: netcat в
//! этом месте упирается ровно в ту же стену.
//!
//! Выход — не ждать входящего соединения, а сделать так, чтобы наружу звонили
//! обе стороны сразу: роутер видит исходящий пакет и сам открывает обратный
//! путь. Этим занимается iroh: он сводит стороны через общий координатор,
//! пробивает NAT, а если у провайдера NAT строгий и пробить не вышло —
//! пропускает трафик через релей. Соединение поднимается в обоих случаях.
//!
//! Сам чат об этом ничего не знает. Через готовый поток идёт тот же WebSocket
//! с тем же JSON, поэтому комнаты, картинки, голосовые и история работают без
//! единой правки: снаружи это просто труба, по которой ходят байты.

use std::time::Duration;

use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Connection, presets},
};
use tokio::io::{Join, join};

/// Имя протокола. Соединения с другим ALPN endpoint даже не примет, так что
/// чужая программа на том же адресе нас не потревожит.
const ALPN: &[u8] = b"tuichat/1";

/// Сколько ждём, пока endpoint доложит о себе координатору.
///
/// Без этого можно выдать человеку тикет, по которому пока никто не достучится:
/// адреса ещё не опубликованы. Ждём с потолком — если сети нет, честнее отдать
/// тикет и сказать правду, чем висеть молча.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);

/// Готовый двусторонний поток к собеседнику.
///
/// Склеен из двух половин QUIC-потока: читающей и пишущей. Для всего
/// остального это обычный сокет.
pub type Duplex = Join<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>;

/// Поднятый туннель. Живёт, пока жив возвращённый `Tunnel`: уронив его,
/// закрываем и endpoint, и все соединения через него.
pub struct Tunnel {
    /// Тикет для друга — он же открытый ключ этой стороны.
    pub ticket: String,
    /// Держим endpoint живым: без него труба закрывается.
    endpoint: Endpoint,
}

/// Открывает туннель к уже поднятому здесь серверу.
///
/// Каждое входящее соединение подключается к своему же серверу на
/// `127.0.0.1:port` и дальше просто переливает байты в обе стороны. То есть
/// это netcat, но с пробиванием NAT и шифрованием.
pub async fn serve(port: u16) -> Result<Tunnel, String> {
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .map_err(|err| format!("не удалось открыть туннель: {err}"))?;

    // Ждём, пока о нас узнает координатор: иначе тикет выдан, а достучаться
    // по нему ещё нельзя.
    let _ = tokio::time::timeout(ONLINE_TIMEOUT, endpoint.online()).await;

    let ticket = endpoint.id().to_string();

    let accepting = endpoint.clone();
    tokio::spawn(async move {
        while let Some(incoming) = accepting.accept().await {
            tokio::spawn(async move {
                // Ошибку одного гостя глотаем: она не должна закрывать трубу
                // для остальных.
                if let Ok(connection) = incoming.await {
                    let _ = pipe_to_local(connection, port).await;
                }
            });
        }
    });

    Ok(Tunnel { ticket, endpoint })
}

impl Tunnel {
    /// Адреса этой стороны целиком: тикет плюс то, где её уже сейчас видно.
    ///
    /// По ним соединяются, минуя координатора, — так тест обходится без сети,
    /// а машины в одной локальной сети находят друг друга сразу.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }
}

/// Переливает одно входящее соединение в местный сервер и обратно.
async fn pipe_to_local(connection: Connection, port: u16) -> Result<(), String> {
    let (mut remote_send, mut remote_recv) = connection
        .accept_bi()
        .await
        .map_err(|err| format!("гость не открыл поток: {err}"))?;

    let mut local = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|err| format!("свой же сервер недоступен: {err}"))?;
    let (mut local_read, mut local_write) = local.split();

    // Конец потока передаём явно в обе стороны. Само по себе переливание
    // байтов об этом не сообщает, а без такого сигнала HTTP-запрос через
    // трубу повисает навсегда: сторона, читающая ответ до закрытия, не узнаёт,
    // что он уже кончился. На переписке это незаметно — там соединение живёт
    // всё время, — зато картинки и голосовые не скачались бы никогда.
    let to_local = async {
        let copied = tokio::io::copy(&mut remote_recv, &mut local_write).await;
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut local_write).await;
        copied
    };
    let to_remote = async {
        let copied = tokio::io::copy(&mut local_read, &mut remote_send).await;
        let _ = remote_send.finish();
        copied
    };
    // Именно `join`, а не `try_join`: обрыв одной стороны не повод бросать
    // вторую недокачанной.
    let _ = tokio::join!(to_local, to_remote);
    Ok(())
}

/// Endpoint этой стороны для исходящих соединений.
///
/// Один на весь процесс, и не ради экономии: endpoint нельзя ронять, пока жив
/// хоть один поток через него — вместе с ним закрывается и соединение. А клиент
/// переподключается сам при каждом обрыве, и заводить endpoint заново на каждую
/// попытку значило бы каждый раз заново привязывать сокеты и здороваться с
/// координатором.
static CLIENT: tokio::sync::OnceCell<Endpoint> = tokio::sync::OnceCell::const_new();

async fn client_endpoint() -> Result<&'static Endpoint, String> {
    CLIENT
        .get_or_try_init(|| async {
            Endpoint::builder(presets::N0)
                .bind()
                .await
                .map_err(|err| format!("не удалось открыть туннель: {err}"))
        })
        .await
}

/// Подключается к другу по тикету.
///
/// Адрес не нужен: по тикету — открытому ключу — координатор сам находит, где
/// сейчас эта сторона. Тем и хорош: адрес дома меняется, ключ нет.
pub async fn connect(ticket: &str) -> Result<Duplex, String> {
    let id = parse_ticket(ticket)?;
    dial(id).await
}

/// То же самое, но когда адреса собеседника уже известны и координатор не
/// нужен: так соединяются в пределах одной машины и одной сети.
pub async fn connect_to(addr: impl Into<EndpointAddr>) -> Result<Duplex, String> {
    dial(addr).await
}

async fn dial(addr: impl Into<EndpointAddr>) -> Result<Duplex, String> {
    let endpoint = client_endpoint().await?;

    let connection = endpoint
        .connect(addr, ALPN)
        .await
        .map_err(|err| format!("друг не отвечает: {err}"))?;

    let (send, recv) = connection
        .open_bi()
        .await
        .map_err(|err| format!("не удалось открыть поток: {err}"))?;

    Ok(join(recv, send))
}

/// Похоже ли это на тикет, а не на адрес сервера.
///
/// Тикет — открытый ключ в base32: только буквы и цифры, ни точек, ни
/// двоеточий, ни слэшей. Так его ни с `192.168.1.5:8080`, ни с `ws://…` не
/// перепутать.
pub fn looks_like_ticket(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 40
        && value.len() <= 80
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() && !ch.is_ascii_uppercase())
}

/// Разбирает тикет, объясняя по-человечески, что не так.
pub fn parse_ticket(ticket: &str) -> Result<EndpointId, String> {
    ticket
        .trim()
        .parse::<EndpointId>()
        .map_err(|_| "это не похоже на тикет: нужен ключ, который печатает /host".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_is_told_apart_from_an_address() {
        // Настоящий тикет — 52 знака base32.
        let ticket = "a".repeat(52);
        assert!(looks_like_ticket(&ticket));

        // Всё, чем люди обычно пользуются как адресом, тикетом быть не должно.
        for address in [
            "192.168.1.5:8080",
            "ws://192.168.1.5:8080/ws",
            "http://localhost:8080",
            "localhost",
            "",
            "короткое",
        ] {
            assert!(!looks_like_ticket(address), "принял за тикет: {address}");
        }
    }

    #[test]
    fn nonsense_tickets_are_refused_with_a_readable_reason() {
        let err = parse_ticket("не тикет").unwrap_err();

        assert!(err.contains("тикет"), "{err}");
    }

    #[tokio::test]
    async fn a_ticket_round_trips_through_the_parser() {
        // Тикет, выданный настоящим endpoint, должен читаться обратно: иначе
        // человек скопировал бы то, что не принимается на другой стороне.
        let endpoint = Endpoint::builder(presets::N0DisableRelay)
            .bind()
            .await
            .expect("endpoint не поднялся");
        let ticket = endpoint.id().to_string();

        assert!(looks_like_ticket(&ticket), "{ticket}");
        assert_eq!(parse_ticket(&ticket).unwrap(), endpoint.id());
    }
}
