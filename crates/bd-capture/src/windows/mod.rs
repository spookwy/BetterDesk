//! Windows-бэкенды захвата.
//!
//! Весь `unsafe` крейта живёт здесь и в подмодулях. Наружу (в `lib.rs`
//! и выше) уходят только безопасные типы.

mod device;
mod dxgi;

pub use device::D3dDevice;
pub use dxgi::{CursorState, DxgiCapturer, DxgiFrame};

use crate::{CaptureError, MonitorInfo, Result};

/// Перечислить мониторы, доступные для захвата.
///
/// Порядок соответствует индексам выходов адаптера: индекс из
/// [`MonitorInfo::index`] можно передать в [`DxgiCapturer::new`].
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    device::enumerate_monitors()
}

/// Проверить, доступен ли DXGI-захват на этой системе.
///
/// Полезно для выбора бэкенда: на гибридной графике DXGI может быть
/// недоступен, и тогда нужен WGC (CLAUDE.md §5.1).
pub fn dxgi_available() -> bool {
    match enumerate_monitors() {
        Ok(monitors) => !monitors.is_empty(),
        Err(err) => {
            tracing::debug!(?err, "DXGI недоступен");
            false
        }
    }
}

/// Преобразовать `HRESULT` в ошибку захвата.
///
/// Вынесено сюда, чтобы соответствие кодов и вариантов ошибки было
/// в одном месте, а не размазано по вызовам.
pub(crate) fn hresult_to_error(
    hr: ::windows::core::HRESULT,
    context: &'static str,
) -> CaptureError {
    use ::windows::Win32::Foundation::E_ACCESSDENIED;
    use ::windows::Win32::Graphics::Dxgi::{
        DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
        DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED,
    };

    match hr {
        // Штатная и частая ситуация: смена разрешения, UAC, Ctrl+Alt+Del,
        // переключение GPU. Требует пересоздания сессии, но не завершения.
        DXGI_ERROR_ACCESS_LOST | DXGI_ERROR_SESSION_DISCONNECTED => CaptureError::AccessLost,
        // Secure desktop: пока на экране UAC-диалог или экран блокировки,
        // пользовательская сессия не вправе дуплицировать рабочий стол,
        // и `DuplicateOutput` отвечает именно этим кодом.
        //
        // Отдельный вариант, а не `Platform`, потому что реакция другая:
        // ждать и повторять, а не падать. Без этого первый же UAC
        // завершает процесс — что и наблюдалось на живом железе.
        E_ACCESSDENIED => CaptureError::DesktopUnavailable,
        // Дупликация уже занята (например, запущен OBS) или исчерпан лимит.
        DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => CaptureError::Unavailable,
        DXGI_ERROR_UNSUPPORTED => {
            CaptureError::Unsupported("DXGI Desktop Duplication не поддерживается".into())
        }
        other => CaptureError::Platform {
            context,
            code: other.0 as u32,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::windows::Win32::Foundation::{E_ACCESSDENIED, E_OUTOFMEMORY};
    use ::windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST;

    #[test]
    fn uac_maps_to_recoverable_desktop_unavailable() {
        // Дефект, найденный на живом железе: при UAC-диалоге
        // `DuplicateOutput` отвечает `E_ACCESSDENIED`. Раньше этот код
        // не был разобран и уезжал в `Platform`, который невосстановим,
        // — первый же UAC завершал процесс.
        //
        // Проверяется именно классификация: она и есть то место, где
        // «штатное событие» превращалось в «фатальную ошибку».
        let err = hresult_to_error(E_ACCESSDENIED, "DuplicateOutput");
        assert!(
            matches!(err, CaptureError::DesktopUnavailable),
            "E_ACCESSDENIED должен читаться как secure desktop, а не {err:?}"
        );
        assert!(err.is_recoverable());
        assert!(err.needs_backoff());
    }

    #[test]
    fn access_lost_maps_to_access_lost() {
        let err = hresult_to_error(DXGI_ERROR_ACCESS_LOST, "AcquireNextFrame");
        assert!(matches!(err, CaptureError::AccessLost));
    }

    #[test]
    fn unknown_code_stays_fatal() {
        // Обратная сторона: расширяя список восстановимых кодов, легко
        // объявить восстановимым всё подряд. Незнакомый код обязан
        // остаться фатальным, иначе настоящая поломка уйдёт в
        // бесконечный цикл повторов.
        let err = hresult_to_error(E_OUTOFMEMORY, "DuplicateOutput");
        assert!(matches!(err, CaptureError::Platform { .. }));
        assert!(!err.is_recoverable());
    }
}
