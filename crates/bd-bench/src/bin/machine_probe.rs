//! Проба машины: что здесь есть и на что эта машина годится.
//!
//! # Зачем отдельная проба
//!
//! Остальные пробы отвечают на вопрос «с какой задержкой работает
//! подсистема», и почти все требуют NVENC. Эта отвечает на другой
//! вопрос — **запустится ли BetterDesk на этой машине вообще**, и
//! годится ли она в хосты, в клиенты или ни во что.
//!
//! Разница принципиальная. Пробу запускает человек, который не читал
//! этот код, на машине, к которой у разработчика нет доступа. Значит:
//!
//! - она **не должна падать** на первой же неудаче. Отказ NVENC — это
//!   не конец проверки, а один из результатов: машина не годится в
//!   хосты, но может быть отличным клиентом;
//! - каждый ответ печатается словами, а не кодом ошибки;
//! - в конце — вывод, который человек может переслать одной строкой.
//!
//! # Почему это не заменяет живой прогон
//!
//! Проба говорит «подсистема создалась», а не «картинка правильная».
//! Находка 45 показала, что глаз видит то, чего не видит ни один
//! автоматический ответ. Проба нужна, чтобы **до** живого прогона
//! знать, чего от машины ждать, и не разбирать вслепую жалобу
//! «ничего не работает».
//!
//! Запуск: `cargo run --release -p bd-bench --bin machine_probe`

// Открытие сессии NVENC требует сырого указателя на устройство
// D3D11, то есть `unsafe` (§4.3). Остальная проба его не использует.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(windows))]
fn main() {
    println!("BetterDesk работает только на Windows (CLAUDE.md §3.1).");
}

#[cfg(windows)]
fn main() {
    // Логи по умолчанию выключены: человеку за чужой машиной нужен
    // вывод пробы, а не поток INFO от подсистем. Включаются через
    // RUST_LOG, если разработчик попросит подробностей.
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .init();
    }

    println!("=== BetterDesk: проверка машины ===\n");

    let capture = check_capture();
    let encoder = check_encoder();
    let decoder = check_decoder(&capture);
    let other_encoders = list_other_encoders();

    verdict(&capture, &encoder, &decoder, &other_encoders);
}

/// Результат проверки одной подсистемы.
///
/// Свой тип, а не `Result`: неудача здесь — не ошибка выполнения, а
/// **факт о машине**, который надо напечатать и учесть в выводе.
/// `Result` подталкивал бы к `?`, то есть к выходу на первой же
/// неудаче — ровно тому, чего проба делать не должна.
#[cfg(windows)]
enum Check {
    Ok(String),
    Failed(String),
}

#[cfg(windows)]
impl Check {
    fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    fn report(&self, title: &str) {
        match self {
            Self::Ok(details) => println!("✅ {title}: {details}"),
            Self::Failed(why) => println!("❌ {title}: {why}"),
        }
    }
}

/// Захват экрана через DXGI.
#[cfg(windows)]
fn check_capture() -> Check {
    use bd_capture::windows::{enumerate_monitors, DxgiCapturer};
    use bd_capture::Capturer;
    use bd_core::time::Epoch;

    let monitors = match enumerate_monitors() {
        Ok(m) if !m.is_empty() => m,
        Ok(_) => return Check::Failed("мониторов не найдено".into()),
        Err(e) => return Check::Failed(format!("не удалось перечислить мониторы: {e}")),
    };

    let mut listed = String::new();
    for m in &monitors {
        listed.push_str(&format!(
            "\n     монитор {}: {} — {}x{}",
            m.index, m.device_name, m.size.width, m.size.height
        ));
    }

    // Захват пробуется по-настоящему, а не только перечислением:
    // именно создание дупликации ломается на гибридной графике
    // (§5.1), а список мониторов при этом успешно возвращается.
    match DxgiCapturer::new(monitors[0].index, Epoch::new()) {
        Ok(c) => {
            let size = c.size();
            // Размер дупликации и размер из перечисления мониторов
            // могут расходиться: перечисление отдаёт логический
            // размер (с учётом масштаба Windows), а дупликация —
            // физические пиксели. При масштабе 150 % это 1707x1067
            // против 2560x1600.
            //
            // Расхождение печатается, а не скрывается: две разные
            // цифры в одном отчёте без объяснения — это ровно то,
            // на чём горит находка 34. Работает пайплайн по размеру
            // дупликации, он здесь и главный.
            let logical = monitors[0].size;
            let note = if logical.width != size.width || logical.height != size.height {
                format!(
                    "\n     (перечисление показывает {}x{} — это логический\n      \
                     размер с учётом масштаба Windows; кодируется {}x{})",
                    logical.width, logical.height, size.width, size.height
                )
            } else {
                String::new()
            };
            Check::Ok(format!(
                "DXGI Desktop Duplication работает, кодируется {}x{}{listed}{note}",
                size.width, size.height
            ))
        }
        Err(e) => Check::Failed(format!(
            "DXGI не работает ({e}).\n     \
             Частая причина — гибридная графика: DXGI требует, чтобы\n     \
             приложение шло на том же GPU, что и дисплей. Фоллбэк\n     \
             на WGC запланирован (§5.1).{listed}"
        )),
    }
}

/// Аппаратный энкодер: NVENC, а при его отсутствии — Media Foundation.
///
/// # Почему Media Foundation проверяется ВСЕГДА
///
/// Раньше вся эта функция сводилась к одной ветке `cfg`: без
/// `nvenc_available` она сразу возвращала отказ, и до Media
/// Foundation дело не доходило. Пока LLVM был обязателен для сборки,
/// это ничего не меняло — флаг стоял всегда.
///
/// После находки 61 клиент собирается без LLVM, и ветка «NVENC не
/// собран» стала обычным делом. Тогда и выяснилось, что она **не
/// проверяет ничего**: машина с исправным AMD-энкодером получала
/// «энкодера нет», хотя `MfEncoder` от NVENC не зависит ни в чём.
///
/// Компилятор об этом честно предупреждал — `try_media_foundation is
/// never used`, — но предупреждение появлялось только на сборке без
/// LLVM, то есть у того, кто и так видел неверный вывод, а не у того,
/// кто мог его исправить.
///
/// Тот же класс, что находки 47, 55, 61: путь, отключённый заодно с
/// соседним, хотя от него не зависит.
#[cfg(windows)]
fn check_encoder() -> Check {
    use bd_capture::windows::{enumerate_monitors, DxgiCapturer};
    use bd_capture::Capturer;
    use bd_core::time::Epoch;

    // Энкодеру нужно устройство D3D11, а оно живёт в капчурере.
    // Если захват не работает, про энкодер сказать нечего —
    // и это честный ответ, а не «не проверено».
    let monitors = match enumerate_monitors() {
        Ok(m) if !m.is_empty() => m,
        _ => return Check::Failed("нечего проверять: нет мониторов".into()),
    };
    let capturer = match DxgiCapturer::new(monitors[0].index, Epoch::new()) {
        Ok(c) => c,
        Err(_) => {
            return Check::Failed(
                "не проверен: без захвата нет устройства D3D11 для энкодера".into(),
            )
        }
    };

    let size = capturer.size();

    // Сначала NVENC, если бэкенд собран. `None` означает «не собран»,
    // а не «не работает» — это разные вещи, и путать их нельзя:
    // первое про сборку, второе про машину.
    match try_nvenc(&capturer, size) {
        Some(Ok(details)) => Check::Ok(details),
        // NVENC есть, но отказал. Media Foundation покрывает Intel
        // QuickSync, AMD VCE и прочее аппаратное железо, поэтому его
        // неудача больше не означает «хостом быть нельзя».
        Some(Err(e)) => match try_media_foundation(&capturer, size) {
            Ok(name) => Check::Ok(format!(
                "Media Foundation: {name}\n     \
                 (NVENC недоступен, и это нормально: {e})"
            )),
            Err(mf_err) => Check::Failed(format!(
                "аппаратного энкодера нет.\n     \
                 NVENC: {e}\n     \
                 Media Foundation: {mf_err}\n     \
                 Хостом эта машина быть не сможет, клиентом — да."
            )),
        },
        // Бэкенд не собран — но это ничего не говорит о машине.
        None => match try_media_foundation(&capturer, size) {
            Ok(name) => Check::Ok(format!(
                "Media Foundation: {name}\n     \
                 (бэкенд NVENC не собран — нет LLVM/libclang; для этой\n     \
                 машины он и не нужен, энкодер найден другой)"
            )),
            Err(mf_err) => Check::Failed(format!(
                "аппаратного энкодера нет.\n     \
                 Media Foundation: {mf_err}\n     \
                 NVENC не проверялся: бэкенд не собран (нет LLVM/libclang).\n     \
                 Если в машине стоит видеокарта NVIDIA — поставьте LLVM\n     \
                 (https://releases.llvm.org) и повторите: возможно, хостом\n     \
                 она быть сможет.\n     \
                 Клиентом — в любом случае да, если декодер ниже ✅."
            )),
        },
    }
}

/// Попробовать NVENC.
///
/// `None` — бэкенд не собран (нет LLVM/libclang); `Some(Err)` — собран,
/// но отказал. Разница существенна: первое про сборку и лечится
/// установкой LLVM, второе про машину и означает отсутствие NVIDIA.
///
/// Отдельная функция, а не блок `cfg` внутри вызывающего: там она
/// заставляла бы вызывающего заканчиваться по-разному в двух
/// конфигурациях, и одна из веток обрастала бы `return`, лишними в
/// другой (на что clippy справедливо жалуется).
#[cfg(all(windows, nvenc_available))]
fn try_nvenc(
    capturer: &bd_capture::windows::DxgiCapturer,
    size: bd_core::frame::FrameSize,
) -> Option<Result<String, String>> {
    use bd_codec::nvenc::NvencEncoder;
    use bd_codec::EncoderConfig;
    use bd_core::time::Epoch;

    let config = EncoderConfig::low_latency(size, 60);
    let bitrate = config.rate_control.target_bitrate();

    // SAFETY: устройство принадлежит `capturer`, который жив дольше
    // энкодера — тот дропается до возврата из этой функции.
    Some(
        match unsafe { NvencEncoder::new(capturer.device_ptr(), config, Epoch::new()) } {
            Ok(_) => Ok(format!(
                "NVENC открыл сессию {}x{}@60, CBR {} Мбит/с",
                size.width,
                size.height,
                bitrate / 1_000_000
            )),
            Err(e) => Err(e.to_string()),
        },
    )
}

/// Бэкенд NVENC не собран — проверять нечего.
#[cfg(all(windows, not(nvenc_available)))]
fn try_nvenc(
    _capturer: &bd_capture::windows::DxgiCapturer,
    _size: bd_core::frame::FrameSize,
) -> Option<Result<String, String>> {
    None
}

/// Аппаратный декодер H.264.
#[cfg(windows)]
fn check_decoder(capture: &Check) -> Check {
    use bd_capture::windows::{enumerate_monitors, DxgiCapturer};
    use bd_codec::mediafoundation::D3d11Decoder;
    use bd_codec::DecoderConfig;
    use bd_core::frame::FrameSize;
    use bd_core::time::Epoch;

    if !capture.is_ok() {
        return Check::Failed("не проверен: без захвата нет устройства D3D11".into());
    }

    let monitors = match enumerate_monitors() {
        Ok(m) if !m.is_empty() => m,
        _ => return Check::Failed("нечего проверять: нет мониторов".into()),
    };
    let capturer = match DxgiCapturer::new(monitors[0].index, Epoch::new()) {
        Ok(c) => c,
        Err(e) => return Check::Failed(format!("не проверен: захват не создался ({e})")),
    };

    // Размер берётся фиксированный 1080p, а не с монитора: декодер
    // проверяется как способность машины, и она не должна зависеть
    // от того, какой здесь экран.
    let size = FrameSize::new(1920, 1080);
    match D3d11Decoder::new(
        capturer.device(),
        DecoderConfig::low_latency(size),
        Epoch::new(),
    ) {
        Ok(_) => Check::Ok("H.264 D3D11VA готов (встроенный декодер Windows)".into()),
        Err(e) => Check::Failed(format!(
            "аппаратного декода нет ({e}).\n     \
             Софтверный декодер (openh264, §5.5) запланирован, но ещё\n     \
             не сделан — пока эта машина не сможет показывать видео."
        )),
    }
}

/// Какие H.264-энкодеры вообще есть на машине.
///
/// # Зачем, если реализован только NVENC
///
/// Затем, что ответ «NVENC нет» не отвечает на нужный вопрос. Машина
/// без NVIDIA может иметь энкодер Intel QuickSync или AMD — и тогда
/// она **могла бы** хостить, если написать бэкенд (§5.2 их и
/// предполагает). А может не иметь ни одного — и тогда бэкенд ей не
/// поможет, и писать его ради неё бессмысленно.
///
/// Разницу между этими случаями нельзя узнать, глядя на отказ NVENC,
/// а решение о нескольких днях работы от неё прямо зависит.
#[cfg(windows)]
fn list_other_encoders() -> Vec<bd_codec::mediafoundation::EncoderEntry> {
    bd_codec::mediafoundation::h264_encoders().unwrap_or_default()
}

/// Итог: на что эта машина годится.
#[cfg(windows)]
fn verdict(
    capture: &Check,
    encoder: &Check,
    decoder: &Check,
    other_encoders: &[bd_codec::mediafoundation::EncoderEntry],
) {
    capture.report("Захват экрана");
    encoder.report("Энкодер");
    decoder.report("Декодер");

    // Список печатается всегда, даже когда NVENC работает: на машине
    // разработчика он показывает, что вообще умеет находиться, и
    // тем самым проверяет саму проверку. Пустой список у всех подряд
    // означал бы, что перечисление сломано, а не что энкодеров нет
    // (тот же урок, что находка 26: ноль — это «неизвестно»).
    let hardware: Vec<_> = other_encoders.iter().filter(|e| e.hardware).collect();
    println!("\nЭнкодеры H.264, заявленные системой:");
    if other_encoders.is_empty() {
        println!("  (ни одного — ни аппаратного, ни софтверного)");
    } else {
        for e in other_encoders {
            let kind = if e.hardware {
                "аппаратный"
            } else {
                "софтверный"
            };
            println!("  • {} — {kind}", e.name);
        }
    }
    // «Заявлено» — не «работает»: MFT из списка может отказать при
    // создании, ровно как NVENC отказал на несовместимых заголовках
    // (находка 12). Проверка списком ничего не доказывает о
    // работоспособности, и обещать обратное нельзя.
    if !hardware.is_empty() && !encoder.is_ok() {
        // Список непустой, а энкодер не создался. Раньше это означало
        // «бэкенда нет»; теперь бэкенд есть (Media Foundation), и
        // значит причина другая — чаще всего чужой адаптер.
        println!(
            "\n  ↑ Система заявляет {} аппаратных энкодеров, но ни один",
            hardware.len()
        );
        println!("    не подошёл. Обычная причина — гибридная графика:");
        println!("    энкодер должен видеть тот же GPU, что владеет кадром.");
        println!("    Подробности: RUST_LOG=bd_codec=debug");
    }

    println!("\n--- Вывод ---");

    // Роли разделены намеренно (§3.1): хост захватывает и кодирует,
    // клиент принимает и показывает. Машина может годиться в одну
    // роль и не годиться в другую, и «не работает» — слишком грубый
    // ответ, чтобы быть полезным.
    let can_host = capture.is_ok() && encoder.is_ok();
    let can_client = decoder.is_ok();

    if can_host {
        println!("✅ ХОСТ: машина может отдавать свой экран.");
    } else {
        // Называем, чего именно не хватило.
        //
        // «Нужны и захват, и энкодер» верно, но бесполезно: человек
        // не знает, что из двух подвело, и на машине с исправным
        // захватом идёт проверять захват. Причина обязана быть
        // конкретной — это единственное, ради чего проба и пишется
        // для чужой машины (находка 48).
        let missing = match (capture.is_ok(), encoder.is_ok()) {
            (false, false) => "нет ни захвата экрана, ни энкодера",
            (false, true) => "энкодер есть, но недоступен захват экрана",
            (true, false) => "захват работает, но нет аппаратного энкодера",
            (true, true) => unreachable!("этот случай — can_host"),
        };
        println!("❌ ХОСТ: нет ({missing}).");
    }

    if can_client {
        println!("✅ КЛИЕНТ: машина может смотреть чужой экран.");
        println!("   Команда: bd-client --connect АДРЕС:7000");
    } else {
        println!("❌ КЛИЕНТ: нет (нужен декодер H.264).");
    }

    if !can_host && !can_client {
        println!("\nСейчас BetterDesk на этой машине не запустится.");
        println!("Перешлите разработчику весь вывод выше целиком —");
        println!("по нему видно, какой именно случай встретился.");
    }
}

/// Попробовать энкодер Media Foundation.
///
/// Отдельно от ветки NVENC, потому что это **не фоллбэк «на всякий
/// случай», а полноценный путь** для машин без NVIDIA: Intel
/// QuickSync, AMD VCE и всё, что система считает аппаратным.
///
/// Возвращает имя выбранного MFT — оно нужно в отчёте: «QuickSync» и
/// «AMD VCE» ведут себя по-разному, и при разборе жалобы знать это
/// надо первым делом.
#[cfg(windows)]
fn try_media_foundation(
    capturer: &bd_capture::windows::DxgiCapturer,
    size: bd_core::frame::FrameSize,
) -> Result<String, String> {
    use bd_codec::mediafoundation::MfEncoder;
    use bd_codec::EncoderConfig;
    use bd_core::time::Epoch;

    MfEncoder::new(
        capturer.device(),
        EncoderConfig::low_latency(size, 60),
        Epoch::new(),
    )
    .map(|e| e.backend_name().to_string())
    .map_err(|e| e.to_string())
}
