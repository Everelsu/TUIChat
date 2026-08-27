//! Сервер, поднятый прямо в клиенте.
//!
//! Отдельный процесс ради разговора вдвоём — лишняя церемония: клиент и так
//! содержит в себе всё, что нужно. Комната живёт, пока открыт терминал.
//!
//! Для постоянной комнаты сервер по-прежнему запускается сам по себе: там
//! важно, чтобы переписка переживала закрытие чьего-то окна.

use std::io;

/// Поднятый в этом же процессе сервер.
pub struct Hosted {
    /// Куда подключается собственный клиент.
    pub url: String,
    pub port: u16,
    /// Тикет для друга из другой сети. `None` — туннель поднять не вышло:
    /// в локальной сети чат всё равно работает, поэтому это не отказ.
    pub ticket: Option<String>,
    /// Туннель нужно держать живым: с его смертью закрывается труба.
    _tunnel: Option<crate::tunnel::Tunnel>,
}

/// Поднимает сервер на указанном порту.
///
/// Порт `0` означает «любой свободный» — им пользуются тесты.
pub async fn start(port: u16) -> io::Result<Hosted> {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, server::app()).await;
    });

    // Туннель поднимаем сразу: без него друг из другого дома не подключится
    // никак, а узнать об этом в момент, когда он уже ждёт приглашения, —
    // худший из возможных вариантов. Не вышло — молчим и работаем по сети:
    // отказываться поднимать комнату из-за этого не за что.
    let tunnel = crate::tunnel::serve(port).await.ok();

    Ok(Hosted {
        url: format!("ws://127.0.0.1:{port}/ws"),
        port,
        ticket: tunnel.as_ref().map(|tunnel| tunnel.ticket.clone()),
        _tunnel: tunnel,
    })
}

impl Hosted {
    /// Строки-приглашения с адресами, которые можно переслать собеседнику.
    ///
    /// Показываем их прямо в переписке: иначе первое, что спросит второй
    /// человек, — «а куда подключаться».
    pub fn invitations(&self) -> Vec<String> {
        let mut lines = vec![format!("сервер поднят здесь же, порт {}", self.port)];

        // Тикет первым: это единственная строка, которая работает откуда
        // угодно, а адрес в локальной сети — только для тех, кто рядом.
        match &self.ticket {
            Some(ticket) => {
                lines.push("друг из любой сети — пусть вставит это в «сервер»:".to_string());
                lines.push(ticket.clone());
            }
            None => lines.push(
                "туннель не поднялся: снаружи не подключиться, только по сети ниже".to_string(),
            ),
        }

        for ip in server::tls::local_ips() {
            lines.push(format!(
                "кто рядом: --server ws://{ip}:{}/ws · с телефона http://{ip}:{}",
                self.port, self.port
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{ClientMessage, ServerMessage};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    #[tokio::test]
    async fn hosted_server_accepts_a_client() {
        let hosted = start(0).await.unwrap();

        let (mut socket, _) = tokio_tungstenite::connect_async(&hosted.url).await.unwrap();
        let join = ClientMessage::Join {
            nickname: "alice".into(),
            room: "general".into(),
        };
        socket
            .send(WsMessage::text(serde_json::to_string(&join).unwrap()))
            .await
            .unwrap();

        let WsMessage::Text(answer) = socket.next().await.unwrap().unwrap() else {
            panic!("сервер ответил не текстом");
        };
        let answer: ServerMessage = serde_json::from_str(answer.as_str()).unwrap();
        assert!(matches!(answer, ServerMessage::Welcome { .. }));
    }

    #[tokio::test]
    async fn invitation_names_the_port_and_the_addresses() {
        let hosted = start(0).await.unwrap();

        let lines = hosted.invitations();

        assert!(lines[0].contains(&hosted.port.to_string()), "{lines:?}");
        // Либо адреса в сети, либо честное «сети не видно» — пустого списка
        // быть не должно, иначе непонятно, что делать дальше.
        assert!(lines.len() >= 2, "{lines:?}");
    }

    #[tokio::test]
    async fn busy_port_is_reported() {
        // Занимаем именно 0.0.0.0: Windows разрешает встать на него поверх
        // уже занятого 127.0.0.1, так что столкновение возникает только когда
        // оба слушают одинаково широко.
        let taken = tokio::net::TcpListener::bind(("0.0.0.0", 0)).await.unwrap();
        let port = taken.local_addr().unwrap().port();

        let result = start(port).await;

        // Занятый порт — обычное дело: сообщение об этом должно доходить до
        // человека, а не теряться внутри.
        assert!(result.is_err());
    }
}
