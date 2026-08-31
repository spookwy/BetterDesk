//! Загрузка NVENC: библиотека драйвера и таблица функций.
//!
//! # Как устроен вход в NVENC
//!
//! Функции по одной не экспортируются. Порядок такой:
//! 1. загрузить `nvEncodeAPI64.dll` (её ставит драйвер NVIDIA);
//! 2. спросить `NvEncodeAPIGetMaxSupportedVersion` — что умеет драйвер;
//! 3. вызвать `NvEncodeAPICreateInstance`, которая заполнит структуру
//!    указателей на все остальные функции.
//!
//! DLL грузится в рантайме, а не линкуется: иначе приложение не
//! запустилось бы на машине без NVIDIA GPU — вместо понятного
//! сообщения пользователь получил бы отказ загрузчика.

use super::sys;
use crate::{CodecError, Result};
use std::sync::OnceLock;

/// Загруженная библиотека NVENC и её таблица функций.
pub struct NvencApi {
    /// Библиотека держится живой: указатели в `functions` ведут внутрь неё.
    /// Порядок полей важен — Rust уничтожает поля сверху вниз, так что
    /// таблица «умрёт» раньше библиотеки.
    functions: sys::NV_ENCODE_API_FUNCTION_LIST,
    _lib: sys::NvencLib,
    api_version: u32,
}

// SAFETY: после инициализации таблица неизменна и содержит только
// указатели на функции драйвера, потокобезопасные по документации
// NVENC. Сессия энкодера потокобезопасной не является, но это
// отдельный тип со своими гарантиями.
unsafe impl Send for NvencApi {}
unsafe impl Sync for NvencApi {}

impl NvencApi {
    /// Получить API, загрузив его при первом обращении.
    ///
    /// Результат кешируется: загрузка DLL не бесплатна, а её итог
    /// неизменен в пределах процесса. Ошибка тоже кешируется — если
    /// GPU нет, повторные попытки ничего не изменят.
    pub fn get() -> Result<&'static NvencApi> {
        static API: OnceLock<std::result::Result<NvencApi, String>> = OnceLock::new();

        API.get_or_init(|| NvencApi::load().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|msg| CodecError::Unavailable(msg.clone()))
    }

    /// Загрузить библиотеку и таблицу функций.
    fn load() -> Result<Self> {
        // SAFETY: загрузка произвольной DLL небезопасна в общем случае,
        // но имя фиксировано и ведёт к системной библиотеке драйвера
        // NVIDIA. Отсутствие файла — штатная ситуация (нет GPU NVIDIA),
        // обрабатывается как ошибка, а не паника.
        let lib = unsafe { sys::NvencLib::new("nvEncodeAPI64.dll") }.map_err(|e| {
            CodecError::Unavailable(format!(
                "не удалось загрузить nvEncodeAPI64.dll ({e}); \
                 нет GPU NVIDIA или не установлен драйвер"
            ))
        })?;

        // Что поддерживает драйвер.
        let mut driver_version: u32 = 0;
        // SAFETY: функция принимает указатель на u32 для записи
        // результата; передаётся валидная локальная переменная.
        let status = unsafe { lib.NvEncodeAPIGetMaxSupportedVersion(&mut driver_version) };
        check_status(status, "NvEncodeAPIGetMaxSupportedVersion")?;

        // Формат ответа: 4 младших бита — minor, остальное — major.
        // Это НЕ формат NVENCAPI_VERSION, где minor сдвинут на 24 бита.
        let driver_major = driver_version >> 4;
        let driver_minor = driver_version & 0xF;
        let sdk_version = (sys::NVENCAPI_MAJOR_VERSION << 4) | sys::NVENCAPI_MINOR_VERSION;

        // Совместимость NVENC односторонняя: приложение со СТАРЫМ SDK
        // работает на новом драйвере, но не наоборот — драйвер не знает
        // раскладки структур будущих версий.
        //
        // Подставить версию драйвера в поля `version` не помогает:
        // проверено на драйвере 591.91 (API 13.0) с заголовками 13.1 —
        // nvEncOpenEncodeSessionEx всё равно отвергает вызов. Поэтому
        // проект собирается с заголовками 13.0 (vendor/nvcodec/README.md).
        if driver_version < sdk_version {
            tracing::warn!(
                driver = format!("{driver_major}.{driver_minor}"),
                sdk = format!(
                    "{}.{}",
                    sys::NVENCAPI_MAJOR_VERSION,
                    sys::NVENCAPI_MINOR_VERSION
                ),
                "версия NVENC в драйвере ниже версии заголовков"
            );
            return Err(CodecError::DriverApiTooOld {
                driver_major,
                driver_minor,
                sdk_major: sys::NVENCAPI_MAJOR_VERSION,
                sdk_minor: sys::NVENCAPI_MINOR_VERSION,
            });
        }

        let mut functions = sys::NV_ENCODE_API_FUNCTION_LIST {
            version: super::versions::FUNCTION_LIST,
            ..Default::default()
        };

        // SAFETY: структура обнулена, поле version заполнено требуемым
        // значением — обязательное условие контракта функции.
        // Передаётся валидный указатель на локальную переменную.
        let status = unsafe { lib.NvEncodeAPICreateInstance(&mut functions) };
        check_status(status, "NvEncodeAPICreateInstance")?;

        // Драйвер обязан заполнить как минимум открытие сессии. Молча
        // упасть на нулевом указателе хуже, чем вернуть внятную ошибку.
        if functions.nvEncOpenEncodeSessionEx.is_none() {
            return Err(CodecError::Unavailable(
                "драйвер не предоставил nvEncOpenEncodeSessionEx".into(),
            ));
        }

        tracing::info!(
            headers = format!(
                "{}.{}",
                sys::NVENCAPI_MAJOR_VERSION,
                sys::NVENCAPI_MINOR_VERSION
            ),
            driver = format!("{driver_major}.{driver_minor}"),
            "NVENC загружен"
        );

        Ok(Self {
            functions,
            _lib: lib,
            api_version: sys::NVENCAPI_VERSION,
        })
    }

    /// Версия API для полей `version` и `apiVersion` в структурах.
    pub(crate) fn api_version(&self) -> u32 {
        self.api_version
    }

    /// Таблица функций.
    pub(crate) fn functions(&self) -> &sys::NV_ENCODE_API_FUNCTION_LIST {
        &self.functions
    }
}

/// Проверить код возврата NVENC и превратить в `Result`.
///
/// Каждый вызов C-API обязан проверяться (CLAUDE.md §4.3): игнорирование
/// кода возврата — источник тихой порчи памяти.
pub(crate) fn check_status(status: sys::_NVENCSTATUS::Type, context: &'static str) -> Result<()> {
    use sys::_NVENCSTATUS as S;

    match status {
        S::NV_ENC_SUCCESS => Ok(()),
        S::NV_ENC_ERR_NO_ENCODE_DEVICE | S::NV_ENC_ERR_UNSUPPORTED_DEVICE => Err(
            CodecError::Unavailable(format!("{context}: устройство не поддерживает NVENC")),
        ),
        // Совместимость драйвера проверена в `load`, поэтому здесь это
        // ошибка заполнения структуры в нашем коде (versions.rs).
        S::NV_ENC_ERR_INVALID_VERSION => Err(CodecError::UnsupportedParams(format!(
            "{context}: драйвер отверг версию структуры"
        ))),
        S::NV_ENC_ERR_UNSUPPORTED_PARAM => Err(CodecError::UnsupportedParams(context.to_string())),
        S::NV_ENC_ERR_OUT_OF_MEMORY => Err(CodecError::OutOfMemory),
        other => Err(CodecError::Encode {
            context,
            status: other as u32,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_status_is_ok() {
        assert!(check_status(sys::_NVENCSTATUS::NV_ENC_SUCCESS, "тест").is_ok());
    }

    #[test]
    fn no_device_maps_to_unavailable() {
        let err = check_status(sys::_NVENCSTATUS::NV_ENC_ERR_NO_ENCODE_DEVICE, "тест")
            .expect_err("должна быть ошибка");
        // Важно, чтобы это вело к пробе следующего бэкенда, а не к отказу.
        assert!(err.should_try_next_backend());
    }

    #[test]
    fn invalid_version_is_reported_as_params_error() {
        // На этом уровне неизвестно, чья версия не подошла. Проверка
        // совместимости драйвера живёт в `load`; здесь ошибка означает
        // неверно заполненную структуру.
        let err = check_status(sys::_NVENCSTATUS::NV_ENC_ERR_INVALID_VERSION, "тест")
            .expect_err("должна быть ошибка");
        assert!(matches!(err, CodecError::UnsupportedParams(_)));
    }
}
