//! Проброс флага доступности NVENC.
//!
//! `bd-codec` включает NVENC-бэкенд по `cfg(nvenc_available)`, который
//! выставляет его собственный build.rs. Флаги cfg не наследуются между
//! крейтами, поэтому здесь повторяется та же проверка — иначе хост
//! не узнает, собран ли бэкенд, и остался бы только на Media Foundation.

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

    if header.is_some_and(|h| h.exists()) {
        println!("cargo:rustc-cfg=nvenc_available");
    }
}
