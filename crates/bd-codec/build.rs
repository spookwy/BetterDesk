//! Генерация биндингов к NVENC.
//!
//! # Откуда берётся заголовок
//!
//! Из `vendor/nvcodec/nvEncodeAPI.h` — это заголовок проекта
//! nv-codec-headers под лицензией **MIT**, соответствующий NVIDIA Video
//! Codec SDK 13.0. Подробности и обоснование — `vendor/nvcodec/README.md`.
//!
//! Заголовки из официального дистрибутива SDK не используются: их
//! лицензия не позволяет вендорить файлы в репозиторий (CLAUDE.md §9.1),
//! а версия 13.1 несовместима с драйверами, поддерживающими API 13.0.
//!
//! # Линковка
//!
//! Её нет. `nvEncodeAPI64.dll` грузится в рантайме, поэтому:
//! - сборка не требует установленного SDK;
//! - машина без NVIDIA GPU собирает проект и запускает его, получая
//!   понятную ошибку вместо отказа загрузиться.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(nvenc_available)");

    // NVENC есть только под Windows.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR не задан"));
    // crates/bd-codec/ -> корень репозитория
    let header = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("vendor").join("nvcodec").join("nvEncodeAPI.h"))
        .expect("не удалось определить корень репозитория");

    if !header.exists() {
        println!(
            "cargo:warning=Не найден {} — NVENC-бэкенд отключён. \
             См. vendor/nvcodec/README.md.",
            header.display()
        );
        return;
    }

    println!("cargo:rerun-if-changed={}", header.display());

    // Без libclang биндинги не сгенерировать — но это НЕ повод
    // обрывать сборку.
    //
    // # Почему проверка отдельная, а не паника bindgen
    //
    // Раньше её не было, и `bindgen` паниковал прямо в build.rs.
    // Следствие: **клиент не собирался на машине без LLVM** — хотя
    // клиенту NVENC не нужен вовсе, он ничего не кодирует (находка 47).
    // Человек на той стороне видел «Unable to find libclang» и не мог
    // сделать ничего: сообщение говорит о зависимости чужого крейта,
    // а не о том, что ему делать.
    //
    // Это тот же класс, что находки 47 и 55, но злее: там роль
    // требовала лишнего в рантайме, здесь — **в сборке**, то есть
    // машина не доходила даже до понятного отказа. И это прямо
    // противоречит §7.1 («скачал и запустил»): требовать гигабайт
    // LLVM ради бэкенда, который на этой машине не заработает,
    // нельзя.
    //
    // Проверка идёт ДО генерации, потому что панику build.rs не
    // перехватить, а `clang_sys` умеет ответить честно.
    if let Err(why) = probe_libclang() {
        println!(
            "cargo:warning=NVENC-бэкенд отключён: {why}. \
             Это нормально на машине без NVIDIA — для клиента NVENC не нужен. \
             Чтобы собрать его, поставьте LLVM (https://releases.llvm.org) \
             и при необходимости задайте LIBCLANG_PATH."
        );
        return;
    }

    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        // Только NVENC: иначе в биндинги затянется половина Windows SDK.
        .allowlist_function("NvEncodeAPI.*")
        .allowlist_type("NV_ENC.*")
        .allowlist_type("NVENC.*")
        .allowlist_var("NV_ENC.*")
        .allowlist_var("NVENC.*")
        // Перечисления как константы: так проще сопоставлять с кодами
        // возврата, не заводя лишних типов.
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        .derive_default(true)
        .derive_debug(true)
        // Раскладка местами не совпадает с ожиданиями bindgen; тесты
        // раскладки только шумят в CI, не добавляя гарантий.
        .layout_tests(false)
        // Функции грузятся вручную через GetProcAddress, поэтому
        // объявления `extern` не нужны — и не должны требовать линковки.
        .dynamic_library_name("NvencLib")
        .dynamic_link_require_all(false)
        .generate()
        .expect("не удалось сгенерировать биндинги к nvEncodeAPI.h");

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR не задан"));
    bindings
        .write_to_file(out.join("nvenc_bindings.rs"))
        .expect("не удалось записать биндинги");

    println!("cargo:rustc-cfg=nvenc_available");
}

/// Есть ли на машине libclang, нужный `bindgen`.
///
/// # Почему загрузкой, а не поиском файла
///
/// Проверять существование файла по списку путей — значит повторять
/// логику поиска `bindgen`, причём заведомо неточно: он смотрит и
/// `LIBCLANG_PATH`, и PATH, и системные каталоги. Разойдясь с ним,
/// проверка либо отключала бы NVENC там, где он собрался бы, либо
/// пропускала бы вперёд к панике — то есть не решала бы задачу.
///
/// Попытка загрузить библиотеку отвечает на тот самый вопрос, который
/// нас интересует: сможет ли `bindgen` ею воспользоваться. Тот же
/// урок, что находка 51: «система вернула список» и «эти вещи
/// работают» — разные утверждения, и второе проверяется попыткой.
fn probe_libclang() -> Result<(), String> {
    // Порядок как у bindgen: сначала явно заданный путь, потом
    // обычные имена, которые ищутся по PATH и системным каталогам.
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(dir) = env::var("LIBCLANG_PATH") {
        let dir = PathBuf::from(dir);
        // LIBCLANG_PATH может указывать и на каталог, и на сам файл.
        if dir.is_dir() {
            for name in LIBCLANG_NAMES {
                candidates.push(dir.join(name));
            }
        } else {
            candidates.push(dir);
        }
    }
    // Голое имя: сработает, если каталог LLVM есть в PATH.
    for name in LIBCLANG_NAMES {
        candidates.push(PathBuf::from(name));
    }

    // Типовые каталоги установки.
    //
    // Нужны потому, что установщик LLVM под Windows **не добавляет
    // себя в PATH по умолчанию**. На машине разработчика libclang.dll
    // лежит в `C:\Program Files\LLVM\bin`, PATH о нём не знает, и
    // проверка «по имени» отвечала бы «нет» там, где `bindgen`
    // прекрасно находит библиотеку сам.
    //
    // Ошибиться здесь в эту сторону хуже, чем не проверять вовсе:
    // NVENC молча отключился бы на машине, где он собирается, и хост
    // остался бы без энкодера без единого сообщения (родственно
    // находке 32 — проверка воспроизводила не то состояние).
    for dir in LIBCLANG_DIRS {
        for name in LIBCLANG_NAMES {
            candidates.push(PathBuf::from(dir).join(name));
        }
    }

    for candidate in &candidates {
        // SAFETY-примечание не требуется: build.rs собирается как
        // обычный бинарь, а `Library::new` сама по себе безопасна с
        // точки зрения этого крейта — мы ничего из библиотеки не
        // вызываем, только проверяем загружаемость.
        if unsafe { libloading::Library::new(candidate) }.is_ok() {
            return Ok(());
        }
    }

    Err(format!(
        "не найден libclang (искали {})",
        LIBCLANG_NAMES.join(", ")
    ))
}

/// Имена динамической библиотеки libclang по платформам.
#[cfg(windows)]
const LIBCLANG_NAMES: &[&str] = &["libclang.dll", "clang.dll"];

#[cfg(not(windows))]
const LIBCLANG_NAMES: &[&str] = &["libclang.so", "libclang.dylib"];

/// Каталоги, где библиотека лежит при обычной установке.
///
/// Установщик LLVM под Windows не прописывает себя в PATH, поэтому
/// без этого списка проверка отвечала бы «нет» на машине, где
/// `bindgen` находит libclang без всяких усилий.
#[cfg(windows)]
const LIBCLANG_DIRS: &[&str] = &[
    r"C:\Program Files\LLVM\bin",
    r"C:\Program Files (x86)\LLVM\bin",
];

#[cfg(not(windows))]
const LIBCLANG_DIRS: &[&str] = &["/usr/lib", "/usr/local/lib", "/usr/lib/llvm/lib"];
