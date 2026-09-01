//! Криптографическая идентичность устройства: пара ключей Ed25519.
//!
//! # Что это закрывает
//!
//! Записанную дыру: сигналинг недоверен (§8.1), и до сих пор клиент
//! не мог проверить, **с той ли машиной** он соединился. Подставной
//! хост принимал любой пароль и показывал свой экран — пароль
//! доказывает знание секрета, но ничего не говорит о том, кому его
//! сообщили.
//!
//! Ed25519-ключ решает ровно это: он не выдаётся сервером, не
//! диктуется голосом и не может быть подделан тем, кто его не знает.
//!
//! # Ключ — это личность, ID — это имя
//!
//! Разделение важно и записано в трёх местах проекта (§7.3,
//! `device.rs`, `auth.rs`):
//!
//! | | ID (девять цифр) | Ключ Ed25519 |
//! |---|---|---|
//! | назначение | назвать машину человеку | доказать, что это она |
//! | секретность | не секрет, диктуется вслух | приватная часть не покидает машину |
//! | подделка | тривиальна | требует приватного ключа |
//!
//! Отсюда правило: **ID нельзя проверять, ключ нельзя показывать**.
//! Смешение этих ролей и есть та дыра, ради которой писан модуль.
//!
//! # Чего этот модуль НЕ делает
//!
//! Он не решает, **кому доверять**. Пара ключей — только удостоверение
//! личности; сопоставление «этот ключ принадлежит машине, к которой я
//! подключался вчера» живёт в [`crate::pinning`]. Разделение
//! намеренное: удостоверение выдаётся один раз при первом запуске,
//! а доверие набирается со временем и может быть отозвано.

use core::fmt;

use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey, SECRET_KEY_LENGTH};
use rand::rngs::SysRng;
use rand::TryRng as _;

/// Длина приватного ключа в байтах.
pub const SECRET_LEN: usize = SECRET_KEY_LENGTH;

/// Длина публичного ключа в байтах.
pub const PUBLIC_LEN: usize = 32;

/// Длина подписи в байтах.
pub const SIGNATURE_LEN: usize = 64;

/// Ошибки работы с идентичностью.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// Ключ повреждён или имеет неверную длину.
    ///
    /// Приватный ключ читается из хранилища, которое может быть
    /// испорчено: обрезанный файл, чужие байты, сбой диска.
    #[error("ключ устройства повреждён: {0}")]
    Corrupt(&'static str),

    /// Источник случайности недоступен.
    ///
    /// Отдельно от [`Self::Corrupt`], потому что реакция другая:
    /// повреждённый ключ надо перевыпустить, а без случайности
    /// генерировать нельзя вообще — молча взять предсказуемый посев
    /// значило бы выдать ключ, который подберут.
    #[error("источник случайности недоступен: {0}")]
    NoRandomness(String),
}

/// Публичный ключ устройства — то, чем машина доказывает свою личность.
///
/// Копируемый и сравнимый: его передают, хранят в списке доверенных
/// и сличают на каждом подключении.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DevicePublicKey(VerifyingKey);

impl DevicePublicKey {
    /// Собрать из сырых байт, пришедших от пира.
    ///
    /// **Недоверенные данные** (§4.3.5): приходят по сети до всякой
    /// проверки, и отвергаются здесь по двум разным причинам.
    ///
    /// Первая — байты не распаковываются в точку кривой. Это просто
    /// мусор или обрыв.
    ///
    /// Вторая интереснее: точка распаковалась, но имеет **малый
    /// порядок**. Для такого ключа существует подпись, верная почти
    /// под любым сообщением, — то есть предъявивший его проходит
    /// проверку, не зная никакого секрета. Библиотека такие ключи
    /// принимает (совместимость со старыми протоколами), и отвергнуть
    /// их — наша забота.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IdentityError> {
        let array: [u8; PUBLIC_LEN] = bytes
            .try_into()
            .map_err(|_| IdentityError::Corrupt("длина публичного ключа не 32 байта"))?;

        let key = VerifyingKey::from_bytes(&array)
            .map(Self)
            .map_err(|_| IdentityError::Corrupt("точка не принадлежит кривой Ed25519"))?;

        if key.is_weak() {
            return Err(IdentityError::Corrupt(
                "ключ малого порядка: под ним подделывается подпись",
            ));
        }

        Ok(key)
    }

    /// Сырые байты для передачи или хранения.
    pub fn to_bytes(&self) -> [u8; PUBLIC_LEN] {
        self.0.to_bytes()
    }

    /// Проверить подпись под сообщением.
    ///
    /// Возвращает `false` на любую неудачу — и на неверную подпись, и
    /// на её испорченную длину. Разделять эти случаи для вызывающего
    /// незачем: реакция одна, а разные сообщения об ошибке подсказали
    /// бы атакующему, насколько он близок.
    ///
    /// # Почему `verify_strict`, а не `verify`
    ///
    /// Обычный `verify` **принимает слабые ключи малого порядка**, для
    /// которых существует подпись, верная почти под любым сообщением.
    /// Для проверки подписи под документом это исторический
    /// компромисс совместимости; для **опознания устройства** —
    /// пробоина: подставной хост предъявил бы такой ключ и прошёл
    /// проверку, ничего не зная.
    ///
    /// `ed25519-dalek` прямо называет этот случай (уникальные
    /// идентичности — Signal, onion-сервисы Tor) и держит строгую
    /// проверку отдельным методом только ради старых протоколов. Наш
    /// протокол новый, совместимость хранить не с чем — берём строгую.
    ///
    /// Это ровно тот класс, что находка 12: **декларация в
    /// документации не заменяет проверки**, каким именно методом
    /// пользуется код.
    #[must_use]
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        let Ok(array) = <[u8; SIGNATURE_LEN]>::try_from(signature) else {
            return false;
        };
        self.0
            .verify_strict(message, &Signature::from_bytes(&array))
            .is_ok()
    }

    /// Слабый ли это ключ (малого порядка).
    ///
    /// Публичен намеренно: [`Self::verify`] такие ключи уже
    /// отвергает, но отказ на этапе разбора ключа даёт **понятную
    /// причину** вместо «подпись не сошлась» — а по второму сообщению
    /// человек за машиной не может сделать ничего (находки 61, 63).
    #[must_use]
    pub fn is_weak(&self) -> bool {
        self.0.is_weak()
    }

    /// Отпечаток для показа человеку — восемь групп по четыре
    /// шестнадцатеричные цифры.
    ///
    /// # Почему не весь ключ и почему не короче
    ///
    /// Целиком 32 байта человек не сверит — он сдастся на середине и
    /// начнёт нажимать «да». Короткий отпечаток (четыре байта, как в
    /// старых SSH) подбирается: атакующему нужно сгенерировать пары
    /// ключей, пока отпечаток не совпадёт, а это часы работы.
    ///
    /// Шестнадцать байт — 128 бит — не подбираются, и при этом строка
    /// умещается в четыре строки на экране группами по четыре знака.
    /// Группы обязательны: сплошную строку глаз сверяет посимвольно и
    /// ошибается, разбитую — блоками.
    pub fn fingerprint(&self) -> String {
        let bytes = self.to_bytes();
        let mut out = String::with_capacity(8 * 5);
        for (index, chunk) in bytes[..16].chunks(2).enumerate() {
            if index > 0 {
                out.push(' ');
            }
            out.push_str(&format!("{:02X}{:02X}", chunk[0], chunk[1]));
        }
        out
    }
}

impl fmt::Debug for DevicePublicKey {
    /// Печатает отпечаток, а не ключ.
    ///
    /// Ключ публичный, скрывать его не от кого — но в логе он
    /// бесполезен: 64 знака, которые никто не читает. Отпечаток и
    /// узнаваем, и совпадает с тем, что видит человек на экране.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DevicePublicKey({})", self.fingerprint())
    }
}

/// Полная пара ключей устройства.
///
/// # Приватная часть не выходит наружу
///
/// Ни `Debug`, ни `Clone`, ни геттера приватного ключа здесь нет —
/// намеренно (§4.3.3). Наружу выходят только подпись и публичный
/// ключ. Единственное исключение — [`Self::to_secret_bytes`] для
/// записи в хранилище; она названа так, чтобы её нельзя было вызвать
/// случайно.
pub struct DeviceIdentity {
    signing: SigningKey,
}

impl DeviceIdentity {
    /// Выпустить новую пару ключей.
    ///
    /// Случайность берётся у ОС (`SysRng`), а не у обычного генератора:
    /// приватный ключ, выведенный из предсказуемого посева,
    /// восстанавливается кем угодно, и тогда весь модуль бессмыслен.
    ///
    /// Отказ ОС в случайности — ошибка, а не повод взять запасной
    /// источник. Молчаливый откат к слабому генератору здесь был бы
    /// худшим из возможных поведений: всё работает, и ничто не
    /// защищено.
    pub fn generate() -> Result<Self, IdentityError> {
        let mut secret = [0u8; SECRET_LEN];
        SysRng
            .try_fill_bytes(&mut secret)
            .map_err(|e| IdentityError::NoRandomness(e.to_string()))?;

        Ok(Self {
            signing: SigningKey::from_bytes(&secret),
        })
    }

    /// Восстановить из сохранённых байт приватного ключа.
    pub fn from_secret_bytes(bytes: &[u8]) -> Result<Self, IdentityError> {
        let array: [u8; SECRET_LEN] = bytes
            .try_into()
            .map_err(|_| IdentityError::Corrupt("длина приватного ключа не 32 байта"))?;

        Ok(Self {
            signing: SigningKey::from_bytes(&array),
        })
    }

    /// Байты приватного ключа — **только** для записи в защищённое
    /// хранилище.
    ///
    /// Имя длинное и неудобное намеренно: всякий его вызов вне
    /// хранилища — ошибка, и он должен бросаться в глаза при чтении
    /// кода.
    pub fn to_secret_bytes(&self) -> [u8; SECRET_LEN] {
        self.signing.to_bytes()
    }

    /// Публичный ключ — то, что показывают пиру.
    pub fn public(&self) -> DevicePublicKey {
        DevicePublicKey(self.signing.verifying_key())
    }

    /// Подписать сообщение.
    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LEN] {
        self.signing.sign(message).to_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_verifies() {
        let identity = DeviceIdentity::generate().expect("генерация ключа");
        let message = b"kadr";
        let signature = identity.sign(message);

        assert!(identity.public().verify(message, &signature));
    }

    /// Проверка на способность провалиться (находка 4): подпись под
    /// другим сообщением обязана отвергаться, иначе `verify` мог бы
    /// возвращать `true` всегда и все остальные тесты проходили бы.
    #[test]
    fn signature_of_other_message_is_rejected() {
        let identity = DeviceIdentity::generate().expect("генерация ключа");
        let signature = identity.sign(b"kadr");

        assert!(!identity.public().verify(b"drugoy kadr", &signature));
    }

    /// Главное свойство модуля: чужая подпись не проходит. Без этого
    /// подставной хост оставался бы неотличим от настоящего.
    #[test]
    fn signature_of_another_device_is_rejected() {
        let ours = DeviceIdentity::generate().expect("генерация ключа");
        let theirs = DeviceIdentity::generate().expect("генерация ключа");
        let message = b"kadr";

        assert!(!ours.public().verify(message, &theirs.sign(message)));
    }

    #[test]
    fn secret_survives_save_and_load() {
        let original = DeviceIdentity::generate().expect("генерация ключа");
        let restored =
            DeviceIdentity::from_secret_bytes(&original.to_secret_bytes()).expect("загрузка ключа");

        assert_eq!(original.public(), restored.public());
    }

    #[test]
    fn public_key_survives_wire_roundtrip() {
        let identity = DeviceIdentity::generate().expect("генерация ключа");
        let public = identity.public();
        let restored = DevicePublicKey::from_bytes(&public.to_bytes()).expect("разбор ключа");

        assert_eq!(public, restored);
    }

    /// Недоверенный ввод: длина не та.
    #[test]
    fn public_key_of_wrong_length_is_rejected() {
        assert!(DevicePublicKey::from_bytes(&[0u8; 31]).is_err());
        assert!(DevicePublicKey::from_bytes(&[0u8; 33]).is_err());
        assert!(DevicePublicKey::from_bytes(&[]).is_err());
    }

    /// Недоверенный ввод: 32 байта верной длины, но не точка кривой.
    #[test]
    fn public_key_not_on_curve_is_rejected() {
        // Старший бит задаёт знак координаты x; при 0xFF в последнем
        // байте точка не распаковывается.
        let mut bogus = [0u8; PUBLIC_LEN];
        bogus[PUBLIC_LEN - 1] = 0xFF;
        bogus[0] = 0xFF;

        assert!(DevicePublicKey::from_bytes(&bogus).is_err());
    }

    /// **Ключ малого порядка обязан отвергаться.**
    ///
    /// Это не теоретический случай, а готовая пробоина: под таким
    /// ключом существует подпись, верная почти под любым сообщением,
    /// — то есть подставной хост прошёл бы опознание, не зная
    /// никакого секрета.
    ///
    /// Байты ниже — `curve25519_dalek::constants::EIGHT_TORSION[4]`,
    /// точка порядка 2. Вектор взят **из тестов самой библиотеки**, а
    /// не выписан по памяти: первая попытка сочинить такие байты дала
    /// точку, которая просто не распаковывается, — тест падал, но по
    /// другой причине, то есть проверял не то (находка 32).
    ///
    /// `VerifyingKey::from_bytes` этот ключ **принимает**; отвергает
    /// его наш код. Тест проверен на способность провалиться: без
    /// проверки `is_weak` в `from_bytes` он падает.
    #[test]
    fn weak_public_key_is_rejected() {
        let low_order = [
            236, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 127,
        ];

        let error = DevicePublicKey::from_bytes(&low_order)
            .expect_err("ключ малого порядка обязан быть отвергнут");
        assert!(
            error.to_string().contains("малого порядка"),
            "причина должна называть слабый ключ, а не что-то другое: {error}"
        );
    }

    /// Подпись неверной длины отвергается, а не паникует: она
    /// приходит по сети от кого угодно.
    #[test]
    fn signature_of_wrong_length_is_rejected() {
        let identity = DeviceIdentity::generate().expect("генерация ключа");

        assert!(!identity.public().verify(b"kadr", &[0u8; 63]));
        assert!(!identity.public().verify(b"kadr", &[]));
    }

    #[test]
    fn fingerprint_is_grouped_and_stable() {
        let identity = DeviceIdentity::generate().expect("генерация ключа");
        let fingerprint = identity.public().fingerprint();

        // Восемь групп по четыре знака через пробел.
        assert_eq!(fingerprint.split(' ').count(), 8);
        assert!(fingerprint.split(' ').all(|g| g.len() == 4));
        // Один и тот же ключ обязан давать одну и ту же строку —
        // иначе человек сверял бы её с прошлым разом впустую.
        assert_eq!(fingerprint, identity.public().fingerprint());
    }

    /// Разные ключи дают разные отпечатки. Проверка на способность
    /// провалиться: `fingerprint`, возвращающий константу, прошёл бы
    /// все тесты выше.
    #[test]
    fn different_keys_give_different_fingerprints() {
        let one = DeviceIdentity::generate().expect("генерация ключа");
        let two = DeviceIdentity::generate().expect("генерация ключа");

        assert_ne!(one.public().fingerprint(), two.public().fingerprint());
    }

    /// Два вызова `generate` обязаны давать разные ключи. Ловит
    /// вырожденный случай, при котором источник случайности отдаёт
    /// нули: всё работает, ключ у всех один.
    #[test]
    fn generated_keys_are_unique() {
        let one = DeviceIdentity::generate().expect("генерация ключа");
        let two = DeviceIdentity::generate().expect("генерация ключа");

        assert_ne!(one.public(), two.public());
    }
}
