//! Аппаратный декодер H.264 через Media Foundation Transform + D3D11VA.
//!
//! # Почему MFT, а не DXVA2 напрямую
//!
//! DXVA2 требует разбирать поток самим: доставать SPS/PPS, заполнять
//! структуры срезов, управлять очередью опорных кадров. Это сотни строк
//! `unsafe` на недоверенных данных — ровно тот класс кода, который
//! CLAUDE.md §8.5 велит минимизировать. MFT берёт разбор на себя,
//! оставляя нам границу «байты внутрь — текстура наружу».
//!
//! # Что сделано ради задержки
//!
//! - `MF_LOW_LATENCY` — декодер не копит кадры для переупорядочивания.
//!   При потоке без B-кадров переупорядочивать нечего, а очередь
//!   стоила бы 2–3 кадра задержки;
//! - `MFT_MESSAGE_SET_D3D_MANAGER` — выход остаётся в GPU (§4.2.3);
//! - синхронный режим: асинхронный MFT добавил бы поток событий и
//!   его планирование между декодом и рендером.

use super::runtime::{DxgiDeviceManager, MediaFoundation};
use crate::{CodecError, Decoder, DecoderConfig, EncodedFrame, Result};
use bd_core::frame::{FrameInfo, FrameSize, PixelFormat};
use bd_core::metrics::Stage;
use bd_core::time::Epoch;
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_DECODER, D3D11_BIND_SHADER_RESOURCE,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, IMFDXGIBuffer, IMFMediaType, IMFSample, IMFTransform,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_MESSAGE_TYPE,
    MFT_OUTPUT_DATA_BUFFER, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SA_D3D11_AWARE, MF_SA_D3D11_BINDFLAGS,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

/// Прогрессивная развёртка — значение `MFVideoInterlace_Progressive`
/// перечисления `MFVideoInterlaceMode`.
const INTERLACE_PROGRESSIVE: u32 = 2;

/// Единица времени Media Foundation — 100 наносекунд.
const HNS_PER_SEC: i64 = 10_000_000;

/// Декодированный кадр: NV12-текстура в GPU и метаданные.
///
/// Текстура принадлежит пулу декодера и действительна, пока жив
/// удерживаемый здесь sample. Копии в системную память нет — это
/// прямое требование бюджета задержки (CLAUDE.md §4.2.3).
///
/// # Порядок полей важен
///
/// `texture` — представление данных, которыми владеет `_sample`.
/// Поэтому текстура объявлена первой: она умирает раньше, чем
/// освобождается сам буфер.
pub struct DecodedFrame {
    texture: ID3D11Texture2D,
    /// Индекс подресурса: MFT отдаёт кадры как срезы массива текстур,
    /// а не отдельными текстурами. Рендер обязан это учитывать, иначе
    /// покажет всегда нулевой слой.
    subresource: u32,
    info: FrameInfo,
    /// Держит буфер живым. Не читается — отсюда подчёркивание.
    _sample: IMFSample,
}

impl DecodedFrame {
    /// NV12-текстура кадра.
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    /// Индекс подресурса внутри текстуры-массива.
    pub fn subresource(&self) -> u32 {
        self.subresource
    }

    /// Метаданные и тайминги.
    pub fn info(&self) -> &FrameInfo {
        &self.info
    }

    /// Изменяемая ссылка на метаданные — чтобы рендер проставил свою
    /// отметку времени.
    pub fn info_mut(&mut self) -> &mut FrameInfo {
        &mut self.info
    }
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("size", &self.info.size)
            .field("subresource", &self.subresource)
            .finish_non_exhaustive()
    }
}

/// Аппаратный декодер H.264 на D3D11VA.
///
/// # Порядок полей важен
///
/// Rust уничтожает поля сверху вниз. MFT обращается к менеджеру
/// устройств при разрушении, а Media Foundation должна быть жива, пока
/// жив хоть один её объект. Отсюда порядок: `transform` → `manager`
/// → `_mf`. Ошибка здесь тихая: она проявится не падением, а
/// `MF_E_SHUTDOWN` в логе при выходе — то же, что было с NVENC
/// (CLAUDE.md §0.1, находка 16).
pub struct D3d11Decoder {
    transform: IMFTransform,
    manager: DxgiDeviceManager,
    _mf: MediaFoundation,
    config: DecoderConfig,
    epoch: Epoch,
    /// Фактический размер кадра. Может отличаться от заявленного в
    /// настройках: поток сообщает своё разрешение в SPS, и оно
    /// побеждает.
    size: FrameSize,
    /// Номер кадра, он же временная метка для MFT.
    frame_index: i64,
}

impl D3d11Decoder {
    /// Создать декодер на заданном D3D11-устройстве.
    ///
    /// Устройство должно быть тем же, на котором работает рендер:
    /// иначе текстуру пришлось бы копировать между устройствами и
    /// zero-copy потерялся бы.
    pub fn new(device: &ID3D11Device, config: DecoderConfig, epoch: Epoch) -> Result<Self> {
        if config.codec != crate::Codec::H264 {
            return Err(CodecError::UnsupportedParams(format!(
                "аппаратный декодер реализован только для H.264, запрошен {:?}",
                config.codec
            )));
        }

        let mf = MediaFoundation::startup()?;
        let manager = DxgiDeviceManager::new(device)?;

        // SAFETY: CLSID — константа; запрошенный интерфейс совпадает
        // с параметром типа, внешних указателей вызов не принимает.
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| {
                    CodecError::DecoderUnavailable(format!("H.264 MFT недоступен: {e}"))
                })?;

        let size = config.size;
        let mut decoder = Self {
            transform,
            manager,
            _mf: mf,
            config,
            epoch,
            size,
            frame_index: 0,
        };

        decoder.configure()?;
        tracing::info!(
            width = size.width,
            height = size.height,
            "аппаратный декодер H.264 (D3D11VA) готов"
        );
        Ok(decoder)
    }

    /// Фактическое разрешение потока.
    pub fn size(&self) -> FrameSize {
        self.size
    }

    /// Настроить MFT: D3D11, низкая задержка, типы входа и выхода.
    fn configure(&mut self) -> Result<()> {
        self.require_d3d11_aware()?;
        self.enable_low_latency();
        self.attach_d3d_manager()?;
        self.set_input_type()?;
        self.select_output_type()?;
        self.request_shader_bindable_output();

        // Обе нотификации обязательны перед первым ProcessInput: без
        // них MFT не выделит пул поверхностей и вернёт ошибку на
        // первом же кадре. Порядок важен: пул выделяется здесь, и
        // флаги привязки после этого уже не изменить.
        self.send_message(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        self.send_message(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        Ok(())
    }

    /// Попросить MFT выделить поверхности, пригодные для шейдера.
    ///
    /// По умолчанию декодер создаёт текстуры только с
    /// `D3D11_BIND_DECODER`. Такую поверхность нельзя привязать к
    /// шейдеру: `CreateShaderResourceView` возвращает `E_INVALIDARG`,
    /// и рендеру пришлось бы копировать кадр в свою текстуру — лишний
    /// проход по 3 МБ на каждом кадре.
    ///
    /// Атрибут запрашивает у пула привязку `SHADER_RESOURCE` вдобавок
    /// к `DECODER`, и конверсия NV12→RGB читает поверхность декодера
    /// напрямую (CLAUDE.md §4.2.3).
    ///
    /// Не критично: если MFT атрибут не примет, декод продолжит
    /// работать — просто рендеру придётся копировать. Поэтому неудача
    /// пишется в лог, а не роняет создание декодера.
    fn request_shader_bindable_output(&self) {
        // SAFETY: MFT жив; поток выхода нулевой.
        let attrs = match unsafe { self.transform.GetOutputStreamAttributes(0) } {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "атрибуты выходного потока недоступны");
                return;
            }
        };

        // BIND_DECODER обязателен: без него декодер не сможет писать
        // в собственные поверхности.
        let flags = (D3D11_BIND_DECODER.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;

        // SAFETY: `attrs` жив; GUID — константа, значение по значению.
        if let Err(e) = unsafe { attrs.SetUINT32(&MF_SA_D3D11_BINDFLAGS, flags) } {
            tracing::warn!(
                error = %e,
                "MF_SA_D3D11_BINDFLAGS не принят — рендеру потребуется копия кадра"
            );
            return;
        }

        tracing::debug!(
            flags = format!("{flags:#x}"),
            "запрошены поверхности, пригодные для шейдера"
        );
    }

    /// Убедиться, что MFT умеет отдавать D3D11-поверхности.
    ///
    /// Проверка не формальность: если MFT не D3D11-aware, декод
    /// произойдёт в системной памяти молча — без ошибок, просто
    /// медленно. Такой отказ нужно поймать здесь, а не гадать потом,
    /// откуда взялись лишние миллисекунды.
    fn require_d3d11_aware(&self) -> Result<()> {
        // SAFETY: MFT только что создан и жив.
        let attrs = unsafe { self.transform.GetAttributes() }
            .map_err(|e| CodecError::DecoderUnavailable(format!("атрибуты MFT недоступны: {e}")))?;

        // SAFETY: `attrs` жив; GUID — константа. Отсутствие атрибута —
        // ошибка вызова, а не UB, поэтому трактуется как «не умеет».
        let aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0);
        if aware == 0 {
            return Err(CodecError::DecoderUnavailable(
                "H.264 MFT не поддерживает D3D11 — аппаратного декода нет".into(),
            ));
        }
        Ok(())
    }

    /// Включить режим низкой задержки.
    ///
    /// Не критичен: декодер, который атрибут игнорирует, поток всё
    /// равно раскодирует. Поэтому неудача пишется в лог, а не роняет
    /// создание декодера.
    fn enable_low_latency(&self) {
        // SAFETY: MFT жив.
        let attrs = match unsafe { self.transform.GetAttributes() } {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "не удалось получить атрибуты MFT");
                return;
            }
        };

        // SAFETY: `attrs` жив, GUID — константа, значение по значению.
        if let Err(e) = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) } {
            tracing::warn!(error = %e, "MF_LOW_LATENCY не принят — возможна лишняя задержка");
        }
    }

    /// Передать MFT менеджер устройств DXGI.
    fn attach_d3d_manager(&self) -> Result<()> {
        self.send_message(MFT_MESSAGE_SET_D3D_MANAGER, self.manager.as_ulong_ptr())
            .map_err(|e| {
                // Отказ здесь означает, что аппаратного пути нет.
                // Продолжать в системной памяти бессмысленно: это
                // молчаливая потеря 5–10 мс на кадр.
                tracing::warn!(error = %e, "MFT отверг DXGI-менеджер");
                CodecError::DecoderUnavailable(
                    "MFT не принял D3D11-устройство: аппаратный декод недоступен".into(),
                )
            })
    }

    /// Задать входной тип: H.264, наше разрешение, прогрессивная развёртка.
    fn set_input_type(&self) -> Result<()> {
        let media_type = create_media_type(&MFVideoFormat_H264)?;

        set_size(&media_type, self.size)?;
        // SAFETY: тип только что создан; GUID и значение — константы.
        unsafe { media_type.SetUINT32(&MF_MT_INTERLACE_MODE, INTERLACE_PROGRESSIVE) }
            .map_err(|e| decode_err("MF_MT_INTERLACE_MODE", e))?;

        // SAFETY: MFT жив; поток входа у декодера всегда нулевой —
        // GetStreamCount у H.264 MFT возвращает 1 вход и 1 выход.
        unsafe { self.transform.SetInputType(0, &media_type, 0) }
            .map_err(|e| decode_err("SetInputType", e))?;
        Ok(())
    }

    /// Выбрать NV12 среди предлагаемых MFT выходных типов.
    ///
    /// Перебор, а не конструирование своего типа: MFT принимает только
    /// те типы, что сам предложил, и различия в скрытых полях
    /// (шаг строки, диапазон яркости) приводят к отказу.
    fn select_output_type(&self) -> Result<()> {
        for index in 0.. {
            // SAFETY: MFT жив; исчерпание списка сигнализируется
            // ошибкой MF_E_NO_MORE_TYPES — это условие выхода.
            let candidate = match unsafe { self.transform.GetOutputAvailableType(0, index) } {
                Ok(t) => t,
                Err(_) => break,
            };

            // SAFETY: тип жив; GUID — константа.
            let subtype = match unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) } {
                Ok(g) => g,
                Err(_) => continue,
            };

            if subtype != MFVideoFormat_NV12 {
                continue;
            }

            // SAFETY: MFT жив, тип получен от него же.
            unsafe { self.transform.SetOutputType(0, &candidate, 0) }
                .map_err(|e| decode_err("SetOutputType", e))?;
            return Ok(());
        }

        Err(CodecError::DecoderUnavailable(
            "MFT не предложил NV12 на выходе".into(),
        ))
    }

    /// Обработать смену выходного типа.
    ///
    /// MFT сообщает об этом, когда из потока стало известно настоящее
    /// разрешение — например, хост переключил монитор. Это штатное
    /// событие, а не сбой: надо пересогласовать выходной тип и
    /// продолжить с того же места.
    fn renegotiate_output(&mut self) -> Result<()> {
        self.select_output_type()?;

        // SAFETY: MFT жив; тип только что установлен.
        if let Ok(current) = unsafe { self.transform.GetOutputCurrentType(0) } {
            if let Some(size) = read_size(&current) {
                if size != self.size {
                    tracing::info!(
                        from = format!("{}x{}", self.size.width, self.size.height),
                        to = format!("{}x{}", size.width, size.height),
                        "разрешение потока изменилось"
                    );
                    self.size = size;
                }
            }
        }
        Ok(())
    }

    /// Отправить сжатый кадр в MFT.
    fn feed(&mut self, frame: &EncodedFrame) -> Result<()> {
        let duration = HNS_PER_SEC / 60;
        let sample = build_sample(&frame.data, self.frame_index, duration)?;

        // SAFETY: MFT жив; поток входа нулевой; sample жив до конца
        // вызова, а MFT удерживает его сам, если он ему ещё нужен.
        unsafe { self.transform.ProcessInput(0, &sample, 0) }
            .map_err(|e| decode_err("ProcessInput", e))?;

        self.frame_index += 1;
        Ok(())
    }

    /// Забрать декодированный кадр, если он готов.
    fn drain(&mut self, info: FrameInfo) -> Result<Option<DecodedFrame>> {
        // Поверхность выделяет сам MFT (у аппаратных декодеров стоит
        // флаг MFT_OUTPUT_STREAM_PROVIDES_SAMPLES), поэтому pSample
        // оставляется пустым.
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;

        // SAFETY: MFT жив; массив буферов и `status` — живые локальные
        // переменные, которые функция заполняет.
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };

        // Забрать значения нужно в любом случае: при успехе владение
        // переходит к нам, и утечка COM-ссылки была бы тихой. Поля —
        // ManuallyDrop, то есть сами не освободятся.
        // SAFETY: после этого вызова поля больше не читаются — массив
        // `buffers` дальше по коду не используется.
        let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pSample) };
        // SAFETY: то же самое; список событий нам не нужен, но
        // освободить его обязаны.
        let _events = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pEvents) };

        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                // Штатно до первого ключевого кадра и когда кадр
                // распределён между несколькими вызовами.
                return Ok(None);
            }
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.renegotiate_output()?;
                return Ok(None);
            }
            Err(e) => return Err(decode_err("ProcessOutput", e)),
        }

        let Some(sample) = sample else {
            return Ok(None);
        };

        let (texture, subresource) = texture_from_sample(&sample)?;

        let mut info = info;
        info.size = self.size;
        info.format = PixelFormat::Nv12;
        info.timings.mark(Stage::Decoded, self.epoch.stamp_now());

        Ok(Some(DecodedFrame {
            texture,
            subresource,
            info,
            _sample: sample,
        }))
    }

    /// Отправить MFT управляющее сообщение.
    fn send_message(&self, message: MFT_MESSAGE_TYPE, param: usize) -> Result<()> {
        // SAFETY: MFT жив; `param` для SET_D3D_MANAGER — указатель на
        // менеджер, который переживает MFT (см. порядок полей).
        unsafe { self.transform.ProcessMessage(message, param) }
            .map_err(|e| decode_err("ProcessMessage", e))
    }
}

impl Decoder for D3d11Decoder {
    type Output = DecodedFrame;

    fn decode(&mut self, frame: &EncodedFrame) -> Result<Option<DecodedFrame>> {
        if frame.data.is_empty() {
            return Err(CodecError::CorruptStream("пустой кадр".into()));
        }

        self.feed(frame)?;
        self.drain(frame.info)
    }

    fn flush(&mut self) -> Result<()> {
        self.send_message(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
        // START_OF_STREAM после сброса обязателен: без него MFT
        // считает поток завершённым и молча перестаёт отдавать кадры.
        self.send_message(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        tracing::debug!("декодер сброшен");
        Ok(())
    }

    fn config(&self) -> &DecoderConfig {
        &self.config
    }
}

// SAFETY: все объекты MF внутри принадлежат одному декодеру и
// используются только через `&mut self`, то есть без одновременного
// доступа из нескольких потоков. Перемещение между потоками MF
// допускает: её объекты free-threaded.
unsafe impl Send for D3d11Decoder {}

/// Достать D3D11-текстуру из sample, не копируя пиксели.
fn texture_from_sample(sample: &IMFSample) -> Result<(ID3D11Texture2D, u32)> {
    // SAFETY: sample получен от MFT и жив.
    let buffer =
        unsafe { sample.GetBufferByIndex(0) }.map_err(|e| decode_err("GetBufferByIndex", e))?;

    let dxgi: IMFDXGIBuffer = buffer.cast().map_err(|_| {
        // Буфер в системной памяти означает, что аппаратный путь
        // отвалился на ходу — молча продолжать нельзя.
        CodecError::DecoderUnavailable(
            "декодер вернул кадр в системной памяти вместо GPU-текстуры".into(),
        )
    })?;

    let mut raw = std::ptr::null_mut();
    // SAFETY: `dxgi` жив; riid соответствует типу, в который результат
    // оборачивается ниже; `raw` — живая локальная переменная.
    unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }
        .map_err(|e| decode_err("IMFDXGIBuffer::GetResource", e))?;

    if raw.is_null() {
        return Err(CodecError::Decode {
            context: "GetResource вернул пустой указатель",
            hresult: 0,
        });
    }

    // SAFETY: GetResource вернул интерфейс с уже увеличенным счётчиком
    // ссылок, запрошенный по IID ID3D11Texture2D. `from_raw` забирает
    // это владение — дополнительный AddRef был бы утечкой.
    let texture: ID3D11Texture2D = unsafe { ID3D11Texture2D::from_raw(raw) };

    // SAFETY: `dxgi` жив. Отсутствие индекса означает нулевой срез.
    let subresource = unsafe { dxgi.GetSubresourceIndex() }.unwrap_or(0);

    Ok((texture, subresource))
}

/// Собрать MF-sample из сжатых байтов.
fn build_sample(data: &[u8], index: i64, duration: i64) -> Result<IMFSample> {
    let len: u32 = data
        .len()
        .try_into()
        .map_err(|_| CodecError::CorruptStream("кадр больше 4 ГиБ".into()))?;

    // SAFETY: функция не принимает входных указателей.
    let sample = unsafe { MFCreateSample() }.map_err(|e| decode_err("MFCreateSample", e))?;
    // SAFETY: то же самое.
    let buffer =
        unsafe { MFCreateMemoryBuffer(len) }.map_err(|e| decode_err("MFCreateMemoryBuffer", e))?;

    let mut dst: *mut u8 = std::ptr::null_mut();
    let mut max_len = 0u32;
    // SAFETY: буфер только что создан; выходные указатели ссылаются на
    // живые локальные переменные. Текущая длина не нужна — None.
    unsafe { buffer.Lock(&mut dst, Some(&mut max_len), None) }
        .map_err(|e| decode_err("IMFMediaBuffer::Lock", e))?;

    // Копия здесь неизбежна и допустима: это сжатые байты (десятки
    // килобайт), а не кадр. Запрет §4.2.3 касается пикселей.
    if dst.is_null() || max_len < len {
        // Разблокировать обязаны даже на пути ошибки, иначе буфер
        // останется занятым навсегда.
        // SAFETY: парный Unlock к успешному Lock выше.
        let _ = unsafe { buffer.Unlock() };
        return Err(CodecError::Decode {
            context: "буфер MF меньше запрошенного",
            hresult: 0,
        });
    }

    // SAFETY: `dst` не пуст и вмещает `len` байт (проверено выше);
    // области не пересекаются — буфер только что выделен MF.
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };

    // SAFETY: парный Unlock к Lock выше.
    unsafe { buffer.Unlock() }.map_err(|e| decode_err("IMFMediaBuffer::Unlock", e))?;
    // SAFETY: буфер разблокирован, длина не превышает выделенную.
    unsafe { buffer.SetCurrentLength(len) }.map_err(|e| decode_err("SetCurrentLength", e))?;

    // SAFETY: sample и буфер живы.
    unsafe { sample.AddBuffer(&buffer) }.map_err(|e| decode_err("AddBuffer", e))?;

    // Метки времени условные: настоящие тайминги едут в FrameInfo,
    // а MFT нужен лишь монотонный порядок.
    // SAFETY: sample жив; аргументы передаются по значению.
    unsafe { sample.SetSampleTime(index * duration) }
        .map_err(|e| decode_err("SetSampleTime", e))?;
    // SAFETY: то же самое.
    unsafe { sample.SetSampleDuration(duration) }
        .map_err(|e| decode_err("SetSampleDuration", e))?;

    Ok(sample)
}

/// Создать медиатип «видео заданного подтипа».
fn create_media_type(subtype: &GUID) -> Result<IMFMediaType> {
    // SAFETY: функция не принимает входных указателей.
    let media_type =
        unsafe { MFCreateMediaType() }.map_err(|e| decode_err("MFCreateMediaType", e))?;

    // SAFETY: тип только что создан; GUID живут дольше вызова.
    unsafe { media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }
        .map_err(|e| decode_err("MF_MT_MAJOR_TYPE", e))?;
    // SAFETY: то же самое.
    unsafe { media_type.SetGUID(&MF_MT_SUBTYPE, subtype) }
        .map_err(|e| decode_err("MF_MT_SUBTYPE", e))?;

    Ok(media_type)
}

/// Записать разрешение в медиатип.
///
/// MF пакует пару 32-битных чисел в один 64-битный атрибут: старшие
/// биты — ширина. Ошибка в порядке даёт не отказ, а перевёрнутую
/// картинку, поэтому упаковка вынесена в одно место вместе с [`read_size`].
fn set_size(media_type: &IMFMediaType, size: FrameSize) -> Result<()> {
    // SAFETY: тип жив; GUID — константа; значение передаётся по значению.
    unsafe { media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(size)) }
        .map_err(|e| decode_err("MF_MT_FRAME_SIZE", e))
}

/// Прочитать разрешение из медиатипа.
fn read_size(media_type: &IMFMediaType) -> Option<FrameSize> {
    // SAFETY: тип жив; GUID — константа.
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
    let size = unpack_size(packed);
    (!size.is_empty()).then_some(size)
}

/// Упаковать разрешение в формат атрибутов MF.
fn pack_size(size: FrameSize) -> u64 {
    ((size.width as u64) << 32) | size.height as u64
}

/// Распаковать разрешение из формата атрибутов MF.
fn unpack_size(packed: u64) -> FrameSize {
    FrameSize::new((packed >> 32) as u32, packed as u32)
}

/// Превратить ошибку Windows в ошибку кодека.
fn decode_err(context: &'static str, error: windows::core::Error) -> CodecError {
    CodecError::Decode {
        context,
        hresult: error.code().0 as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_survives_pack_roundtrip() {
        // Порядок половин перепутать легко, а проявится это
        // перевёрнутой картинкой, а не ошибкой вызова.
        let size = FrameSize::new(1920, 1080);
        assert_eq!(unpack_size(pack_size(size)), size);
    }

    #[test]
    fn packed_size_puts_width_in_high_bits() {
        assert_eq!(pack_size(FrameSize::new(1920, 1080)) >> 32, 1920);
        assert_eq!(pack_size(FrameSize::new(1920, 1080)) as u32, 1080);
    }

    #[test]
    fn non_square_size_survives() {
        // Второй монитор разработчика — 1707x1067 (CLAUDE.md §0.1).
        let size = FrameSize::new(1707, 1067);
        assert_eq!(unpack_size(pack_size(size)), size);
    }
}
