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
//!
//! # Отсутствие libclang — не ошибка
//!
//! `bindgen` требует libclang и **паникует** без него, обрывая сборку
//! всего workspace. Раньше это означало, что клиент не собирался на
//! машине без LLVM — ради бэкенда, который там всё равно не заработает
//! и который клиенту не нужен (находка 61).
//!
//! Теперь бэкенд просто отключается: `cfg(nvenc_available)` не
//! выставляется, `AnyEncoder` остаётся на Media Foundation, а человек
//! видит предупреждение вместо непонятной паники.

include!("../../build-support/nvenc_probe.rs");

fn main() {
    println!("cargo:rustc-check-cfg=cfg(nvenc_available)");
    println!("cargo:rerun-if-changed=../../vendor/nvcodec/nvEncodeAPI.h");
    println!("cargo:rerun-if-changed=../../build-support/nvenc_probe.rs");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");

    let header = match nvenc_buildable() {
        Ok(header) => header,
        Err(why) => {
            // Не под Windows — молча: там NVENC нет по определению, и
            // предупреждение на каждой сборке только шумело бы.
            if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
                println!(
                    "cargo:warning=NVENC-бэкенд отключён: {why}. \
                     Это нормально на машине без NVIDIA — для клиента NVENC не нужен. \
                     Чтобы собрать его, поставьте LLVM (https://releases.llvm.org) \
                     и при необходимости задайте LIBCLANG_PATH."
                );
            }
            return;
        }
    };

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
