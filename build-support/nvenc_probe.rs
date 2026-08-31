// Общее условие доступности NVENC-бэкенда для всех build.rs.
// 
// # Зачем отдельный файл, а не копия в каждом скрипте
// 
// Флаги `cfg` не наследуются между крейтами, поэтому каждый крейт,
// которому нужен `nvenc_available`, обязан вычислить его сам. Скриптов
// три: `bd-codec` (владелец флага), `bd-host` и `bd-bench`.
// 
// Пока условие было простым («есть ли заголовок»), копии держались
// согласованными сами собой. Как только к нему добавилась вторая
// половина — наличие libclang (находка 61), — копии немедленно
// разъехались: правку внесли в два скрипта из трёх, и `bd-host`
// перестал собираться на машине без LLVM, объявляя NVENC собранным
// там, где `bd-codec` его отключил.
// 
// Ошибка при этом громкая (`unresolved import bd_codec::nvenc`), но
// указывает **не туда**: человек читает про импорт в чужом крейте,
// а причина — в расхождении двух build.rs.
// 
// Поэтому условие живёт в одном файле и подключается через
// `include!`. Крейт сборки заводить ради тридцати строк дороже, а
// `include!` даёт то самое свойство, которого не хватало: копия
// ровно одна.
// 
// Подключается так:
// 
// ```ignore
// include!("../../build-support/nvenc_probe.rs");
// ```

use std::env;
use std::path::PathBuf;

/// Имена динамической библиотеки libclang.
#[cfg(windows)]
const LIBCLANG_NAMES: &[&str] = &["libclang.dll", "clang.dll"];

#[cfg(not(windows))]
const LIBCLANG_NAMES: &[&str] = &["libclang.so", "libclang.dylib"];

/// Каталоги, где библиотека лежит при обычной установке.
///
/// Установщик LLVM под Windows не прописывает себя в PATH, поэтому
/// поиска по имени недостаточно: проверка отвечала бы «нет» на машине,
/// где `bindgen` находит libclang без всяких усилий, и NVENC молча
/// отключался бы там, где собирается (находка 61).
#[cfg(windows)]
const LIBCLANG_DIRS: &[&str] = &[
    r"C:\Program Files\LLVM\bin",
    r"C:\Program Files (x86)\LLVM\bin",
];

#[cfg(not(windows))]
const LIBCLANG_DIRS: &[&str] = &["/usr/lib", "/usr/local/lib", "/usr/lib/llvm/lib"];

/// Есть ли на машине libclang, нужный `bindgen`.
///
/// Проверка **попыткой загрузить**, а не поиском файла по списку
/// путей: попытка отвечает на тот вопрос, который задан, — сможет ли
/// `bindgen` этим воспользоваться. Повторять его логику поиска
/// вручную значит разойтись с ней и ошибаться в обе стороны
/// (родственно находке 51).
fn libclang_present() -> bool {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // Явно заданный путь имеет приоритет; может указывать и на
    // каталог, и на сам файл.
    if let Ok(dir) = env::var("LIBCLANG_PATH") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            candidates.extend(LIBCLANG_NAMES.iter().map(|n| dir.join(n)));
        } else {
            candidates.push(dir);
        }
    }

    // Голое имя: сработает, если каталог LLVM есть в PATH.
    candidates.extend(LIBCLANG_NAMES.iter().map(PathBuf::from));

    // Типовые каталоги установки.
    for dir in LIBCLANG_DIRS {
        candidates.extend(LIBCLANG_NAMES.iter().map(|n| PathBuf::from(dir).join(n)));
    }

    candidates.iter().any(|candidate| {
        // Библиотека только загружается для проверки; ничего из неё
        // не вызывается.
        unsafe { libloading::Library::new(candidate) }.is_ok()
    })
}

/// Путь к вендорённому заголовку NVENC.
fn nvenc_header() -> Option<PathBuf> {
    let manifest =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR не задан"));
    // crates/<крейт>/ -> корень репозитория
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("vendor").join("nvcodec").join("nvEncodeAPI.h"))
}

/// Собирается ли NVENC-бэкенд на этой машине.
///
/// Условий два, и оба обязательны: вендорённый заголовок (он в
/// репозитории, см. `vendor/nvcodec/README.md`) и libclang для
/// генерации биндингов.
///
/// `Err` несёт причину словами — её печатают в предупреждение, чтобы
/// человек знал, чего не хватает и нужно ли ему это вообще.
fn nvenc_buildable() -> Result<PathBuf, String> {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Err("NVENC есть только под Windows".into());
    }

    let header = nvenc_header().ok_or("не удалось определить корень репозитория")?;
    if !header.exists() {
        return Err(format!(
            "не найден {} (см. vendor/nvcodec/README.md)",
            header.display()
        ));
    }

    if !libclang_present() {
        return Err(format!(
            "не найден libclang (искали {})",
            LIBCLANG_NAMES.join(", ")
        ));
    }

    Ok(header)
}
