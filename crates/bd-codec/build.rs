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
