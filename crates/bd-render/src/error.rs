//! Ошибки рендера.

/// Результат операций рендера.
pub type Result<T> = std::result::Result<T, RenderError>;

/// Ошибка создания окна или вывода кадра.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// Не удалось создать окно.
    #[error("не удалось создать окно: {0}")]
    WindowCreation(String),

    /// Не удалось создать swapchain или его ресурсы.
    #[error("не удалось создать swapchain: {0}")]
    SwapChain(String),

    /// Ошибка компиляции шейдера.
    ///
    /// Шейдер зашит в бинарь, поэтому в норме случиться не может.
    /// Если случилась — это опечатка в HLSL, а не проблема машины
    /// пользователя, и сообщение компилятора важно увидеть целиком.
    #[error("шейдер не скомпилировался: {0}")]
    ShaderCompilation(String),

    /// Устройство D3D11 потеряно.
    ///
    /// Происходит при обновлении драйвера, TDR (зависание GPU),
    /// отключении внешнего монитора. Лечится пересозданием всего
    /// стека рендера, а не повторной попыткой того же вызова.
    #[error("устройство D3D11 потеряно: {0}")]
    DeviceLost(String),

    /// Не удалось подготовить курсор к отрисовке.
    ///
    /// Форма курсора приходит по сети (CLAUDE.md §8.5), поэтому может
    /// быть несогласованной или в неизвестном формате. **Это не повод
    /// прерывать сессию:** без курсора работать можно, без картинки —
    /// нет. Вызывающий должен пропустить кадр курсора и продолжить.
    #[error("не удалось подготовить курсор: {0}")]
    Cursor(String),

    /// Ошибка вызова Windows API в горячем пути.
    #[error("ошибка рендера: {context} (HRESULT {hresult:#010x})")]
    Platform {
        /// Что именно выполнялось.
        context: &'static str,
        /// Код возврата.
        hresult: u32,
    },
}

impl RenderError {
    /// Требует ли ошибка пересоздания стека рендера.
    ///
    /// Потеря устройства — штатное событие на машине пользователя
    /// (обновился драйвер, сработал TDR), а не повод падать.
    pub fn needs_recreation(&self) -> bool {
        matches!(self, RenderError::DeviceLost(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_lost_asks_for_recreation() {
        assert!(RenderError::DeviceLost("TDR".into()).needs_recreation());
    }

    #[test]
    fn shader_error_is_not_recoverable_at_runtime() {
        // Шейдер зашит в бинарь: пересоздание устройства его не починит.
        let err = RenderError::ShaderCompilation("syntax error".into());
        assert!(!err.needs_recreation());
    }
}
