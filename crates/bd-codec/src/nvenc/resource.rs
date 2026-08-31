//! Регистрация GPU-текстур и буфер битстрима.
//!
//! # Zero-copy — суть модуля
//!
//! D3D11-текстура из захвата отдаётся энкодеру **напрямую**, через
//! `nvEncRegisterResource`. Кадр не проходит через системную память:
//! копия GPU→CPU→GPU стоила бы 5–10 мс и съела треть бюджета задержки
//! (CLAUDE.md §4.2.3).
//!
//! # Два уровня владения
//!
//! У NVENC ресурс живёт в двух состояниях, и путать их нельзя:
//!
//! - **registered** — текстура известна энкодеру. Регистрация дорогая,
//!   поэтому делается один раз на текстуру ([`RegisteredResource`]).
//! - **mapped** — ресурс отдан энкодеру на время одного кадра. Дешёвая
//!   операция, обязана быть парной ([`MappedResource`]).
//!
//! Оба состояния — RAII-типы: `Drop` снимает регистрацию и отображение
//! (CLAUDE.md §4.3.4). Забыть освободить невозможно.

use super::api::{check_status, NvencApi};
use super::{sys, versions};
use crate::{CodecError, Result};
use std::ffi::c_void;
use std::ptr;

/// Текстура, зарегистрированная в NVENC.
///
/// Регистрация не бесплатна, поэтому объект создаётся один раз на
/// текстуру и переиспользуется между кадрами.
pub struct RegisteredResource {
    handle: sys::NV_ENC_REGISTERED_PTR,
    encoder: *mut c_void,
    api: &'static NvencApi,
}

impl RegisteredResource {
    /// Зарегистрировать D3D11-текстуру в формате BGRA8.
    ///
    /// # Safety
    ///
    /// - `encoder` — валидный хендл открытой сессии NVENC;
    /// - `texture` — валидный указатель на `ID3D11Texture2D`, который
    ///   переживёт возвращённый объект.
    pub unsafe fn register_d3d11(
        api: &'static NvencApi,
        encoder: *mut c_void,
        texture: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let register = api
            .functions()
            .nvEncRegisterResource
            .ok_or_else(|| CodecError::Unavailable("nvEncRegisterResource отсутствует".into()))?;

        let mut params = sys::NV_ENC_REGISTER_RESOURCE {
            version: versions::struct_version_for(
                api.api_version(),
                versions::num::REGISTER_RESOURCE,
            ),
            resourceType: sys::_NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width,
            height,
            // Для D3D11-текстур шаг строки знает драйвер; ноль означает
            // «взять из описания ресурса».
            pitch: 0,
            resourceToRegister: texture,
            // DXGI Desktop Duplication отдаёт B8G8R8A8. В терминах NVENC
            // это ARGB: формат назван по порядку слов, а не байтов.
            bufferFormat: sys::_NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            bufferUsage: sys::_NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };

        // SAFETY: `encoder` валиден по контракту функции; `params`
        // заполнена требуемыми полями, включая version; `texture`
        // валиден и переживёт регистрацию.
        let status = unsafe { register(encoder, &mut params) };
        check_status(status, "nvEncRegisterResource")?;

        if params.registeredResource.is_null() {
            return Err(CodecError::InitFailed(
                "nvEncRegisterResource вернул пустой хендл".into(),
            ));
        }

        Ok(Self {
            handle: params.registeredResource,
            encoder,
            api,
        })
    }

    /// Отобразить ресурс для кодирования одного кадра.
    pub fn map(&self) -> Result<MappedResource<'_>> {
        let map_fn =
            self.api.functions().nvEncMapInputResource.ok_or_else(|| {
                CodecError::Unavailable("nvEncMapInputResource отсутствует".into())
            })?;

        let mut params = sys::NV_ENC_MAP_INPUT_RESOURCE {
            version: versions::struct_version_for(
                self.api.api_version(),
                versions::num::MAP_INPUT_RESOURCE,
            ),
            registeredResource: self.handle,
            ..Default::default()
        };

        // SAFETY: `encoder` и `handle` валидны, пока жив `self`;
        // `params` заполнена требуемыми полями.
        let status = unsafe { map_fn(self.encoder, &mut params) };
        check_status(status, "nvEncMapInputResource")?;

        Ok(MappedResource {
            input: params.mappedResource,
            buffer_format: params.mappedBufferFmt,
            owner: self,
        })
    }
}

impl Drop for RegisteredResource {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        let Some(unregister) = self.api.functions().nvEncUnregisterResource else {
            tracing::error!("nvEncUnregisterResource отсутствует, ресурс не освобождён");
            return;
        };
        // SAFETY: `handle` получен из nvEncRegisterResource и не был
        // освобождён ранее — Drop вызывается ровно один раз.
        let status = unsafe { unregister(self.encoder, self.handle) };
        if status != sys::_NVENCSTATUS::NV_ENC_SUCCESS {
            tracing::warn!(status, "nvEncUnregisterResource вернул ошибку");
        }
        self.handle = ptr::null_mut();
    }
}

/// Ресурс, отображённый для кодирования одного кадра.
///
/// Время жизни привязано к [`RegisteredResource`]: отображение нельзя
/// пережить регистрацию, и компилятор это гарантирует.
pub struct MappedResource<'a> {
    input: sys::NV_ENC_INPUT_PTR,
    buffer_format: sys::NV_ENC_BUFFER_FORMAT,
    owner: &'a RegisteredResource,
}

impl MappedResource<'_> {
    /// Указатель на входной буфер для `nvEncEncodePicture`.
    pub(crate) fn input_ptr(&self) -> sys::NV_ENC_INPUT_PTR {
        self.input
    }

    /// Формат, в котором энкодер видит буфер.
    pub(crate) fn buffer_format(&self) -> sys::NV_ENC_BUFFER_FORMAT {
        self.buffer_format
    }
}

impl Drop for MappedResource<'_> {
    fn drop(&mut self) {
        if self.input.is_null() {
            return;
        }
        let Some(unmap) = self.owner.api.functions().nvEncUnmapInputResource else {
            tracing::error!("nvEncUnmapInputResource отсутствует");
            return;
        };
        // SAFETY: `input` получен из nvEncMapInputResource, сессия жива
        // (гарантировано временем жизни `owner`), освобождение однократно.
        let status = unsafe { unmap(self.owner.encoder, self.input) };
        if status != sys::_NVENCSTATUS::NV_ENC_SUCCESS {
            tracing::warn!(status, "nvEncUnmapInputResource вернул ошибку");
        }
        self.input = ptr::null_mut();
    }
}

/// Выходной буфер для сжатого битстрима.
pub struct BitstreamBuffer {
    handle: sys::NV_ENC_OUTPUT_PTR,
    encoder: *mut c_void,
    api: &'static NvencApi,
}

impl BitstreamBuffer {
    /// Создать буфер битстрима.
    ///
    /// # Safety
    ///
    /// `encoder` — валидный хендл открытой сессии NVENC, переживающий
    /// возвращённый буфер.
    pub unsafe fn new(api: &'static NvencApi, encoder: *mut c_void) -> Result<Self> {
        let create = api.functions().nvEncCreateBitstreamBuffer.ok_or_else(|| {
            CodecError::Unavailable("nvEncCreateBitstreamBuffer отсутствует".into())
        })?;

        let mut params = sys::NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: versions::struct_version_for(
                api.api_version(),
                versions::num::CREATE_BITSTREAM_BUFFER,
            ),
            ..Default::default()
        };

        // SAFETY: `encoder` валиден по контракту; `params` заполнена
        // требуемым полем version.
        let status = unsafe { create(encoder, &mut params) };
        check_status(status, "nvEncCreateBitstreamBuffer")?;

        if params.bitstreamBuffer.is_null() {
            return Err(CodecError::InitFailed(
                "nvEncCreateBitstreamBuffer вернул пустой хендл".into(),
            ));
        }

        Ok(Self {
            handle: params.bitstreamBuffer,
            encoder,
            api,
        })
    }

    /// Хендл буфера для `nvEncEncodePicture`.
    pub(crate) fn handle(&self) -> sys::NV_ENC_OUTPUT_PTR {
        self.handle
    }

    /// Прочитать закодированные данные.
    ///
    /// Возвращает копию в `Vec`: NVENC требует разблокировать буфер
    /// сразу, иначе он не сможет писать в него следующий кадр. Это
    /// копия **сжатых** данных (десятки-сотни килобайт), а не кадра —
    /// правило zero-copy §4.2.3 к ней не относится.
    pub fn read(&self) -> Result<LockedBitstream> {
        let lock = self
            .api
            .functions()
            .nvEncLockBitstream
            .ok_or_else(|| CodecError::Unavailable("nvEncLockBitstream отсутствует".into()))?;

        let mut params = sys::NV_ENC_LOCK_BITSTREAM {
            version: versions::struct_version_for(
                self.api.api_version(),
                versions::num::LOCK_BITSTREAM,
            ),
            outputBitstream: self.handle,
            ..Default::default()
        };

        // SAFETY: `encoder` и `handle` валидны, пока жив `self`;
        // `params` заполнена требуемыми полями.
        let status = unsafe { lock(self.encoder, &mut params) };
        check_status(status, "nvEncLockBitstream")?;

        let size = params.bitstreamSizeInBytes as usize;
        let data = if size == 0 || params.bitstreamBufferPtr.is_null() {
            Vec::new()
        } else {
            // SAFETY: NVENC гарантирует, что bitstreamBufferPtr указывает
            // на bitstreamSizeInBytes читаемых байт, пока буфер заблокирован.
            unsafe {
                std::slice::from_raw_parts(params.bitstreamBufferPtr as *const u8, size).to_vec()
            }
        };

        let picture_type = params.pictureType;

        // Разблокировать нужно в любом случае, иначе энкодер не сможет
        // использовать буфер под следующий кадр.
        if let Some(unlock) = self.api.functions().nvEncUnlockBitstream {
            // SAFETY: буфер был заблокирован успешным nvEncLockBitstream.
            let status = unsafe { unlock(self.encoder, self.handle) };
            if status != sys::_NVENCSTATUS::NV_ENC_SUCCESS {
                tracing::warn!(status, "nvEncUnlockBitstream вернул ошибку");
            }
        }

        Ok(LockedBitstream { data, picture_type })
    }
}

impl Drop for BitstreamBuffer {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        let Some(destroy) = self.api.functions().nvEncDestroyBitstreamBuffer else {
            tracing::error!("nvEncDestroyBitstreamBuffer отсутствует");
            return;
        };
        // SAFETY: `handle` получен из nvEncCreateBitstreamBuffer и
        // освобождается ровно один раз.
        let status = unsafe { destroy(self.encoder, self.handle) };
        if status != sys::_NVENCSTATUS::NV_ENC_SUCCESS {
            tracing::warn!(status, "nvEncDestroyBitstreamBuffer вернул ошибку");
        }
        self.handle = ptr::null_mut();
    }
}

/// Прочитанные данные битстрима.
pub struct LockedBitstream {
    /// Сжатые данные в формате Annex B.
    pub data: Vec<u8>,
    /// Тип кадра по классификации NVENC.
    picture_type: sys::NV_ENC_PIC_TYPE,
}

impl LockedBitstream {
    /// Ключевой ли это кадр.
    ///
    /// IDR и I декодируются самостоятельно; P и B требуют предыдущих.
    pub fn is_keyframe(&self) -> bool {
        use sys::_NV_ENC_PIC_TYPE as T;
        matches!(
            self.picture_type,
            T::NV_ENC_PIC_TYPE_IDR | T::NV_ENC_PIC_TYPE_I
        )
    }
}
