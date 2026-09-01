//! Встроенные бинари сессии и их распаковка.
//!
//! # Зачем это нужно
//!
//! §7.1 требует «скачал и запустил»: человек, которому дали файл, не
//! должен ни ставить программу, ни держать рядом три `.exe`. Отправить
//! один файл в мессенджер — это то, что люди делают не задумываясь;
//! отправить папку с тремя файлами — уже инструкция.
//!
//! Оболочка при этом обязана запускать сессию **отдельным процессом**
//! (см. `session`): окно сессии — нативное Win32 со своим циклом
//! сообщений, и внутри Tauri ему не место. Значит бинари нужны как
//! файлы на диске — но приносить их с собой можно внутри себя.
//!
//! # Как это работает
//!
//! `bd-host.exe` и `bd-client.exe` вкомпилированы в оболочку через
//! `include_bytes!` и при первом запуске распаковываются во временный
//! каталог. Дальше `session` запускает их оттуда.
//!
//! Цена — около 7 МБ к размеру оболочки. Это меньше, чем стоит
//! объяснять человеку, какие файлы положить рядом.
//!
//! # Чего здесь нет намеренно
//!
//! Нет проверки подписи и хеша: распаковываем то, что сами же в себя
//! и вложили при сборке. Подпись появится на этапе 8 вместе с
//! Authenticode — тогда проверять будет что и чем (§8.4).
//!
//! # Сборка без встраивания
//!
//! Если бинарей нет на месте сборки, `build.rs` кладёт пустышки, а
//! этот модуль честно отвечает «не встроено» и `session` ищет файлы
//! рядом, как раньше. Иначе оболочку нельзя было бы собрать раньше
//! продукта — то есть порядок сборки диктовал бы порядок работы.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Байты `bd-host.exe`, вложенные при сборке.
///
/// Пустой срез означает «собирали без продукта» — см. `build.rs`.
const HOST_BYTES: &[u8] = include_bytes!(env!("BD_HOST_BLOB"));

/// Байты `bd-client.exe`.
const CLIENT_BYTES: &[u8] = include_bytes!(env!("BD_CLIENT_BLOB"));

/// Какой бинарь нужен.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binary {
    Host,
    Client,
}

impl Binary {
    fn bytes(self) -> &'static [u8] {
        match self {
            Binary::Host => HOST_BYTES,
            Binary::Client => CLIENT_BYTES,
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Binary::Host => "bd-host.exe",
            Binary::Client => "bd-client.exe",
        }
    }
}

/// Есть ли встроенные бинари.
pub fn is_embedded() -> bool {
    !HOST_BYTES.is_empty() && !CLIENT_BYTES.is_empty()
}

/// Каталог, куда распаковываются бинари.
///
/// Во временном каталоге пользователя с именем, включающим версию:
/// иначе после обновления оболочки рядом остался бы старый бинарь, и
/// запускался бы он. Такую ошибку почти невозможно заметить — версия
/// нигде не видна, а поведение прежнее (ровно находка 15).
fn unpack_dir() -> PathBuf {
    std::env::temp_dir().join(format!("betterdesk-{}", env!("CARGO_PKG_VERSION")))
}

/// Распаковать бинарь и вернуть путь к нему.
///
/// Если файл уже на месте и того же размера — не переписываем: он
/// может быть запущен прямо сейчас, и Windows не даст заменить
/// работающий файл.
pub fn ensure_unpacked(binary: Binary) -> Result<PathBuf, String> {
    let bytes = binary.bytes();
    if bytes.is_empty() {
        return Err(format!("{} не встроен в эту сборку", binary.file_name()));
    }

    let dir = unpack_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;

    let path = dir.join(binary.file_name());

    // Размер как признак «тот же файл».
    //
    // Не хеш: считать его на 3.4 МБ при каждом запуске — заметная
    // пауза перед окном, а версия уже разделена каталогом. Внутри
    // одной версии байты те же по построению.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() == bytes.len() as u64 {
            return Ok(path);
        }
    }

    write_atomically(&path, bytes)?;
    Ok(path)
}

/// Записать файл целиком, не оставляя обрезка при сбое.
///
/// Через временное имя и переименование: прерванная запись прямо в
/// целевой файл оставила бы наполовину записанный `.exe`, который
/// Windows попыталась бы запустить.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("part");

    {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| format!("не удалось создать {}: {e}", tmp.display()))?;
        file.write_all(bytes)
            .map_err(|e| format!("не удалось записать {}: {e}", tmp.display()))?;
        file.sync_all()
            .map_err(|e| format!("не удалось сбросить {} на диск: {e}", tmp.display()))?;
    }

    // Старый файл может быть занят запущенной сессией — тогда
    // переименование откажет. Это не повод падать: раз он занят,
    // значит он же и работает, и он нужной версии (каталог общий
    // только внутри версии).
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        if path.exists() {
            return Ok(());
        }
        return Err(format!("не удалось положить {}: {e}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpack_dir_carries_the_version() {
        // Каталог обязан меняться с версией: иначе после обновления
        // оболочки рядом останется прежний бинарь, и запустится он —
        // без единого признака, что версия не та.
        let dir = unpack_dir();
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(name.starts_with("betterdesk-"));
        assert!(
            name.contains(env!("CARGO_PKG_VERSION")),
            "в имени каталога должна быть версия: {name}"
        );
    }

    #[test]
    fn file_names_are_distinct() {
        assert_ne!(Binary::Host.file_name(), Binary::Client.file_name());
    }

    #[test]
    fn missing_blob_is_reported_not_panicked() {
        // Сборка без продукта — законный случай (см. build.rs), и она
        // обязана давать объяснение, а не панику при запуске.
        if !is_embedded() {
            let err = ensure_unpacked(Binary::Host).unwrap_err();
            assert!(err.contains("не встроен"), "{err}");
        }
    }

    #[test]
    fn write_atomically_leaves_no_part_file() {
        let dir = std::env::temp_dir().join("betterdesk-test-atomic");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sample.bin");
        write_atomically(&path, b"content").expect("запись");

        assert_eq!(std::fs::read(&path).expect("чтение"), b"content");
        assert!(
            !path.with_extension("part").exists(),
            "временный файл обязан исчезнуть: иначе рядом копится мусор"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn embedded_binaries_unpack_and_run() {
        // Главная проверка встраивания: распакованный файл обязан
        // быть работающим `.exe`, а не просто присутствовать.
        //
        // Проверяется запуском с `--help`: он ничего не делает, но
        // проходит весь путь загрузчика Windows. Файл, записанный
        // наполовину или не тот, здесь и провалится.
        if !is_embedded() {
            // Сборка без продукта — законный случай, проверять нечего.
            return;
        }

        for kind in [Binary::Host, Binary::Client] {
            let path = ensure_unpacked(kind).expect("распаковка");
            assert!(path.exists(), "{} не появился", path.display());

            let size = std::fs::metadata(&path).expect("метаданные").len();
            assert!(
                size > 1_000_000,
                "{} подозрительно мал: {size}",
                path.display()
            );

            let out = std::process::Command::new(&path)
                .arg("--help")
                .output()
                .unwrap_or_else(|e| panic!("{} не запустился: {e}", path.display()));
            assert!(
                out.status.success(),
                "{} вернул {:?}",
                path.display(),
                out.status.code()
            );
        }
    }

    #[test]
    fn second_unpack_reuses_the_file() {
        // Повторная распаковка не должна переписывать файл: он может
        // быть запущен прямо сейчас, и Windows не даст его заменить.
        if !is_embedded() {
            return;
        }
        let first = ensure_unpacked(Binary::Host).expect("первая распаковка");
        let before = std::fs::metadata(&first)
            .expect("метаданные")
            .modified()
            .ok();
        let second = ensure_unpacked(Binary::Host).expect("вторая распаковка");
        assert_eq!(first, second);
        let after = std::fs::metadata(&second)
            .expect("метаданные")
            .modified()
            .ok();
        assert_eq!(before, after, "файл переписан без нужды");
    }
}
