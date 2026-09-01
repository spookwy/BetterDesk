//! Сборка оболочки.
//!
//! Помимо `tauri_build::build()` копирует дизайн-токены в каталог
//! фронтенда.
//!
//! # Зачем копировать, а не держать две копии
//!
//! `docs/design/tokens.css` — единственный источник цветов (§6.2).
//! Вторая копия рядом с `index.html` неизбежно разошлась бы с ним:
//! правку внесли бы в одну, а смотрели бы на другую, и расхождение
//! проявилось бы не ошибкой, а неверным цветом — то есть тем, что
//! замечают глазом и не замечают тестами.
//!
//! Webview не может читать файл выше своего корня, поэтому копия
//! всё же нужна — но она **производная**, и правится только
//! оригинал. `rerun-if-changed` следит за оригиналом.

use std::path::PathBuf;

fn main() {
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR не задан"));

    // ui/src-tauri -> ui -> корень репозитория
    let root = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("оболочка лежит не там, где ожидалось");

    let src = root.join("docs").join("design").join("tokens.css");
    let dst = manifest
        .parent()
        .expect("нет ui/")
        .join("src")
        .join("tokens.css");

    println!("cargo:rerun-if-changed={}", src.display());

    match std::fs::copy(&src, &dst) {
        Ok(_) => {}
        Err(e) => {
            // Падать нельзя: без токенов экран будет некрашеным, но
            // соберётся, и человек увидит, что именно не так. Молчать
            // тоже нельзя — некрашеный экран иначе выглядит багом
            // вёрстки, а не отсутствием файла.
            println!(
                "cargo:warning=не удалось скопировать {}: {e}. \
                 Оболочка соберётся без токенов и будет выглядеть неверно.",
                src.display()
            );
        }
    }

    // Бинари сессии внутрь оболочки: один файл вместо трёх (§7.1).
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR не задан"));
    declare_blob(root, &out_dir, "bd-host", "BD_HOST_BLOB");
    declare_blob(root, &out_dir, "bd-client", "BD_CLIENT_BLOB");

    tauri_build::build()
}

/// Найти бинарь продукта и объявить его путь для `include_bytes!`.
///
/// # Почему не падаем, когда его нет
///
/// Оболочку должно быть можно собрать раньше продукта — иначе порядок
/// сборки диктовал бы порядок работы, и `cargo build` в пустом
/// клоне падал бы на первом же шаге. Вместо этого кладём пустой файл,
/// а модуль `embedded` честно отвечает «не встроено» и ищет бинари
/// рядом, как раньше.
///
/// Предупреждение при этом обязательно: молчаливая сборка без
/// продукта дала бы `.exe`, который не умеет запускать сессию, и
/// понять это можно было бы только нажав кнопку.
fn declare_blob(root: &std::path::Path, out_dir: &std::path::Path, name: &str, var: &str) {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    // Только release: встраивать отладочный бинарь в раздаваемый
    // файл незачем — он втрое больше и медленнее.
    let candidate = root.join("target").join("release").join(&exe);
    println!("cargo:rerun-if-changed={}", candidate.display());

    if candidate.exists() {
        println!("cargo:rustc-env={var}={}", candidate.display());
        return;
    }

    // Пустышка вместо отсутствующего бинаря.
    let stub = out_dir.join(format!("{name}.empty"));
    if std::fs::write(&stub, b"").is_err() {
        panic!("не удалось создать заглушку {}", stub.display());
    }
    println!("cargo:rustc-env={var}={}", stub.display());
    println!(
        "cargo:warning={exe} не найден — оболочка соберётся, но не сможет \
         запускать сессию. Соберите продукт: cargo build --release -p {name}"
    );
}
