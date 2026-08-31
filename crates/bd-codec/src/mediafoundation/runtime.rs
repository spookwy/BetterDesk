//! Инициализация Media Foundation и менеджер устройств DXGI.

use crate::{CodecError, Result};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Multithread};
use windows::Win32::Media::MediaFoundation::{
    IMFDXGIDeviceManager, MFCreateDXGIDeviceManager, MFShutdown, MFStartup, MFSTARTUP_LITE,
    MF_VERSION,
};

/// Счётчик живых пользователей Media Foundation.
///
/// `MFStartup`/`MFShutdown` — парные и глобальные на процесс. Декодеров
/// в процессе может быть несколько (многомониторность, этап 11), а
/// вызвать `MFShutdown` при живом MFT — значит получить россыпь
/// `MF_E_SHUTDOWN` в самых неожиданных местах. Поэтому счётчик, а не
/// «инициализировать один раз и не выключать».
static USERS: AtomicUsize = AtomicUsize::new(0);

/// Сериализация самих `MFStartup`/`MFShutdown`.
///
/// Атомарного счётчика мало: между «увидел ноль» и «вызвал MFStartup»
/// другой поток может успеть сделать то же самое.
static INIT_LOCK: Mutex<()> = Mutex::new(());

/// Живая подписка на Media Foundation.
///
/// Пока значение живо, `MFShutdown` не вызывается (CLAUDE.md §4.3.4).
pub struct MediaFoundation {
    _private: (),
}

impl MediaFoundation {
    /// Инициализировать Media Foundation (или присоединиться к уже
    /// инициализированной).
    pub fn startup() -> Result<Self> {
        let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        if USERS.load(Ordering::Acquire) == 0 {
            // MFSTARTUP_LITE не поднимает сокетный слой: он нужен
            // сетевым источникам, а мы кормим декодер байтами сами.
            // SAFETY: версия — константа из заголовков; функция не
            // принимает указателей.
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }
                .map_err(|e| CodecError::DecoderUnavailable(format!("MFStartup: {e}")))?;
            tracing::debug!("Media Foundation инициализирована");
        }

        USERS.fetch_add(1, Ordering::AcqRel);
        Ok(Self { _private: () })
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        if USERS.fetch_sub(1, Ordering::AcqRel) == 1 {
            // SAFETY: парный вызов к MFStartup; последний пользователь
            // ушёл, значит живых объектов MF не осталось.
            if let Err(e) = unsafe { MFShutdown() } {
                tracing::warn!(error = %e, "MFShutdown завершился ошибкой");
            }
        }
    }
}

/// Менеджер устройств DXGI: то, через что MFT получает наш D3D11-девайс.
///
/// Без него декодер вернёт кадр в системной памяти, и весь смысл
/// аппаратного декода пропадёт — картинку пришлось бы загружать
/// обратно в GPU (CLAUDE.md §4.2.3).
pub struct DxgiDeviceManager {
    manager: IMFDXGIDeviceManager,
}

impl DxgiDeviceManager {
    /// Создать менеджер поверх существующего D3D11-устройства.
    ///
    /// Устройство обязано быть многопоточно-защищённым: MFT работает
    /// с ним из своих потоков параллельно с нашим рендером. Без
    /// `SetMultithreadProtected` это тихая порча состояния устройства,
    /// а не ошибка вызова.
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let multithread: ID3D11Multithread = device.cast().map_err(|e| {
            CodecError::DecoderUnavailable(format!("ID3D11Multithread недоступен: {e}"))
        })?;
        // SAFETY: интерфейс получен из живого устройства.
        // Возвращается прежнее состояние флага — нам оно не нужно.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };

        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        // SAFETY: оба выходных указателя ссылаются на живые локальные
        // переменные, которые функция заполняет.
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }.map_err(|e| {
            CodecError::DecoderUnavailable(format!("MFCreateDXGIDeviceManager: {e}"))
        })?;

        let manager = manager.ok_or_else(|| {
            CodecError::DecoderUnavailable(
                "MFCreateDXGIDeviceManager вернул пустой менеджер".into(),
            )
        })?;

        // SAFETY: менеджер только что создан; `token` — тот, что
        // вернул создатель, иначе ResetDevice отвергнет вызов.
        unsafe { manager.ResetDevice(device, token) }
            .map_err(|e| CodecError::DecoderUnavailable(format!("ResetDevice: {e}")))?;

        Ok(Self { manager })
    }

    /// Значение для `MFT_MESSAGE_SET_D3D_MANAGER`.
    ///
    /// MFT принимает менеджер как `ULONG_PTR`, но ссылку **не
    /// удерживает** через подсчёт ссылок надёжным образом, поэтому
    /// менеджер обязан жить дольше MFT. Это обеспечивается порядком
    /// полей в декодере, а не надеждой.
    pub fn as_ulong_ptr(&self) -> usize {
        self.manager.as_raw() as usize
    }
}

// SAFETY: IMFDXGIDeviceManager потокобезопасен по контракту MF — он
// и создан ради раздачи устройства нескольким потокам. Перемещение
// между потоками тем более безопасно.
unsafe impl Send for DxgiDeviceManager {}
