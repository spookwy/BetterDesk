//! Настройка QUIC: TLS, таймауты, параметры под низкую задержку.
//!
//! # Про сертификаты на этом этапе
//!
//! QUIC не бывает без TLS — это часть протокола, а не опция. Но
//! полноценная модель доверия (Ed25519-идентичность устройства,
//! пиннинг публичного ключа, предупреждение при смене) — это этап 5
//! (CLAUDE.md §7.3, §8.1).
//!
//! Пока хост выдаёт **самоподписанный сертификат**, а клиент
//! принимает любой. Это честно записанный временный компромисс, а не
//! недосмотр: шифрование канала работает, аутентификации стороны
//! нет. MITM на этом этапе возможен — и именно поэтому этап 5
//! существует отдельным пунктом плана.
//!
//! Чтобы это не забылось, проверяющий сертификаты объект называется
//! [`AcceptAnyServer`] и несёт предупреждение в документации.

use crate::{Result, TransportError};
use std::sync::Arc;
use std::time::Duration;

/// Название протокола в ALPN.
///
/// ALPN обязателен в QUIC. Своё имя, а не `h3`: мы не HTTP/3, и
/// притворяться им значило бы получать чужие пакеты от промежуточных
/// узлов, которые попытаются их разобрать.
pub const ALPN: &[u8] = b"betterdesk/1";

/// Через сколько молчания считать соединение потерянным.
///
/// Меньше значения по умолчанию (30 с у quinn): пользователь должен
/// узнать о разрыве быстрее, чем успеет решить, что программа
/// зависла. Но и не слишком мало — критерий этапа 3 требует пережить
/// **двухсекундный** разрыв сети, а значит порог обязан быть заметно
/// больше двух секунд.
const IDLE_TIMEOUT: Duration = Duration::from_secs(8);

/// Как часто слать keep-alive.
///
/// Нужен, чтобы NAT не закрыл отображение при паузе в трафике: у
/// домашних роутеров окно бывает и 30 секунд. Пять — с запасом.
const KEEP_ALIVE: Duration = Duration::from_secs(5);

/// Настройки транспорта QUIC, общие для клиента и сервера.
///
/// Вынесены отдельно, потому что расходиться они не должны: разные
/// таймауты у сторон дают разрыв, который выглядит как сетевая
/// проблема, а на деле — рассогласование настроек.
fn transport_config() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();

    config.max_idle_timeout(Some(
        IDLE_TIMEOUT
            .try_into()
            .expect("IDLE_TIMEOUT — константа в допустимом диапазоне"),
    ));
    config.keep_alive_interval(Some(KEEP_ALIVE));

    // Датаграмы — основной путь для видео и ввода (§5.3). Размер
    // буфера отправки ограничен: если сеть не успевает, датаграмы
    // надо **терять**, а не копить. Копящаяся очередь превращается
    // в задержку, а устаревший кадр не нужен никому.
    //
    // 256 КБ — это около восьми кадров 1080p при 15 Мбит/с. Больше
    // держать бессмысленно: восемь кадров задержки уже неприемлемы.
    config.datagram_send_buffer_size(256 * 1024);

    // Потоки нужны только для управления и файлов (§5.3), поэтому
    // их немного. Ограничение — часть защиты: сторона, открывающая
    // потоки без предела, иначе исчерпала бы память.
    config.max_concurrent_bidi_streams(4u32.into());
    config.max_concurrent_uni_streams(4u32.into());

    config
}

/// Конфигурация сервера (хоста) и его самоподписанный сертификат.
///
/// Возвращает конфигурацию и DER-кодированный сертификат: клиенту
/// его надо будет предъявить для пиннинга (этап 5), а пока он нужен
/// хотя бы для диагностики.
pub fn server_config() -> Result<(quinn::ServerConfig, Vec<u8>)> {
    // Имя в сертификате произвольное: проверять его сейчас некому,
    // а на этапе 5 доверие будет строиться на пиннинге ключа, а не
    // на имени.
    let cert = rcgen::generate_simple_self_signed(vec!["betterdesk".to_string()])
        .map_err(|e| TransportError::Setup(format!("генерация сертификата: {e}")))?;

    let cert_der = cert.cert.der().to_vec();
    let key_der = cert.key_pair.serialize_der();

    // TLS собирается вручную, а не через `ServerConfig::with_single_cert`,
    // ровно ради одной строки — `alpn_protocols`.
    //
    // Тот удобный конструктор ALPN не выставляет, и сервер молча
    // соглашается на пустой список. Клиент при этом требует
    // `betterdesk/1`, стороны не находят общего протокола, и
    // рукопожатие обрывается с «peer doesn't support any known
    // protocol» — ошибкой, которая ничего не говорит о причине.
    //
    // Юнит-тесты этого не ловят: обе конфигурации собираются без
    // единой жалобы. Нашла проба `quic_probe` на первом же реальном
    // соединении.
    let mut crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(cert_der.clone())],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.into()),
        )
        .map_err(|e| TransportError::Setup(format!("серверный TLS: {e}")))?;

    crypto.alpn_protocols = vec![ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
        .map_err(|e| TransportError::Setup(format!("серверный QUIC-TLS: {e}")))?;

    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
    config.transport_config(Arc::new(transport_config()));

    Ok((config, cert_der))
}

/// Конфигурация клиента.
///
/// **Принимает любой сертификат.** См. примечание в начале модуля:
/// аутентификация стороны — этап 5, здесь только шифрование канала.
pub fn client_config() -> Result<quinn::ClientConfig> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer::new()))
        .with_no_client_auth();

    let mut crypto = crypto;
    crypto.alpn_protocols = vec![ALPN.to_vec()];

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|e| TransportError::Setup(format!("клиентский TLS: {e}")))?;

    let mut config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    config.transport_config(Arc::new(transport_config()));

    Ok(config)
}

/// Проверяющий, принимающий **любой** сертификат сервера.
///
/// # Это временно и небезопасно
///
/// Такой проверяющий делает MITM возможным: подменивший сервер
/// предъявит свой сертификат, и клиент его примет. Здесь это
/// осознанный компромисс этапа 3, где задача — измерить задержку на
/// реальной сети, а не защититься от атак.
///
/// **Заменяется на этапе 5** (CLAUDE.md §7.3, §8.1): пиннинг
/// публичного ключа хоста, громкое предупреждение при его смене,
/// E2E-слой поверх QUIC.
#[derive(Debug)]
struct AcceptAnyServer {
    /// Набор поддерживаемых алгоритмов подписи.
    ///
    /// Берётся у провайдера, а не перечисляется вручную: список
    /// зависит от собранных features `rustls`, и расхождение дало бы
    /// отказ рукопожатия с невнятной диагностикой.
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl AcceptAnyServer {
    fn new() -> Self {
        Self {
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServer {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_builds_with_certificate() {
        let (_, cert) = server_config().expect("конфигурация сервера");
        assert!(!cert.is_empty(), "сертификат пуст");
    }

    #[test]
    fn client_config_builds() {
        // Проверяет заодно, что провайдер криптографии установлен:
        // без него `rustls` паникует не здесь, а при рукопожатии,
        // где причину искать много труднее.
        client_config().expect("конфигурация клиента");
    }

    #[test]
    fn idle_timeout_exceeds_required_outage() {
        // Критерий этапа 3: сессия переживает двухсекундный разрыв
        // сети. Таймаут, равный или меньший, сделал бы критерий
        // невыполнимым по построению.
        assert!(
            IDLE_TIMEOUT > Duration::from_secs(2),
            "таймаут простоя не даёт пережить разрыв из критерия этапа"
        );
    }

    #[test]
    fn keep_alive_is_shorter_than_idle_timeout() {
        // Keep-alive реже таймаута означает разрыв на ровном месте:
        // сторона молчит дольше, чем другая готова ждать.
        assert!(KEEP_ALIVE < IDLE_TIMEOUT);
    }

    #[test]
    fn both_sides_advertise_the_same_alpn() {
        // Дефект, найденный `quic_probe` на первом же соединении:
        // `ServerConfig::with_single_cert` не выставляет ALPN, клиент
        // его требует, и рукопожатие обрывается с «peer doesn't
        // support any known protocol».
        //
        // Обе конфигурации при этом собирались без жалоб — поэтому
        // тесты на «строится ли» дефект пропустили. Здесь проверяется
        // именно **согласованность** сторон, а не их валидность
        // по отдельности.
        //
        // Собрать TLS-конфигурации заново и сравнить списки напрямую
        // нельзя: `quinn` их прячет. Поэтому проверяется инвариант,
        // из которого дефект следовал: обе стороны обязаны строиться
        // из одной константы, и она не должна быть пустой.
        assert!(!ALPN.is_empty(), "пустой ALPN не согласуется ни с чем");

        // Косвенная, но существенная проверка: обе конфигурации
        // строятся успешно. Если сервер вернётся к конструктору без
        // ALPN, здесь ничего не сломается — вот почему рядом обязана
        // существовать `quic_probe`, гоняющая настоящее рукопожатие.
        server_config().expect("сервер");
        client_config().expect("клиент");
    }

    #[test]
    fn alpn_is_not_http3() {
        // Притворяться h3 нельзя: промежуточные узлы попытаются
        // разобрать наши пакеты как HTTP/3.
        assert_ne!(ALPN, b"h3");
        assert!(!ALPN.is_empty(), "ALPN обязателен в QUIC");
    }
}
