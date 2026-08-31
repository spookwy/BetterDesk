//! Выбор энкодера: NVENC или Media Foundation.
//!
//! # Зачем
//!
//! Хостом должна быть **любая** машина, а не только с NVIDIA (§7.1:
//! скачал и запустил). NVENC покрывает NVIDIA, Media Foundation —
//! Intel QuickSync, AMD VCE и всё остальное, что система считает
//! аппаратным.
//!
//! # Порядок выбора и почему именно такой
//!
//! Сначала NVENC, потом MF. Не из симпатии к NVIDIA, а потому что
//! прямой NVENC даёт то, чего через MF получить нельзя:
//!
//! - **Intra Refresh** вместо полных IDR (§5.2, находка 43) — на
//!   плохом канале это разница между рабочей сессией и рывками;
//! - точный VBV и `maxQP` — ручки, которыми лечилась пикселизация
//!   (находка 30);
//! - смена битрейта без пересоздания сессии, нужная этапу 6.
//!
//! Через MF часть этих настроек либо отсутствует, либо молча
//! игнорируется конкретным вендором. Поэтому MF — не замена, а
//! расширение охвата: там, где NVENC есть, он лучше.
//!
//! # Почему enum, а не `Box<dyn Encoder>`
//!
//! У trait'а `Encoder` связанный тип `Input`, и объектом его не
//! сделать без обёрток. Бэкендов при этом ровно два, оба известны при
//! компиляции — та же причина, что у `AnyTransport`.

#![cfg(windows)]

use bd_codec::mediafoundation::{MfEncoder, MfEncoderInput};
use bd_codec::{CodecError, EncodedFrame, Encoder, EncoderConfig};
use bd_core::time::Epoch;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

#[cfg(nvenc_available)]
use bd_codec::nvenc::{EncoderInput as NvencInput, NvencEncoder};

/// Энкодер, выбранный под железо этой машины.
pub enum AnyEncoder {
    /// Прямой NVENC — предпочтительный путь на NVIDIA.
    #[cfg(nvenc_available)]
    Nvenc(Box<NvencEncoder>),
    /// Media Foundation — всё остальное железо.
    MediaFoundation(Box<MfEncoder>),
}

impl AnyEncoder {
    /// Выбрать и создать энкодер под это устройство.
    ///
    /// Устройство обязано быть тем же, на котором идёт захват: иначе
    /// текстуру пришлось бы копировать между GPU (§4.2.3).
    ///
    pub fn new(
        device: &ID3D11Device,
        config: EncoderConfig,
        epoch: Epoch,
    ) -> Result<Self, CodecError> {
        // NVENC пробуется первым — и **его неудача не фатальна**.
        //
        // На машине без NVIDIA он честно вернёт «DLL не загружена»,
        // и это не повод отказываться от работы: ровно ради этого
        // случая и написан путь через MF.
        #[cfg(nvenc_available)]
        {
            match NvencEncoder::for_device(device, config.clone(), epoch) {
                Ok(e) => {
                    tracing::info!("энкодер: NVENC (прямой)");
                    return Ok(Self::Nvenc(Box::new(e)));
                }
                Err(e) => {
                    tracing::info!(
                        error = %e,
                        "NVENC недоступен — перехожу на Media Foundation"
                    );
                }
            }
        }

        let mf = MfEncoder::new(device, config, epoch)?;
        tracing::info!(backend = mf.backend_name(), "энкодер: Media Foundation");
        Ok(Self::MediaFoundation(Box::new(mf)))
    }

    /// Название бэкенда для отчёта.
    ///
    /// Показывается человеку: «NVENC» и «QuickSync» ведут себя
    /// по-разному, и при разборе жалобы это первое, что надо знать.
    pub fn backend_name(&self) -> String {
        match self {
            #[cfg(nvenc_available)]
            Self::Nvenc(_) => "NVENC".to_string(),
            Self::MediaFoundation(e) => format!("Media Foundation: {}", e.backend_name()),
        }
    }

    /// Закодировать кадр из GPU-текстуры.
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        info: bd_core::frame::FrameInfo,
    ) -> Result<Option<EncodedFrame>, CodecError> {
        match self {
            #[cfg(nvenc_available)]
            Self::Nvenc(e) => {
                use windows::core::Interface;
                // Указатель живёт ровно время вызова: `texture` —
                // заимствование, переживающее его по построению.
                e.encode(&NvencInput {
                    texture: texture.as_raw(),
                    info,
                })
            }
            Self::MediaFoundation(e) => e.encode_texture(&MfEncoderInput { texture, info }),
        }
    }

    /// Запросить ключевой кадр.
    pub fn request_keyframe(&mut self) {
        match self {
            #[cfg(nvenc_available)]
            Self::Nvenc(e) => e.request_keyframe(),
            Self::MediaFoundation(e) => e.request_keyframe(),
        }
    }

    /// Сменить битрейт на лету.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), CodecError> {
        match self {
            #[cfg(nvenc_available)]
            Self::Nvenc(e) => e.set_bitrate(bitrate),
            Self::MediaFoundation(e) => e.set_bitrate(bitrate),
        }
    }

    /// Текущие настройки.
    pub fn config(&self) -> &EncoderConfig {
        match self {
            #[cfg(nvenc_available)]
            Self::Nvenc(e) => e.config(),
            Self::MediaFoundation(e) => e.config(),
        }
    }
}
