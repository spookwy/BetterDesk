//! Проброс флага доступности NVENC.
//!
//! `bd-codec` включает NVENC-бэкенд по `cfg(nvenc_available)`, который
//! выставляет его собственный build.rs. Флаги cfg не наследуются между
//! крейтами, поэтому здесь повторяется та же проверка — иначе проба
//! не узнает, собран ли бэкенд.
//!
//! Условие берётся из общего файла: три копии одного условия уже
//! разошлись однажды (находка 61). Подробности — в
//! `build-support/nvenc_probe.rs`.

include!("../../build-support/nvenc_probe.rs");

fn main() {
    println!("cargo:rustc-check-cfg=cfg(nvenc_available)");
    println!("cargo:rerun-if-changed=../../vendor/nvcodec/nvEncodeAPI.h");
    println!("cargo:rerun-if-changed=../../build-support/nvenc_probe.rs");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");

    if nvenc_buildable().is_ok() {
        println!("cargo:rustc-cfg=nvenc_available");
    }
}
