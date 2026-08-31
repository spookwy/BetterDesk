//! Кодирование кадров через NVENC.

use super::api::check_status;
use super::resource::{BitstreamBuffer, RegisteredResource};
use super::session::NvencSession;
use super::{sys, versions};
use crate::{CodecError, EncodedFrame, Encoder, EncoderConfig, FrameKind, RateControl, Result};
use bd_core::frame::FrameInfo;
use bd_core::metrics::Stage;
use bd_core::time::Epoch;
use std::collections::HashMap;
use std::ffi::c_void;

/// Кадр, поступающий в энкодер.
///
/// Несёт указатель на GPU-текстуру, а не пиксели: копирование в
/// системную память запрещено (CLAUDE.md §4.2.3).
pub struct EncoderInput {
    /// Указатель на `ID3D11Texture2D`.
    ///
    /// # Safety
    ///
    /// Должен быть валиден на момент вызова `encode` и указывать на
    /// текстуру того размера, с которым создан энкодер.
    pub texture: *mut c_void,
    /// Метаданные и тайминги кадра.
    pub info: FrameInfo,
}

/// Аппаратный энкодер NVENC.
///
/// # Порядок полей важен
///
/// Rust уничтожает поля сверху вниз. Буфер битстрима и регистрации
/// текстур принадлежат сессии: если она закроется первой, их
/// освобождение вернёт `NV_ENC_ERR_DEVICE_NOT_EXIST` и ресурсы
/// драйвера утекут. Поэтому `session` объявлена **последней** —
/// она умирает после всего, что от неё зависит.
pub struct NvencEncoder {
    /// Кеш регистраций: одна текстура — одна регистрация.
    ///
    /// DXGI переиспользует небольшой набор текстур по кругу, а
    /// `nvEncRegisterResource` — дорогая операция. Регистрировать
    /// заново на каждый кадр значило бы отдать миллисекунды впустую.
    /// Ключ — адрес текстуры.
    registered: HashMap<usize, RegisteredResource>,
    bitstream: BitstreamBuffer,
    /// Уничтожается последней — см. заметку о порядке выше.
    session: NvencSession,
    epoch: Epoch,
    /// Запрошен ли ключевой кадр на следующем вызове.
    force_keyframe: bool,
    /// Номер кадра для NVENC.
    frame_index: u64,
}

impl NvencEncoder {
    /// Создать энкодер поверх типизированного устройства D3D11.
    ///
    /// Безопасная обёртка над [`Self::new`]: время жизни устройства
    /// проверяет заимствование, а не комментарий. Предпочитать её —
    /// сырой указатель нужен только там, где его требует C-ABI
    /// (CLAUDE.md §4.3.3).
    ///
    /// Существует потому, что `bd-bench` и будущий `bd-host` объявлены
    /// с `#![forbid(unsafe_code)]`: без этой обёртки им пришлось бы
    /// ослабить запрет ради одного вызова, то есть отдать гарантию,
    /// ради которой §4.3 и написан.
    pub fn for_device(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        config: EncoderConfig,
        epoch: Epoch,
    ) -> Result<Self> {
        use windows::core::Interface;
        // SAFETY: указатель получен из живой ссылки на устройство и
        // используется только внутри вызова; заимствование гарантирует,
        // что устройство переживёт создание энкодера, а сам энкодер
        // держит на него ссылку через сессию NVENC.
        unsafe { Self::new(device.as_raw(), config, epoch) }
    }

    /// Создать энкодер для D3D11-устройства.
    ///
    /// # Safety
    ///
    /// `device` — валидный указатель на `ID3D11Device`, переживающий
    /// возвращённый энкодер.
    pub unsafe fn new(device: *mut c_void, config: EncoderConfig, epoch: Epoch) -> Result<Self> {
        // SAFETY: контракт по `device` передаётся вызывающему выше.
        let session = unsafe { NvencSession::open_d3d11(device, config) }?;
        // SAFETY: сессия только что открыта и жива.
        let bitstream = unsafe { BitstreamBuffer::new(session.api(), session.handle()) }?;

        Ok(Self {
            registered: HashMap::new(),
            bitstream,
            session,
            epoch,
            force_keyframe: false,
            frame_index: 0,
        })
    }

    /// Убедиться, что текстура зарегистрирована.
    ///
    /// Ничего не возвращает намеренно: возврат ссылки удерживал бы
    /// заимствование `self` до конца кодирования и мешал бы читать
    /// остальные поля. Сама регистрация достаётся отдельным
    /// неизменяемым доступом к `registered`.
    ///
    /// # Safety
    ///
    /// `texture` — валидный `ID3D11Texture2D` нужного размера.
    unsafe fn ensure_registered(&mut self, texture: *mut c_void) -> Result<()> {
        let key = texture as usize;
        if self.registered.contains_key(&key) {
            return Ok(());
        }

        let size = self.session.config().size;
        // SAFETY: сессия жива; контракт по `texture` — на вызывающем.
        let res = unsafe {
            RegisteredResource::register_d3d11(
                self.session.api(),
                self.session.handle(),
                texture,
                size.width,
                size.height,
            )
        }?;
        tracing::debug!(
            texture = format!("{texture:p}"),
            total = self.registered.len() + 1,
            "текстура зарегистрирована в NVENC"
        );
        self.registered.insert(key, res);
        Ok(())
    }
}

impl Encoder for NvencEncoder {
    type Input = EncoderInput;

    fn encode(&mut self, frame: &EncoderInput) -> Result<Option<EncodedFrame>> {
        if frame.texture.is_null() {
            return Err(CodecError::UnsupportedParams(
                "передан пустой указатель на текстуру".into(),
            ));
        }

        // Всё, что нужно от сессии, читается до регистрации ресурса:
        // `resource_for` берёт `&mut self`, и одновременное чтение
        // `self.session` конфликтовало бы с ним.
        let api = self.session.api();
        let handle = self.session.handle();
        let size = self.session.config().size;
        let output = self.bitstream.handle();
        let version = versions::struct_version_ex_for(api.api_version(), versions::num::PIC_PARAMS);
        let timestamp = self.frame_index;
        let force_idr = std::mem::take(&mut self.force_keyframe);

        let encode = api
            .functions()
            .nvEncEncodePicture
            .ok_or_else(|| CodecError::Unavailable("nvEncEncodePicture отсутствует".into()))?;

        // SAFETY: непустоту проверили выше; валидность — контракт
        // `EncoderInput::texture`.
        unsafe { self.ensure_registered(frame.texture) }?;
        let resource = self
            .registered
            .get(&(frame.texture as usize))
            .expect("регистрация обеспечена выше");
        let mapped = resource.map()?;

        let mut pic = sys::NV_ENC_PIC_PARAMS {
            version,
            inputWidth: size.width,
            inputHeight: size.height,
            inputPitch: size.width,
            inputBuffer: mapped.input_ptr(),
            outputBitstream: output,
            bufferFmt: mapped.buffer_format(),
            pictureStruct: sys::_NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            inputTimeStamp: timestamp,
            encodePicFlags: if force_idr {
                sys::_NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
            } else {
                0
            },
            ..Default::default()
        };

        // SAFETY: сессия жива; `pic` заполнена требуемыми полями и
        // ссылается на отображённый ресурс и живой буфер битстрима,
        // оба переживают вызов.
        let status = unsafe { encode(handle, &mut pic) };

        // NEED_MORE_INPUT — не ошибка: энкодер накапливает кадры.
        // В нашей конфигурации (без B-кадров) встречаться не должно,
        // но обрабатывается явно, чтобы не выглядеть сбоем.
        if status == sys::_NVENCSTATUS::NV_ENC_ERR_NEED_MORE_INPUT {
            self.frame_index += 1;
            return Ok(None);
        }
        check_status(status, "nvEncEncodePicture")?;

        let locked = self.bitstream.read()?;
        // Отображение снимается здесь: буфер уже прочитан, держать
        // ресурс занятым дольше незачем.
        drop(mapped);

        self.frame_index += 1;

        let mut info = frame.info;
        info.timings.mark(Stage::Encoded, self.epoch.stamp_now());

        Ok(Some(EncodedFrame {
            kind: if locked.is_keyframe() {
                FrameKind::Key
            } else {
                FrameKind::Delta
            },
            data: locked.data,
            info,
        }))
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        let api = self.session.api();
        let reconfigure = api
            .functions()
            .nvEncReconfigureEncoder
            .ok_or_else(|| CodecError::Unavailable("nvEncReconfigureEncoder отсутствует".into()))?;

        // Пересоздавать сессию ради смены битрейта нельзя: это дало бы
        // разрыв картинки. Реконфигурация меняет параметры на лету —
        // именно то, что нужно адаптации к каналу (этап 6).
        // `config` обязана пережить вызов: `reInitEncodeParams` держит
        // на неё указатель. Обе переменные живут до конца функции.
        let mut config = self.session.build_config(RateControl::Cbr { bitrate })?;
        let init = self.session.build_init_params(&mut config);

        let mut params = sys::NV_ENC_RECONFIGURE_PARAMS {
            version: versions::struct_version_ex_for(
                api.api_version(),
                versions::num::RECONFIGURE_PARAMS,
            ),
            reInitEncodeParams: init,
            ..Default::default()
        };

        // SAFETY: сессия жива; `params` заполнена требуемыми полями и
        // ссылается на `config`, которая живёт до конца функции —
        // то есть переживает вызов.
        let status = unsafe { reconfigure(self.session.handle(), &mut params) };
        check_status(status, "nvEncReconfigureEncoder")?;

        self.session.set_rate_control(RateControl::Cbr { bitrate });
        tracing::info!(bitrate, "битрейт изменён");
        Ok(())
    }

    fn config(&self) -> &EncoderConfig {
        self.session.config()
    }
}

// SAFETY: все ресурсы внутри принадлежат одной сессии NVENC, которую
// можно перемещать между потоками, но не использовать из нескольких
// одновременно. `&mut self` в методах `Encoder` это и обеспечивает.
unsafe impl Send for NvencEncoder {}
