//! Ошибки кодирования и декодирования.

/// Результат операций кодека.
pub type Result<T> = std::result::Result<T, CodecError>;

/// Ошибка энкодера или декодера.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// Аппаратный энкодер недоступен на этой системе.
    ///
    /// Нет GPU NVIDIA, слишком старый драйвер, или это виртуальная
    /// машина без проброса GPU. Продукт требует аппаратного энкода
    /// (CLAUDE.md §1), поэтому это фатально для роли хоста.
    #[error("аппаратный энкодер недоступен: {0}")]
    Unavailable(String),

    /// Версия NVENC в драйвере ниже версии SDK, с которой собран проект.
    ///
    /// NVENC даёт обратную совместимость только в одну сторону: старый
    /// SDK работает на новом драйвере, но не наоборот. Драйвер не знает
    /// раскладки структур будущих версий и отвергает вызов.
    ///
    /// Решений два, и оба валидны:
    /// - собрать проект с SDK, соответствующим драйверу (для сборки
    ///   у пользователей это правильнее — не требует от них действий);
    /// - обновить драйвер до версии, поддерживающей нужный API.
    #[error(
        "версия NVENC в драйвере ({driver_major}.{driver_minor}) ниже версии SDK \
         ({sdk_major}.{sdk_minor}); нужен SDK {driver_major}.x или более новый драйвер"
    )]
    DriverApiTooOld {
        /// Мажорная версия API в драйвере.
        driver_major: u32,
        /// Минорная версия API в драйвере.
        driver_minor: u32,
        /// Мажорная версия API в SDK.
        sdk_major: u32,
        /// Минорная версия API в SDK.
        sdk_minor: u32,
    },

    /// Запрошенные параметры не поддерживаются.
    #[error("параметры кодирования не поддерживаются: {0}")]
    UnsupportedParams(String),

    /// Ошибка при инициализации сессии кодирования.
    #[error("не удалось инициализировать энкодер: {0}")]
    InitFailed(String),

    /// Ошибка в процессе кодирования кадра.
    #[error("ошибка кодирования: {context} (NVENCSTATUS {status})")]
    Encode {
        /// Что именно выполнялось.
        context: &'static str,
        /// Код возврата NVENC.
        status: u32,
    },

    /// Аппаратный декодер недоступен на этой системе.
    ///
    /// В отличие от энкода, это не фатально: у клиента может не быть
    /// ни NVIDIA, ни рабочего GPU вообще (CLAUDE.md §0.1), и тогда
    /// остаётся программный путь через `openh264` (§5.5).
    #[error("аппаратный декодер недоступен: {0}")]
    DecoderUnavailable(String),

    /// Ошибка в процессе декодирования кадра.
    #[error("ошибка декодирования: {context} (HRESULT {hresult:#010x})")]
    Decode {
        /// Что именно выполнялось.
        context: &'static str,
        /// Код возврата Windows API.
        hresult: u32,
    },

    /// Поток повреждён или потеряна синхронизация.
    ///
    /// Штатная ситуация на канале с потерями, а не сбой: пропал
    /// опорный кадр. Лечится запросом ключевого кадра у хоста, а не
    /// пересозданием декодера.
    #[error("поток повреждён: {0}")]
    CorruptStream(String),

    /// Недостаточно памяти GPU.
    #[error("недостаточно памяти GPU для кодирования")]
    OutOfMemory,
}

impl CodecError {
    /// Стоит ли пробовать другой бэкенд.
    ///
    /// Приоритет реализации — NVENC → AMF → QuickSync (CLAUDE.md §5.2).
    /// Если NVENC недоступен, имеет смысл попробовать следующий; если
    /// же он есть, но параметры неверны — другой бэкенд не поможет.
    pub fn should_try_next_backend(&self) -> bool {
        matches!(
            self,
            CodecError::Unavailable(_)
                | CodecError::DriverApiTooOld { .. }
                | CodecError::DecoderUnavailable(_)
        )
    }

    /// Лечится ли ошибка запросом ключевого кадра.
    ///
    /// На канале с потерями декодер регулярно теряет синхронизацию.
    /// Пересоздавать его при этом — лишние миллисекунды и чёрный экран;
    /// достаточно попросить хост прислать опорный кадр (CLAUDE.md §5.2).
    pub fn needs_keyframe(&self) -> bool {
        matches!(self, CodecError::CorruptStream(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_suggests_next_backend() {
        // Нет NVIDIA — надо пробовать AMF/QSV, а не сдаваться.
        assert!(CodecError::Unavailable("нет GPU".into()).should_try_next_backend());
        assert!(CodecError::DriverApiTooOld {
            driver_major: 13,
            driver_minor: 0,
            sdk_major: 13,
            sdk_minor: 1,
        }
        .should_try_next_backend());
    }

    #[test]
    fn bad_params_do_not_suggest_next_backend() {
        // Если параметры неверны, другой энкодер их тоже не примет.
        let err = CodecError::UnsupportedParams("8K@240".into());
        assert!(!err.should_try_next_backend());
    }
}
