//! Самоподписанный сертификат и определение адреса в локальной сети.
//!
//! HTTPS нужен не ради «безопасности в домашней сети», а потому что браузеры
//! дают микрофон, камеру и уведомления только в защищённом контексте: по
//! `http://192.168.x.x` они не работают вообще. Заодно исчезает предупреждение
//! в адресной строке.

use std::{
    fs, io,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

/// Файлы сертификата и ключа. Хранятся на диске, а не выпускаются заново при
/// каждом запуске: иначе телефон будет ругаться на «новый» сертификат после
/// каждой перезагрузки сервера.
pub struct Certificate {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Готовит сертификат в каталоге `dir`, создавая его при первом запуске.
///
/// В сертификат попадают `localhost` и все локальные адреса машины. Без нужного
/// адреса телефон получит ошибку не про «неизвестный центр сертификации»,
/// которую можно пропустить одним нажатием, а про «сертификат не для этого
/// адреса», которую пропустить уже нельзя.
pub fn ensure_certificate(dir: &Path) -> io::Result<Certificate> {
    let paths = Certificate {
        cert: dir.join("cert.pem"),
        key: dir.join("key.pem"),
    };
    if paths.cert.exists() && paths.key.exists() {
        return Ok(paths);
    }

    fs::create_dir_all(dir)?;

    let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    names.extend(local_ips().iter().map(IpAddr::to_string));

    let generated = rcgen::generate_simple_self_signed(names)
        .map_err(|err| io::Error::other(format!("не удалось выпустить сертификат: {err}")))?;
    fs::write(&paths.cert, generated.cert.pem())?;
    fs::write(&paths.key, generated.signing_key.serialize_pem())?;

    Ok(paths)
}

/// Адреса машины в локальной сети, самый вероятный — первым.
///
/// Перебираем все интерфейсы, а не спрашиваем «адрес маршрута по умолчанию»:
/// на машине с Docker или WSL маршрут по умолчанию ведёт через виртуальный
/// адаптер, и в подсказке оказался бы адрес, по которому телефон не достучится.
pub fn local_ips() -> Vec<IpAddr> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };

    let mut found: Vec<Ipv4Addr> = interfaces
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.ip() {
            IpAddr::V4(ip) if ip.is_private() => Some(ip),
            _ => None,
        })
        .collect();

    found.sort_by_key(|ip| (rank(*ip), ip.octets()));
    found.dedup();
    found.into_iter().map(IpAddr::V4).collect()
}

/// Домашние сети почти всегда 192.168.x.x, поэтому такой адрес показываем
/// первым. 172.16–31.x чаще всего оказывается мостом Docker или WSL.
fn rank(ip: Ipv4Addr) -> u8 {
    match ip.octets() {
        [192, 168, ..] => 0,
        [10, ..] => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_is_created_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("chat-tls-{}", uuid::Uuid::new_v4()));

        let first = ensure_certificate(&dir).unwrap();
        let pem = fs::read_to_string(&first.cert).unwrap();
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(
            fs::read_to_string(&first.key)
                .unwrap()
                .contains("PRIVATE KEY")
        );

        // Повторный запуск обязан взять тот же файл: иначе телефон каждый раз
        // видит новый сертификат и требует подтверждения заново.
        let second = ensure_certificate(&dir).unwrap();
        assert_eq!(fs::read_to_string(&second.cert).unwrap(), pem);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn home_network_address_comes_first() {
        let mut addresses = [
            Ipv4Addr::new(172, 18, 0, 1),
            Ipv4Addr::new(10, 0, 0, 5),
            Ipv4Addr::new(192, 168, 1, 40),
        ];

        addresses.sort_by_key(|ip| (rank(*ip), ip.octets()));

        // Адрес мостов Docker и WSL не должен попадать в подсказку первым.
        assert_eq!(addresses[0], Ipv4Addr::new(192, 168, 1, 40));
    }

    #[test]
    fn only_private_addresses_are_offered() {
        for ip in local_ips() {
            let IpAddr::V4(ip) = ip else {
                panic!("ожидались только адреса IPv4");
            };
            assert!(ip.is_private(), "публичный адрес в подсказке: {ip}");
        }
    }
}
