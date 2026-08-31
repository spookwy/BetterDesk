//! Проброс флага доступности NVENC.
//!
//! `bd-codec` включает NVENC-бэкенд по `cfg(nvenc_available)`, который
//! выставляет его собственный build.rs. Флаги cfg не наследуются между
//! крейтами, поэтому здесь повторяется та же проверка — иначе проба
//! не узнает, собран ли бэкенд.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(nvenc_available)");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Условие то же, что в crates/bd-codec/build.rs: наличие
    // вендорённого заголовка (vendor/nvcodec/README.md).
    let manifest =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR не задан"));
    let header = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("vendor").join("nvcodec").join("nvEncodeAPI.h"));

    println!("cargo:rerun-if-changed=../../vendor/nvcodec/nvEncodeAPI.h");

    // Условий ДВА, и оба обязаны совпадать с bd-codec: заголовок и
    // libclang. Проверять только заголовок нельзя — на машине без
    // LLVM bd-codec отключает бэкенд, а проба объявляла бы его
    // собранным и не компилировалась бы, ссылаясь на несуществующий
    // тип.
    //
    // Расхождение двух build.rs даёт ошибку компиляции, а не тихий
    // дефект, — но ошибку в чужом крейте и без объяснения причины.
    if header.is_some_and(|h| h.exists()) && libclang_present() {
        println!("cargo:rustc-cfg=nvenc_available");
    }
}

/// Есть ли libclang — то же условие, что в `crates/bd-codec/build.rs`.
///
/// Проверка дублируется, а не выносится в общий крейт: ради
/// двух десятков строк заводить крейт сборки дороже, чем держать
/// их согласованными. Если правится одна — правится и вторая.
fn libclang_present() -> bool {
    let names: &[&str] = if cfg!(windows) {
        &["libclang.dll", "clang.dll"]
    } else {
        &["libclang.so", "libclang.dylib"]
    };
    // Установщик LLVM под Windows не добавляет себя в PATH, поэтому
    // одного поиска по имени мало (см. bd-codec/build.rs).
    let dirs: &[&str] = if cfg!(windows) {
        &[
            r"C:\Program Files\LLVM\bin",
            r"C:\Program Files (x86)\LLVM\bin",
        ]
    } else {
        &["/usr/lib", "/usr/local/lib", "/usr/lib/llvm/lib"]
    };

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = env::var("LIBCLANG_PATH") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            candidates.extend(names.iter().map(|n| dir.join(n)));
        } else {
            candidates.push(dir);
        }
    }
    candidates.extend(names.iter().map(PathBuf::from));
    for dir in dirs {
        candidates.extend(names.iter().map(|n| PathBuf::from(dir).join(n)));
    }

    candidates
        .iter()
        .any(|c| unsafe { libloading::Library::new(c) }.is_ok())
}
