//! Обёртка `CryptProtectData` / `CryptUnprotectData`.
//!
//! Сигнатуры сверены с исходниками `windows` 0.62 (§12, находка 6):
//! обе функции возвращают `Result<()>`, а выходной буфер выделяет
//! **Windows**, и освобождать его обязаны мы — `LocalFree`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bd_crypto::keystore::{KeyStore, KeyStoreError};
use bd_crypto::DeviceIdentity;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
};

use crate::SecretsError;

/// Буфер, выделенный Windows, с освобождением по `Drop`.
///
/// # Почему тип, а не пара вызовов
///
/// `CryptProtectData` кладёт результат в память, выделенную через
/// `LocalAlloc`. Забыть `LocalFree` — утечка на каждый запуск; вызвать
/// дважды — порча кучи. Оба случая тихие.
///
/// §4.3.4 требует, чтобы каждая FFI-обёртка была RAII-типом, и это
/// ровно тот случай: путь с ошибкой посередине (а он здесь есть —
/// расшифровка может не удаться) слишком легко оставляет буфер
/// висеть.
struct LocalBuffer(CRYPT_INTEGER_BLOB);

impl LocalBuffer {
    /// Копия содержимого в обычный `Vec`.
    ///
    /// Копия делается намеренно: наружу не должен выходить указатель
    /// на память, которую освободит `Drop` (§4.3.3). Данные здесь —
    /// 32 байта ключа, цена копии не имеет значения.
    fn to_vec(&self) -> Vec<u8> {
        if self.0.pbData.is_null() || self.0.cbData == 0 {
            return Vec::new();
        }
        // SAFETY: `pbData` получен от CryptProtectData/CryptUnprotectData,
        // которые вернули успех, и потому указывает на `cbData` байт
        // корректной памяти. Буфер жив, пока жив `self`: освобождает
        // его только `Drop`, а `&self` не даёт ему выполниться.
        unsafe { std::slice::from_raw_parts(self.0.pbData, self.0.cbData as usize).to_vec() }
    }
}

impl Drop for LocalBuffer {
    fn drop(&mut self) {
        if self.0.pbData.is_null() {
            return;
        }
        // SAFETY: указатель выделен Windows через LocalAlloc внутри
        // Crypt*Data и ещё не освобождён — `Drop` вызывается ровно
        // один раз, а сам тип не `Copy` и не `Clone`.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.pbData as *mut _)));
        }
        self.0.pbData = std::ptr::null_mut();
    }
}

/// Зашифровать байты для текущего пользователя.
fn protect(plain: &[u8]) -> Result<Vec<u8>, SecretsError> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    // SAFETY: `input` описывает живой срез `plain` (он жив до конца
    // вызова), `output` — валидная запись, которую заполнит Windows.
    // Необязательные параметры переданы как None, что допускается
    // сигнатурой. Флаг 0 = область текущего пользователя.
    unsafe {
        CryptProtectData(
            &input,
            // Описание не задаётся: оно видно в дампе и ничего не
            // защищает, а строку пришлось бы тащить в UTF-16.
            windows::core::PCWSTR::null(),
            None,
            None,
            None,
            0,
            &mut output,
        )
    }
    .map_err(|e| SecretsError::Dpapi(e.message()))?;

    Ok(LocalBuffer(output).to_vec())
}

/// Расшифровать байты, зашифрованные на этой машине этим пользователем.
fn unprotect(sealed: &[u8]) -> Result<Vec<u8>, SecretsError> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: sealed.len() as u32,
        pbData: sealed.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();

    // SAFETY: те же инварианты, что в `protect`.
    let result = unsafe { CryptUnprotectData(&input, None, None, None, None, 0, &mut output) };

    match result {
        Ok(()) => Ok(LocalBuffer(output).to_vec()),
        // Все отказы расшифровки трактуются как «зашифровано не для
        // нас». Разделять их по коду ошибки Windows не стоит: список
        // кодов не документирован полностью, а для человека разницы
        // нет — ключ в любом случае не восстановить.
        //
        // Тот же приём, что в находке 36в с secure desktop: где
        // полного списка кодов нет, надёжнее решать по существу, а не
        // по коду.
        Err(_) => Err(SecretsError::NotOurs),
    }
}

/// Хранилище ключа, защищённое DPAPI.
///
/// Формат файла — просто выход `CryptProtectData`. Своего заголовка
/// нет намеренно: DPAPI кладёт в блоб собственную структуру, и
/// подделать её, не зная ключа пользователя, нельзя. Наш заголовок
/// добавил бы только повод разойтись версиям.
pub struct DpapiKeyStore {
    path: PathBuf,
}

impl DpapiKeyStore {
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

impl KeyStore for DpapiKeyStore {
    fn load(&self) -> Result<Option<DeviceIdentity>, KeyStoreError> {
        let sealed = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.io_error(e)),
        };

        let plain = unprotect(&sealed).map_err(|e| KeyStoreError::Io {
            path: self.path.clone(),
            // `InvalidData`, а не `Other`: файл на месте и читается,
            // негодно его содержимое. Текст ошибки объясняет, что
            // именно случилось (переустановка, чужая машина).
            source: io::Error::new(io::ErrorKind::InvalidData, e.to_string()),
        })?;

        DeviceIdentity::from_secret_bytes(&plain)
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

        let sealed = protect(&identity.to_secret_bytes()).map_err(|e| KeyStoreError::Io {
            path: self.path.clone(),
            source: io::Error::other(e.to_string()),
        })?;

        // Запись атомарная — та же причина, что у `FileKeyStore`:
        // оборванная запись оставила бы полуключ, то есть заставила бы
        // сменить личность из-за сбоя питания.
        let temporary = self.path.with_extension("part");
        fs::write(&temporary, &sealed).map_err(|e| KeyStoreError::Io {
            path: temporary.clone(),
            source: e,
        })?;

        fs::rename(&temporary, &self.path).map_err(|e| self.io_error(e))
    }

    fn protects_at_rest(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("bd-secrets-test-{name}-{}", std::process::id()));
        path.push("device.key");
        path
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn roundtrip_returns_the_same_bytes() {
        let secret = b"tridtsat dva bayta rovno, proverim";
        let sealed = protect(secret).expect("шифрование");
        let plain = unprotect(&sealed).expect("расшифровка");

        assert_eq!(plain, secret);
    }

    /// **Главное свойство: зашифрованное не похоже на исходное.**
    ///
    /// Проверка на способность провалиться (находка 4): реализация,
    /// которая просто копирует байты, прошла бы roundtrip-тест выше и
    /// не защищала бы ничего.
    #[test]
    fn sealed_bytes_do_not_contain_the_secret() {
        let secret = b"ochen sekretnyy klyuch ustroystva";
        let sealed = protect(secret).expect("шифрование");

        assert_ne!(sealed, secret);
        assert!(
            !sealed
                .windows(secret.len())
                .any(|window| window == secret.as_slice()),
            "исходные байты найдены внутри зашифрованных"
        );
        // DPAPI добавляет свою структуру, так что результат заведомо
        // длиннее входа. Равная длина означала бы подмену шифрования
        // чем-то вроде XOR.
        assert!(sealed.len() > secret.len());
    }

    /// Два шифрования одного и того же дают разные байты: DPAPI
    /// подмешивает случайность. Одинаковый выход означал бы, что по
    /// файлу можно узнать, менялся ли ключ.
    #[test]
    fn sealing_twice_gives_different_bytes() {
        let secret = b"odin i tot zhe sekret";

        assert_ne!(
            protect(secret).expect("первое"),
            protect(secret).expect("второе")
        );
    }

    /// Испорченный блоб не расшифровывается и **не паникует**: файл
    /// мог быть обрезан или подменён.
    #[test]
    fn corrupt_blob_is_rejected() {
        let mut sealed = protect(b"chto-to").expect("шифрование");
        // Портим середину: конец мог бы быть отброшен как хвост.
        let middle = sealed.len() / 2;
        sealed[middle] ^= 0xFF;

        assert!(matches!(unprotect(&sealed), Err(SecretsError::NotOurs)));
    }

    #[test]
    fn empty_and_garbage_input_do_not_panic() {
        assert!(unprotect(&[]).is_err());
        assert!(unprotect(&[0u8; 16]).is_err());
        assert!(unprotect(&[0xFFu8; 300]).is_err());
    }

    #[test]
    fn key_survives_save_and_load() {
        let path = temp_path("roundtrip");
        cleanup(&path);
        let store = DpapiKeyStore::new(&path);

        let saved = DeviceIdentity::generate().expect("генерация");
        store.save(&saved).expect("сохранение");

        let loaded = store
            .load()
            .expect("чтение")
            .expect("ключ должен быть на месте");

        assert_eq!(saved.public(), loaded.public());
        cleanup(&path);
    }

    /// **Файл на диске не содержит ключа в открытом виде.**
    ///
    /// Ради этого свойства крейт и написан: без DPAPI 32 байта лежали
    /// бы как есть, и скопировавший файл выдал бы себя за эту машину.
    #[test]
    fn file_on_disk_does_not_contain_the_raw_key() {
        let path = temp_path("not-plain");
        cleanup(&path);
        let store = DpapiKeyStore::new(&path);

        let identity = DeviceIdentity::generate().expect("генерация");
        let raw = identity.to_secret_bytes();
        store.save(&identity).expect("сохранение");

        let on_disk = fs::read(&path).expect("чтение файла");
        assert!(
            !on_disk.windows(raw.len()).any(|window| window == raw),
            "приватный ключ найден в файле открытым текстом"
        );

        cleanup(&path);
    }

    #[test]
    fn missing_key_is_not_an_error() {
        let path = temp_path("missing");
        cleanup(&path);

        assert!(DpapiKeyStore::new(&path)
            .load()
            .expect("чтение отсутствующего")
            .is_none());
    }

    /// Хранилище обязано честно сообщать, что защищает байты: на этом
    /// основано то, что показывается человеку.
    #[test]
    fn dpapi_store_reports_protection() {
        assert!(DpapiKeyStore::new("device.key").protects_at_rest());
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        let path = temp_path("no-leftovers");
        cleanup(&path);
        let store = DpapiKeyStore::new(&path);

        store
            .save(&DeviceIdentity::generate().expect("генерация"))
            .expect("сохранение");

        assert!(!path.with_extension("part").exists());
        cleanup(&path);
    }

    /// Ключ, устойчивый между запусками: `load_or_create` не должен
    /// выпускать новый, если файл на месте.
    #[test]
    fn load_or_create_is_stable() {
        let path = temp_path("stable");
        cleanup(&path);
        let store = DpapiKeyStore::new(&path);

        let first = store.load_or_create().expect("первый запуск");
        let second = store.load_or_create().expect("второй запуск");

        assert_eq!(first.public(), second.public());
        cleanup(&path);
    }

    /// Файл, зашифрованный не для нас, даёт **понятную** причину, а не
    /// «ключ повреждён»: иначе человек после переустановки Windows
    /// пошёл бы искать сбой диска.
    #[test]
    fn foreign_blob_names_the_real_reason() {
        let path = temp_path("foreign");
        cleanup(&path);
        fs::create_dir_all(path.parent().expect("родитель")).expect("каталог");
        // Не наш блоб: случайные байты правдоподобной длины.
        fs::write(&path, [0x01u8; 200]).expect("запись");

        // `expect_err` здесь не годится: он требует `Debug` у
        // успешного значения, а `DeviceIdentity` его намеренно не
        // имеет — приватный ключ не должен попадать в вывод (§4.3.3).
        // Отсутствие `Debug` — свойство типа, а не помеха, поэтому
        // подстраивается тест, а не тип.
        let Err(error) = DpapiKeyStore::new(&path).load() else {
            panic!("чужой блоб обязан быть отвергнут");
        };
        let text = error.to_string();

        assert!(
            text.contains("другого пользователя") || text.contains("другой машины"),
            "причина должна быть названа человеческим языком: {text}"
        );

        cleanup(&path);
    }
}
