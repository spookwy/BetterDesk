//! Хранилище приватного ключа устройства.
//!
//! # Почему здесь trait, а не сразу DPAPI
//!
//! §7.3 требует хранить приватный ключ в DPAPI (`CryptProtectData`) —
//! он привязывает шифрование к машине и пользователю, так что
//! скопированный файл ключа бесполезен на другом компьютере.
//!
//! Но DPAPI — это Win32, то есть `unsafe` и зависимость от `windows`.
//! А `bd-crypto` живёт под `#![forbid(unsafe_code)]` и обязан
//! собираться под Linux (§4.2.1): сервер сигналинга и будущий релей
//! проверяют подписи теми же типами.
//!
//! Отсюда разделение, ровно как у `Capturer` и `Encoder` (§4.2.2):
//! здесь — [`KeyStore`] и его переносимая файловая реализация,
//! обёртка DPAPI — в платформенном крейте.
//!
//! # Файловое хранилище — это НЕ «пока сойдёт»
//!
//! [`FileKeyStore`] нужен всерьёз и надолго:
//!
//! - на Linux (сервер) DPAPI не существует вовсе;
//! - в переносимом режиме (§7.1) программа запускается с флешки, и
//!   ключ, привязанный к машине, там неуместен: человек ждёт, что
//!   его личность поедет вместе с файлом.
//!
//! Разница между ними записана в [`KeyStore::protects_at_rest`], и
//! вызывающий обязан её показать человеку, а не умолчать.
//!
//! # Чего не делает никакое из них
//!
//! Не защищает от процесса, работающего под тем же пользователем.
//! DPAPI расшифрует ключ для любого такого процесса — это его
//! устройство, а не наш недосмотр: против локального вредоноса
//! (противник 6 в docs/security-model.md) помог бы только TPM или
//! ввод пароля при каждом запуске, и оба ломают «скачал и запустил».

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::identity::{DeviceIdentity, IdentityError, SECRET_LEN};

/// Ошибки хранилища ключа.
#[derive(Debug, thiserror::Error)]
pub enum KeyStoreError {
    /// Не удалось прочитать или записать.
    #[error("хранилище ключа недоступно ({path}): {source}")]
    Io {
        /// Путь, на котором споткнулись, — без него сообщение
        /// бесполезно тому, кто чинит.
        path: PathBuf,
        /// Настоящая причина от ОС.
        #[source]
        source: io::Error,
    },

    /// Ключ прочитан, но негоден.
    #[error("ключ в хранилище негоден: {0}")]
    Invalid(#[from] IdentityError),
}

/// Куда класть приватный ключ устройства.
///
/// Реализация обязана быть **атомарной на запись**: прерванное
/// сохранение не должно оставить полуключ, который при следующем
/// запуске прочитается как «повреждён» и заставит перевыпустить
/// личность. Смена ключа — событие, о котором собеседник получит
/// громкое предупреждение (см. `pinning`), и устраивать его из-за
/// сбоя питания недопустимо.
pub trait KeyStore {
    /// Загрузить ключ, если он уже есть.
    ///
    /// `Ok(None)` — ключа нет, это штатный первый запуск, а не ошибка.
    fn load(&self) -> Result<Option<DeviceIdentity>, KeyStoreError>;

    /// Сохранить ключ.
    fn save(&self, identity: &DeviceIdentity) -> Result<(), KeyStoreError>;

    /// Защищены ли байты ключа средствами ОС.
    ///
    /// `false` означает: файл, скопированный на другую машину, даст
    /// личность этого устройства. Это не обязательно плохо
    /// (переносимый режим на том и стоит), но человек должен знать —
    /// молчаливая разница в защите хуже честной.
    fn protects_at_rest(&self) -> bool;

    /// Загрузить существующий ключ или выпустить новый и сохранить.
    ///
    /// Метод по умолчанию: порядок одинаков для всех реализаций, и
    /// разъехаться копиям здесь незачем (находка 62).
    ///
    /// **Повреждённый ключ не перевыпускается молча.** Соблазн велик:
    /// «не прочитался — сделаем новый, всё заработает». Но у
    /// собеседника наш прежний ключ записан как доверенный, и тихая
    /// подмена выглядит для него ровно как атака — с той разницей,
    /// что настоящую атаку человек после пары ложных тревог
    /// пролистает не глядя.
    fn load_or_create(&self) -> Result<DeviceIdentity, KeyStoreError> {
        if let Some(identity) = self.load()? {
            return Ok(identity);
        }

        let identity = DeviceIdentity::generate()?;
        self.save(&identity)?;
        Ok(identity)
    }
}

/// Ключ в обычном файле, без шифрования средствами ОС.
///
/// Годится для Linux и переносимого режима; на Windows в
/// установленном режиме поверх него кладётся DPAPI.
pub struct FileKeyStore {
    path: PathBuf,
}

impl FileKeyStore {
    /// Хранилище по указанному пути.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Куда сохраняет.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn io_error(&self, source: io::Error) -> KeyStoreError {
        KeyStoreError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

impl KeyStore for FileKeyStore {
    fn load(&self) -> Result<Option<DeviceIdentity>, KeyStoreError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            // Файла нет — первый запуск. Не ошибка.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.io_error(e)),
        };

        DeviceIdentity::from_secret_bytes(&bytes)
            .map(Some)
            .map_err(KeyStoreError::Invalid)
    }

    fn save(&self, identity: &DeviceIdentity) -> Result<(), KeyStoreError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| KeyStoreError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
        }

        // Запись через временный файл и переименование.
        //
        // Прямая запись в целевой файл при сбое питания оставила бы
        // усечённый ключ — то есть заставила бы перевыпустить
        // личность и напугать всех, кто нас запомнил. Переименование
        // же атомарно: файл либо прежний, либо новый целиком.
        //
        // Тот же приём, что при распаковке бинарей в оболочке
        // (находка 69), и по той же причине.
        let temporary = self.path.with_extension("part");
        fs::write(&temporary, identity.to_secret_bytes()).map_err(|e| KeyStoreError::Io {
            path: temporary.clone(),
            source: e,
        })?;

        fs::rename(&temporary, &self.path).map_err(|e| self.io_error(e))
    }

    fn protects_at_rest(&self) -> bool {
        false
    }
}

/// Проверка, что байты похожи на ключ, до попытки его разобрать.
///
/// Нужна реализациям, которые расшифровывают ключ сами (DPAPI):
/// понятная причина лучше, чем «ключ повреждён» после расшифровки
/// чужих байтов.
#[must_use]
pub fn looks_like_secret(bytes: &[u8]) -> bool {
    bytes.len() == SECRET_LEN
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Каталог для теста в системном temp, свой на каждый тест.
    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("bd-keystore-test-{name}-{}", std::process::id()));
        path.push("device.key");
        path
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn missing_key_is_not_an_error() {
        let path = temp_path("missing");
        cleanup(&path);
        let store = FileKeyStore::new(&path);

        assert!(store.load().expect("чтение отсутствующего ключа").is_none());
    }

    #[test]
    fn key_survives_save_and_load() {
        let path = temp_path("roundtrip");
        cleanup(&path);
        let store = FileKeyStore::new(&path);

        let saved = DeviceIdentity::generate().expect("генерация");
        store.save(&saved).expect("сохранение");

        let loaded = store
            .load()
            .expect("чтение")
            .expect("ключ должен быть на месте");

        assert_eq!(saved.public(), loaded.public());
        cleanup(&path);
    }

    /// Главное свойство: **ID машины не меняется между запусками**.
    /// Если бы `load_or_create` каждый раз выпускал новый ключ, все,
    /// кто нас запомнил, получали бы предупреждение о подмене.
    #[test]
    fn load_or_create_is_stable_across_runs() {
        let path = temp_path("stable");
        cleanup(&path);
        let store = FileKeyStore::new(&path);

        let first = store.load_or_create().expect("первый запуск");
        let second = store.load_or_create().expect("второй запуск");

        assert_eq!(first.public(), second.public());
        cleanup(&path);
    }

    /// Проверка на способность провалиться (находка 4): два **разных**
    /// хранилища обязаны дать разные ключи. Без этого предыдущий тест
    /// прошёл бы и при реализации, возвращающей константу.
    #[test]
    fn different_stores_give_different_keys() {
        let one = temp_path("distinct-one");
        let two = temp_path("distinct-two");
        cleanup(&one);
        cleanup(&two);

        let first = FileKeyStore::new(&one)
            .load_or_create()
            .expect("первое хранилище");
        let second = FileKeyStore::new(&two)
            .load_or_create()
            .expect("второе хранилище");

        assert_ne!(first.public(), second.public());
        cleanup(&one);
        cleanup(&two);
    }

    /// Повреждённый ключ — ошибка, а **не** повод молча выпустить
    /// новый: тихая смена личности неотличима от подмены хоста.
    #[test]
    fn corrupt_key_is_reported_not_replaced() {
        let path = temp_path("corrupt");
        cleanup(&path);
        fs::create_dir_all(path.parent().expect("родитель")).expect("каталог");
        fs::write(&path, b"ne klyuch").expect("запись мусора");

        let store = FileKeyStore::new(&path);
        assert!(store.load().is_err());
        assert!(store.load_or_create().is_err());

        cleanup(&path);
    }

    /// После сохранения не должно оставаться временного файла:
    /// иначе на диске лежала бы вторая копия приватного ключа,
    /// о которой никто не знает.
    #[test]
    fn no_temporary_file_is_left_behind() {
        let path = temp_path("no-leftovers");
        cleanup(&path);
        let store = FileKeyStore::new(&path);

        store
            .save(&DeviceIdentity::generate().expect("генерация"))
            .expect("сохранение");

        assert!(!path.with_extension("part").exists());
        cleanup(&path);
    }

    /// Файловое хранилище обязано **честно** сообщать, что не
    /// защищает байты: на этом основано предупреждение человеку.
    #[test]
    fn file_store_admits_it_does_not_protect() {
        assert!(!FileKeyStore::new("device.key").protects_at_rest());
    }

    #[test]
    fn secret_length_is_checked() {
        assert!(looks_like_secret(&[0u8; SECRET_LEN]));
        assert!(!looks_like_secret(&[0u8; SECRET_LEN - 1]));
        assert!(!looks_like_secret(&[]));
    }
}
