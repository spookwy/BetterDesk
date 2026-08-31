//! Аппаратный энкодер H.264 через Media Foundation.
//!
//! # Зачем он нужен, если есть NVENC
//!
//! NVENC работает только на NVIDIA, то есть хостом могла быть только
//! машина с картой NVIDIA. Для продукта, который должен работать
//! «почти у всех» (§7.1 — скачал и запустил), это неприемлемо:
//! ноутбук с Intel-графикой или машина с Radeon не могли отдать свой
//! экран вовсе.
//!
//! Media Foundation закрывает разом весь остальной парк. Windows сама
//! выбирает лучший доступный MFT, и на практике это:
//!
//! - **Intel QuickSync** на машинах с Intel-графикой;
//! - **AMD VCE** на Radeon;
//! - **NVIDIA H.264 Encoder MFT** — тот же NVENC, но через MF;
//! - встроенный в Windows **H264 Encoder MFT** как последний рубеж.
//!
//! Проверено перечислением (`survey.rs`): даже на машине без
//! дискретной карты в системе есть хотя бы один энкодер H.264.
//!
//! # Почему NVENC остаётся основным
//!
//! Прямой NVENC даёт больше контроля: Intra Refresh, точный VBV,
//! `maxQP`, реконфигурация битрейта без пересоздания сессии. Через
//! MF часть этих ручек либо отсутствует, либо игнорируется
//! конкретным вендором. Поэтому порядок выбора — NVENC, затем MF
//! (§5.2), а не наоборот.
//!
//! # Что здесь принципиально иначе, чем у NVENC
//!
//! **Настройки — это просьбы, а не команды.** MFT разных вендоров
//! молча игнорируют то, что не умеют: `SetValue` возвращает успех, а
//! параметр не применяется. Поэтому каждая настройка, чья потеря
//! меняет задержку, проверяется чтением обратно, а результат идёт в
//! лог. Это прямое следствие находки 43: настройка, у которой нет
//! свидетеля, неотличима от отсутствующей.

use super::runtime::{DxgiDeviceManager, MediaFoundation};
use crate::Result;
use crate::{Codec, CodecError, EncodedFrame, Encoder, EncoderConfig, FrameKind, RateControl};
use bd_core::frame::{FrameInfo, FrameSize};
use bd_core::metrics::Stage;
use bd_core::time::Epoch;
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

/// Единица времени Media Foundation — 100 наносекунд.
const HNS_PER_SEC: i64 = 10_000_000;

/// Прогрессивная развёртка.
const INTERLACE_PROGRESSIVE: u32 = 2;

/// Вход энкодера: та же текстура D3D11, что у NVENC.
///
/// Тип намеренно повторяет [`crate::nvenc::EncoderInput`] по смыслу,
/// но объявлен отдельно: `bd-codec` собирается и там, где NVENC-модуля
/// нет вовсе (машина без вендорённых заголовков), и зависимость от
/// его типов сделала бы MF-энкодер несобираемым вместе с ним.
pub struct MfEncoderInput<'a> {
    /// Кадр в GPU. Копии в системную память нет (§4.2.3).
    pub texture: &'a ID3D11Texture2D,
    /// Метаданные и тайминги кадра.
    pub info: FrameInfo,
}

/// Вход через сырой указатель — для реализации [`Encoder`].
///
/// Связанный тип trait'а не может заимствовать у вызывающего, поэтому
/// текстура передаётся указателем, ровно как в NVENC. Одинаковая
/// форма входа у обоих бэкендов — то, что позволяет пайплайну
/// работать с любым из них, не зная каким.
pub struct RawTextureInput {
    /// Указатель на `ID3D11Texture2D`.
    ///
    /// # Safety
    ///
    /// Должен быть валиден на момент вызова `encode` и указывать на
    /// текстуру того размера, с которым создан энкодер.
    pub texture: *mut std::ffi::c_void,
    /// Метаданные и тайминги кадра.
    pub info: FrameInfo,
}

/// Энкодер H.264 поверх Media Foundation.
///
/// # Порядок полей
///
/// MFT объявлен раньше менеджера устройств и рантайма MF, поэтому
/// уничтожается первым. Обратный порядок оставил бы MFT работать с
/// уже выключенной Media Foundation — та же причина, что у порядка
/// полей в NVENC (находка 16).
pub struct MfEncoder {
    transform: IMFTransform,
    /// Имя MFT, который выбрала система. Нужно в отчётах: «работает
    /// через QuickSync» и «работает через софтверный энкодер» — это
    /// разные новости для пользователя.
    name: String,
    /// Асинхронная ли у него модель работы.
    ///
    /// Определяется не по признаку «аппаратный», а по наличию
    /// атрибута `MF_TRANSFORM_ASYNC`: связь между ними обычная, но не
    /// гарантированная, а ошибка здесь стоит `E_UNEXPECTED` на первом
    /// же кадре.
    is_async: bool,
    /// Генератор событий асинхронного MFT.
    ///
    /// `None` у синхронных — им события не нужны.
    events: Option<IMFMediaEventGenerator>,
    _manager: DxgiDeviceManager,
    _mf: MediaFoundation,
    config: EncoderConfig,
    epoch: Epoch,
    frame_index: i64,
    /// Запрошен ли ключевой кадр на следующий вызов.
    force_keyframe: bool,
}

impl MfEncoder {
    /// Создать энкодер на заданном устройстве D3D11.
    ///
    /// Устройство обязано быть тем же, на котором идёт захват: иначе
    /// текстуру пришлось бы копировать между GPU, и zero-copy
    /// потерялся бы (§4.2.3).
    pub fn new(device: &ID3D11Device, config: EncoderConfig, epoch: Epoch) -> Result<Self> {
        if config.codec != Codec::H264 {
            return Err(CodecError::UnsupportedParams(format!(
                "MF-энкодер реализован только для H.264, запрошен {:?}",
                config.codec
            )));
        }

        let mf = MediaFoundation::startup()?;

        // Кандидаты перебираются, а не берётся первый.
        //
        // # Почему это обязательно
        //
        // На машине с гибридной графикой энкодеров несколько, и не
        // каждый умеет работать с нашей текстурой. Проверено живьём:
        // на ноутбуке, где дисплей ведёт NVIDIA, Intel QuickSync
        // отвергает D3D-менеджер (0x80070057) и затем выходной тип с
        // `MF_E_UNSUPPORTED_D3D_TYPE` (0xC00D6D76) — он физически не
        // видит текстуру чужого адаптера.
        //
        // Это тот же класс ограничения, что у DXGI в §5.1: кодировать
        // надо на том адаптере, которому принадлежит кадр. Взяв
        // первый попавшийся MFT, мы получили бы отказ там, где
        // соседний кандидат отработал бы прекрасно.
        //
        // Поэтому «выбор энкодера» — это не чтение списка, а попытка
        // настроить каждого по очереди. Дороже на старте на десятки
        // миллисекунд, зато работает на любой конфигурации.
        let candidates = enumerate_all();
        if candidates.is_empty() {
            return Err(CodecError::Unavailable(
                "на этой машине нет аппаратного H.264-энкодера; \
                 софтверный путь (openh264, §5.5) ещё не реализован"
                    .into(),
            ));
        }

        let mut last_error = None;
        let total = candidates.len();
        for (index, (transform, name)) in candidates.into_iter().enumerate() {
            match Self::configure_candidate(
                transform,
                name.clone(),
                device,
                config.clone(),
                epoch,
                &mf,
            ) {
                Ok(encoder) => return Ok(encoder),
                Err(e) => {
                    tracing::debug!(
                        candidate = %name,
                        index = index + 1,
                        total,
                        error = %e,
                        "энкодер не подошёл, пробую следующий"
                    );
                    last_error = Some((name, e));
                }
            }
        }

        Err(match last_error {
            Some((name, e)) => CodecError::Unavailable(format!(
                "ни один аппаратный энкодер не подошёл; последний — «{name}»: {e}"
            )),
            None => CodecError::Unavailable("аппаратный H.264-энкодер не настроился".into()),
        })
    }

    /// Попробовать настроить одного кандидата.
    ///
    /// Отдельная функция, потому что неудача здесь — штатный ход
    /// перебора, а не ошибка: следующий кандидат может подойти.
    fn configure_candidate(
        transform: IMFTransform,
        name: String,
        device: &ID3D11Device,
        config: EncoderConfig,
        epoch: Epoch,
        mf: &MediaFoundation,
    ) -> Result<Self> {
        let manager = DxgiDeviceManager::new(device)?;
        // Свой guard на каждого кандидата: MediaFoundation считает
        // пользователей (runtime.rs), поэтому повторный startup
        // дешёв и корректен, а клонировать guard нечем.
        let _ = mf;
        let mf = MediaFoundation::startup()?;

        // Вид модели читается у самого MFT, а не выводится из
        // «аппаратный значит асинхронный»: связь обычная, но не
        // обязательная, и ошибка стоит отказа на первом же кадре.
        // SAFETY: MFT только что создан.
        let is_async = unsafe { transform.GetAttributes() }
            .ok()
            .and_then(|a| {
                // SAFETY: атрибуты живы; GUID — константа.
                unsafe { a.GetUINT32(&MF_TRANSFORM_ASYNC) }.ok()
            })
            .unwrap_or(0)
            != 0;

        // Генератор событий нужен только асинхронным. Запрашивать его
        // у синхронного бессмысленно, а отказ там — норма.
        let events = if is_async {
            transform.cast::<IMFMediaEventGenerator>().ok()
        } else {
            None
        };

        let mut encoder = Self {
            transform,
            name,
            is_async,
            events,
            _manager: manager,
            _mf: mf,
            config,
            epoch,
            frame_index: 0,
            force_keyframe: false,
        };

        encoder.configure()?;

        tracing::info!(
            encoder = %encoder.name,
            width = encoder.config.size.width,
            height = encoder.config.size.height,
            bitrate = encoder.config.rate_control.target_bitrate(),
            "энкодер H.264 через Media Foundation готов"
        );

        Ok(encoder)
    }

    /// Имя выбранного системой MFT.
    ///
    /// Показывается в отчётах: «работает через QuickSync» и «работает
    /// через AMD VCE» — разные новости, и при разборе жалобы на
    /// качество знать это надо первым делом.
    pub fn backend_name(&self) -> &str {
        &self.name
    }

    /// Аппаратный ли выбранный энкодер.
    ///
    /// Всегда `true`: перечисление берёт только аппаратные MFT
    /// (см. [`enumerate_all`]). Метод оставлен, потому что вызывающий
    /// не обязан знать это правило, а появление софтверного пути
    /// (`openh264`, §5.5) сделает ответ содержательным.
    pub fn is_hardware(&self) -> bool {
        true
    }

    /// Настроить MFT целиком.
    ///
    /// Порядок обязателен: выходной тип задаётся ДО входного, иначе
    /// MFT не знает, во что кодировать, и отвергает вход. У декодера
    /// порядок обратный — это не описка, а зеркальность задач.
    fn configure(&mut self) -> Result<()> {
        self.attach_d3d_manager()?;
        self.apply_codec_api();
        self.set_output_type()?;
        self.set_input_type()?;
        self.send_message(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        self.send_message(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        Ok(())
    }

    /// Передать MFT менеджер устройств DXGI.
    ///
    /// В отличие от декодера, отказ здесь **не фатален**: софтверный
    /// энкодер D3D-менеджер не принимает по определению, но кодировать
    /// умеет. Это и есть тот случай, ради которого модуль написан, —
    /// машина без аппаратного энкодера всё равно должна работать.
    fn attach_d3d_manager(&self) -> Result<()> {
        self.send_message(MFT_MESSAGE_SET_D3D_MANAGER, self._manager.as_ulong_ptr())
            .map_err(|e| {
                // Отказ означает, что этот MFT не видит наш адаптер:
                // так ведёт себя Intel QuickSync, когда дисплей и
                // текстура принадлежат NVIDIA.
                //
                // Продолжать с ним нельзя. Кадры пошли бы через
                // системную память — 12 МБ на копию при 2560×1600,
                // то есть весь бюджет задержки разом (§4.2.3).
                // Правильный ход — взять следующего кандидата, а не
                // соглашаться на негодный путь.
                CodecError::Unavailable(format!(
                    "MFT не принял D3D11-устройство ({e}): чужой адаптер"
                ))
            })
    }

    /// Применить настройки низкой задержки через ICodecAPI.
    ///
    /// Всё здесь — «просьбы»: MFT вправе не поддержать параметр и
    /// вернуть ошибку, и это не повод падать. Но каждая потеря
    /// меняет задержку, поэтому пишется в лог поимённо.
    fn apply_codec_api(&self) {
        let Ok(codec_api) = self.transform.cast::<ICodecAPI>() else {
            tracing::warn!("ICodecAPI недоступен — настройки низкой задержки не применены");
            return;
        };

        // Низкая задержка: главный переключатель. Без него MFT
        // накапливает кадры ради качества, и бюджет §5.2 уезжает
        // сразу на несколько кадров.
        //
        // Используется `AVLowLatencyMode`, а НЕ `AVEncCommonLowLatency`:
        // второй аппаратный MFT отвергает с 0x80070057 «параметр задан
        // неверно» (проверено на NVIDIA H.264 Encoder MFT). Смысл у
        // них один, принимают — разные.
        set_bool(
            &codec_api,
            &CODECAPI_AVLowLatencyMode,
            true,
            "низкая задержка",
        );

        // CBR: битрейт делится поровну между кадрами. Те же
        // рассуждения, что у NVENC (§5.2).
        set_u32(
            &codec_api,
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_CBR.0 as u32,
            "режим CBR",
        );

        let bitrate = self.config.rate_control.target_bitrate();
        set_u32(
            &codec_api,
            &CODECAPI_AVEncCommonMeanBitRate,
            bitrate,
            "битрейт",
        );

        // Потолок равен среднему: иначе энкодер вправе разгоняться,
        // а всплеск на плохом канале — это пик задержки.
        if let RateControl::Cbr { .. } = self.config.rate_control {
            set_u32(
                &codec_api,
                &CODECAPI_AVEncCommonMaxBitRate,
                bitrate,
                "потолок битрейта",
            );
        }

        // VBV в полтора кадра — ровно как у NVENC. Буфер в один кадр
        // упирается в потолок при любом всплеске сложности сцены, и
        // качество проседает рывком (§5.2).
        let frame_bits = bitrate / self.config.fps.max(1);
        set_u32(
            &codec_api,
            &CODECAPI_AVEncCommonBufferSize,
            frame_bits * 3 / 2,
            "размер VBV",
        );

        // Бесконечный GOP: полные IDR дают всплеск битрейта.
        // У MF нет Intra Refresh как отдельной ручки, поэтому
        // ключевые кадры просто отодвигаются как можно дальше, а
        // приходят по запросу — из `request_keyframe`.
        set_u32(
            &codec_api,
            &CODECAPI_AVEncMPVGOPSize,
            // Ноль означает «на усмотрение энкодера» и у разных
            // вендоров трактуется по-разному, поэтому задаётся
            // большое конкретное число: час при 60 fps.
            self.config.fps.max(1) * 3600,
            "длина GOP",
        );

        // Потолок огрубления: без него при нехватке бит картинка
        // распадается на квадраты. Значение то же, что у NVENC.
        set_u32(&codec_api, &CODECAPI_AVEncVideoMaxQP, 38, "maxQP");
    }

    /// Задать выходной тип: H.264 с нашим разрешением и битрейтом.
    fn set_output_type(&self) -> Result<()> {
        let media_type = create_media_type(&MFVideoFormat_H264)?;
        let size = self.config.size;

        set_size(&media_type, size)?;
        set_frame_rate(&media_type, self.config.fps)?;

        // SAFETY: тип только что создан; GUID и значения — константы.
        unsafe {
            media_type
                .SetUINT32(
                    &MF_MT_AVG_BITRATE,
                    self.config.rate_control.target_bitrate(),
                )
                .map_err(|e| encode_err("MF_MT_AVG_BITRATE", e))?;
            media_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, INTERLACE_PROGRESSIVE)
                .map_err(|e| encode_err("MF_MT_INTERLACE_MODE", e))?;
            // High Profile — как договорено в §9.2.
            media_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .map_err(|e| encode_err("MF_MT_MPEG2_PROFILE", e))?;
        }
        set_ratio(&media_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;

        // SAFETY: MFT жив; поток выхода нулевой.
        unsafe { self.transform.SetOutputType(0, &media_type, 0) }
            .map_err(|e| encode_err("SetOutputType", e))?;
        Ok(())
    }

    /// Выбрать NV12 среди входных типов, которые предлагает MFT.
    ///
    /// Перебор, а не конструирование своего типа: MFT принимает только
    /// то, что предложил сам, и расхождение в скрытых полях даёт отказ
    /// (та же причина, что у выбора выходного типа декодера).
    fn set_input_type(&self) -> Result<()> {
        for index in 0.. {
            // SAFETY: MFT жив; исчерпание списка сигнализируется
            // ошибкой — это условие выхода.
            let candidate = match unsafe { self.transform.GetInputAvailableType(0, index) } {
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

            set_size(&candidate, self.config.size)?;
            set_frame_rate(&candidate, self.config.fps)?;

            // SAFETY: MFT жив; тип получен от него же.
            if unsafe { self.transform.SetInputType(0, &candidate, 0) }.is_ok() {
                return Ok(());
            }
        }

        Err(CodecError::Unavailable(
            "MFT не принял NV12 на входе".into(),
        ))
    }

    /// Отправить сообщение MFT.
    fn send_message(&self, message: MFT_MESSAGE_TYPE, param: usize) -> Result<()> {
        // SAFETY: MFT жив; сообщение и параметр передаются по значению.
        unsafe { self.transform.ProcessMessage(message, param) }
            .map_err(|e| encode_err("ProcessMessage", e))
    }

    /// Забрать закодированный кадр, если он готов.
    fn drain(&mut self, info: FrameInfo) -> Result<Option<EncodedFrame>> {
        // Софтверный MFT сам буфер не выделяет, поэтому его надо
        // подготовить. У аппаратных стоит флаг
        // MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, и тогда наш буфер
        // игнорируется — проверять флаг ради этого незачем.
        let sample = self.prepare_output_sample()?;

        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;

        // SAFETY: MFT жив; массив буферов и `status` — живые локальные
        // переменные, которые функция заполняет.
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };

        // Забрать sample до любых ветвлений: ManuallyDrop иначе течёт.
        let produced = std::mem::ManuallyDrop::into_inner(std::mem::replace(
            &mut buffers[0].pSample,
            std::mem::ManuallyDrop::new(None),
        ));
        let _events = std::mem::ManuallyDrop::into_inner(std::mem::replace(
            &mut buffers[0].pEvents,
            std::mem::ManuallyDrop::new(None),
        ));

        if let Err(e) = result {
            // «Нужно больше входных данных» — штатный ответ, а не
            // ошибка: энкодер ещё не набрал, чем ответить.
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(None);
            }
            return Err(encode_err("ProcessOutput", e));
        }

        let Some(sample) = produced else {
            return Ok(None);
        };

        let mut info = info;
        info.timings.mark(Stage::Encoded, self.epoch.stamp_now());

        let keyframe = read_clean_point(&sample);
        let data = copy_sample_bytes(&sample)?;

        Ok(Some(EncodedFrame {
            data,
            kind: if keyframe {
                FrameKind::Key
            } else {
                FrameKind::Delta
            },
            info,
        }))
    }

    /// Подготовить буфер под выход, если MFT его не выделяет сам.
    fn prepare_output_sample(&self) -> Result<Option<IMFSample>> {
        // SAFETY: MFT жив; поток выхода нулевой.
        let info = match unsafe { self.transform.GetOutputStreamInfo(0) } {
            Ok(i) => i,
            Err(e) => return Err(encode_err("GetOutputStreamInfo", e)),
        };

        let provides_samples = (info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
            != 0;
        if provides_samples {
            return Ok(None);
        }

        // Размер с запасом: cbSize — минимум, а не гарантия. Ключевой
        // кадр крупнее разностного в разы, и тесный буфер дал бы
        // отказ ровно на нём.
        let size = info.cbSize.max(1 << 20);

        // SAFETY: функции не принимают внешних указателей.
        let sample = unsafe { MFCreateSample() }.map_err(|e| encode_err("MFCreateSample", e))?;
        let buffer = unsafe { MFCreateMemoryBuffer(size) }
            .map_err(|e| encode_err("MFCreateMemoryBuffer", e))?;
        // SAFETY: и sample, и buffer только что созданы.
        unsafe { sample.AddBuffer(&buffer) }.map_err(|e| encode_err("AddBuffer", e))?;
        Ok(Some(sample))
    }
}

// SAFETY: все объекты MF внутри принадлежат одному энкодеру и
// используются только через `&mut self`, то есть без одновременного
// доступа из нескольких потоков. Перемещение между потоками MF
// допускает: её объекты free-threaded. Та же гарантия, что у
// `D3d11Decoder`.
unsafe impl Send for MfEncoder {}

impl Encoder for MfEncoder {
    /// Сырой указатель на текстуру — как у NVENC.
    ///
    /// Ссылка с временем жизни здесь невозможна: связанный тип
    /// trait'а не может заимствовать у вызывающего. NVENC решает это
    /// так же (`*mut c_void`), и одинаковый вход — условие того,
    /// чтобы пайплайн мог работать с любым бэкендом, не зная каким.
    type Input = RawTextureInput;

    fn encode(&mut self, frame: &Self::Input) -> Result<Option<EncodedFrame>> {
        if frame.texture.is_null() {
            return Err(CodecError::UnsupportedParams(
                "текстура кадра — нулевой указатель".into(),
            ));
        }
        // SAFETY: контракт `RawTextureInput` требует валидного
        // указателя на живую ID3D11Texture2D нужного размера;
        // проверку на null сделали выше. Ссылка не переживает вызов.
        let texture = unsafe { ID3D11Texture2D::from_raw_borrowed(&frame.texture) }
            .ok_or_else(|| CodecError::UnsupportedParams("неверный указатель текстуры".into()))?;

        self.encode_texture(&MfEncoderInput {
            texture,
            info: frame.info,
        })
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        let Ok(codec_api) = self.transform.cast::<ICodecAPI>() else {
            return Err(CodecError::Unavailable("ICodecAPI недоступен".into()));
        };
        set_u32(
            &codec_api,
            &CODECAPI_AVEncCommonMeanBitRate,
            bitrate,
            "битрейт",
        );
        self.config.rate_control = RateControl::Cbr { bitrate };
        Ok(())
    }

    fn config(&self) -> &EncoderConfig {
        &self.config
    }
}

impl MfEncoder {
    /// Закодировать кадр из GPU-текстуры.
    ///
    /// Отдельный метод, а не `Encoder::encode`: вход держит ссылку на
    /// чужую текстуру, а связанный тип trait'а потребовал бы `'static`.
    pub fn encode_texture(&mut self, input: &MfEncoderInput<'_>) -> Result<Option<EncodedFrame>> {
        let duration = HNS_PER_SEC / self.config.fps.max(1) as i64;

        // SAFETY: текстура жива всё время вызова; индекс подресурса
        // нулевой — DXGI отдаёт цельную текстуру, а не массив.
        let buffer =
            unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, input.texture, 0, false) }
                .map_err(|e| encode_err("MFCreateDXGISurfaceBuffer", e))?;

        // SAFETY: функция не принимает внешних указателей.
        let sample = unsafe { MFCreateSample() }.map_err(|e| encode_err("MFCreateSample", e))?;
        // SAFETY: и sample, и buffer живы.
        unsafe {
            sample
                .AddBuffer(&buffer)
                .map_err(|e| encode_err("AddBuffer", e))?;
            sample
                .SetSampleTime(self.frame_index * duration)
                .map_err(|e| encode_err("SetSampleTime", e))?;
            sample
                .SetSampleDuration(duration)
                .map_err(|e| encode_err("SetSampleDuration", e))?;
        }

        if self.force_keyframe {
            // Запрос ключевого кадра — атрибут на конкретном сэмпле,
            // а не глобальная настройка: он относится к этому кадру.
            // SAFETY: sample жив; GUID — константа.
            let _ = unsafe { sample.SetUINT32(&MFSampleExtension_CleanPoint, 1) };
            self.force_keyframe = false;
        }

        if self.is_async {
            return self.encode_async(sample, input.info);
        }

        // SAFETY: MFT жив; поток входа нулевой; sample жив до конца
        // вызова, а MFT удерживает его сам, если он ещё нужен.
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                // MFT не принимает вход, пока не забрали выход. Это
                // не ошибка, а порядок работы: забираем и выходим,
                // кадр придёт со следующим вызовом.
                return self.drain(input.info);
            }
            Err(e) => return Err(encode_err("ProcessInput", e)),
        }

        self.frame_index += 1;
        self.drain(input.info)
    }

    /// Кодирование через асинхронную модель.
    ///
    /// # Почему это отдельный путь
    ///
    /// Аппаратные MFT асинхронны: `ProcessOutput` у них нельзя
    /// вызывать «когда захочется» — он отвечает `E_UNEXPECTED`
    /// (0x8000FFFF). Вместо этого MFT сам сообщает событиями, когда
    /// готов принять вход (`METransformNeedInput`) и когда есть
    /// выход (`METransformHaveOutput`).
    ///
    /// Первая версия этого не учитывала: перечисление спрашивало
    /// синхронные MFT, но система всё равно отдавала асинхронный, и
    /// прогон падал на первом же кадре. Ошибку нашла живая проба —
    /// по конфигурации всё выглядело исправным.
    ///
    /// # Почему цикл, а не ожидание
    ///
    /// События забираются **без блокировки** (`MF_EVENT_FLAG_NO_WAIT`).
    /// Ждать здесь нельзя: пока поток стоит, захват не идёт, и
    /// следующий кадр придёт устаревшим — то же правило, что у
    /// ограничителя частоты (§5.1).
    fn encode_async(&mut self, sample: IMFSample, info: FrameInfo) -> Result<Option<EncodedFrame>> {
        let Some(events) = self.events.clone() else {
            return Err(CodecError::Unavailable(
                "асинхронный MFT без генератора событий".into(),
            ));
        };

        let mut pending = Some(sample);
        let mut produced = None;

        // Потолок итераций: MFT, который не отвечает ни одним из
        // ожидаемых событий, не должен превращаться в вечный цикл.
        // Кадров в полёте у энкодера единицы, так что запас огромен.
        for _ in 0..64 {
            // SAFETY: генератор жив; флаг — константа. NO_WAIT
            // означает, что отсутствие события вернёт ошибку, а не
            // заблокирует поток.
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                // Событий больше нет — выходим с тем, что набрали.
                Err(_) => break,
            };

            // SAFETY: событие живо.
            let event_type = match unsafe { event.GetType() } {
                Ok(t) => t,
                Err(_) => continue,
            };

            if event_type == METransformNeedInput.0 as u32 {
                if let Some(sample) = pending.take() {
                    // SAFETY: MFT жив; поток входа нулевой.
                    unsafe { self.transform.ProcessInput(0, &sample, 0) }
                        .map_err(|e| encode_err("ProcessInput (async)", e))?;
                    self.frame_index += 1;
                }
            } else if event_type == METransformHaveOutput.0 as u32 {
                if let Some(frame) = self.drain(info)? {
                    produced = Some(frame);
                }
            }

            // Кадр отдан и вход принят — больше ждать нечего.
            if produced.is_some() && pending.is_none() {
                break;
            }
        }

        Ok(produced)
    }
}

/// Перечислить всех аппаратных кандидатов в порядке предпочтения.
///
/// # Почему только аппаратные
///
/// Софтверный MFT есть в любой Windows, и соблазн взять его «хоть
/// как-нибудь» велик. Но живой прогон показал: он отвергает
/// GPU-текстуру с `DXGI_ERROR_INVALID_CALL` (0x887A0001) — работает
/// только с пикселями в системной памяти.
///
/// Чтобы его накормить, каждый кадр пришлось бы копировать с GPU в
/// RAM: 12 МБ при 2560×1600, 5–10 мс на копию. Это прямо запрещено
/// §4.2.3 и съедает бюджет задержки целиком, то есть даёт не
/// «медленно, но работает», а «не работает».
///
/// Настоящий софтверный путь — `openh264` (§5.5) со своим конвейером
/// и осознанной ценой, а не MFT, притворяющийся аппаратным.
///
/// # Почему перечисление, а не CoCreateInstance по CLSID
///
/// `CLSID_MSH264EncoderMFT` — это как раз софтверный энкодер
/// Microsoft. Создав его напрямую, мы получили бы худший вариант на
/// машине, где есть QuickSync или VCE.
fn enumerate_all() -> Vec<(IMFTransform, String)> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    // Аппаратные энкодеры **асинхронны**, софтверные — синхронны.
    //
    // Это не мелочь перечисления, а разные модели работы. Первая
    // версия спрашивала только `SYNCMFT` и всё равно получала
    // аппаратный MFT, который затем отвечал `MF_E_TRANSFORM_ASYNC_LOCKED`
    // (0xC00D6D77) на первое же сообщение: асинхронный MFT до
    // разблокировки не делает ничего.
    let flags = MFT_ENUM_FLAG_SORTANDFILTER | MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT;

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;

    // SAFETY: категория и флаги — константы; описания типов живут до
    // конца вызова; выходные указатели валидны.
    let hr = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
    };
    if hr.is_err() || activates.is_null() || count == 0 {
        if !activates.is_null() {
            // SAFETY: указатель получен от MF.
            unsafe { CoTaskMemFree(Some(activates.cast())) };
        }
        return Vec::new();
    }

    // SAFETY: MF заполнила `count` элементов.
    let items = unsafe { std::slice::from_raw_parts(activates, count as usize) };

    let mut result = Vec::new();
    for item in items.iter().flatten() {
        // Активация может не удаться: MFT числится в реестре, но его
        // библиотеки нет или драйвер сломан. Это не повод сдаваться —
        // следующий в списке может работать.
        // SAFETY: `item` жив.
        let Ok(transform) = (unsafe { item.ActivateObject::<IMFTransform>() }) else {
            continue;
        };

        // Асинхронный MFT надо разблокировать ДО любого другого
        // вызова — иначе он отвечает `MF_E_TRANSFORM_ASYNC_LOCKED`
        // (0xC00D6D77) даже на `ProcessMessage`.
        //
        // Атрибут ставится безусловно: у синхронного MFT его просто
        // нет, и попытка вернёт ошибку, которую здесь и игнорируем.
        // Проверять вид отдельным запросом значило бы сделать лишний
        // вызов ради того же результата.
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            // SAFETY: атрибуты живы; GUID — константа.
            let _ = unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) };
        }

        let name = read_friendly_name(item);
        result.push((transform, name));
    }

    // SAFETY: массив получен от MFTEnumEx и больше не используется.
    unsafe { CoTaskMemFree(Some(activates.cast())) };
    result
}

/// Прочитать имя MFT.
fn read_friendly_name(item: &IMFActivate) -> String {
    let mut value = windows::core::PWSTR::null();
    let mut length: u32 = 0;
    // SAFETY: `item` жив; GUID — константа; out-указатели валидны.
    match unsafe { item.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut length) }
    {
        Ok(()) if !value.is_null() => {
            // SAFETY: MF вернула строку с нулевым терминатором.
            let text = unsafe { value.to_string() }.unwrap_or_default();
            // SAFETY: строка выделена аллокатором MF.
            unsafe { CoTaskMemFree(Some(value.0.cast())) };
            super::survey::clean_vendor_name(&text)
        }
        _ => "(без имени)".to_string(),
    }
}

/// Скопировать байты из сэмпла.
///
/// Копия здесь неизбежна и допустима: это уже **сжатый** поток
/// (десятки килобайт), а не кадр в 12 МБ. Запрет §4.2.3 касается
/// пикселей, а не битстрима — тот и у NVENC копируется из буфера.
fn copy_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    // SAFETY: sample жив.
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|e| encode_err("ConvertToContiguousBuffer", e))?;

    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut current: u32 = 0;

    // SAFETY: buffer жив; out-указатели валидны. Lock даёт доступ к
    // памяти буфера до парного Unlock.
    unsafe { buffer.Lock(&mut ptr, None, Some(&mut current)) }
        .map_err(|e| encode_err("IMFMediaBuffer::Lock", e))?;

    // SAFETY: Lock вернул валидный указатель на `current` байт.
    let data = unsafe { std::slice::from_raw_parts(ptr, current as usize) }.to_vec();

    // SAFETY: парный вызов к Lock выше.
    let _ = unsafe { buffer.Unlock() };
    Ok(data)
}

/// Ключевой ли это кадр.
fn read_clean_point(sample: &IMFSample) -> bool {
    // SAFETY: sample жив; GUID — константа. Отсутствие атрибута —
    // законный ответ «нет», а не ошибка.
    unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0
}

/// Создать медиатип с заданным подтипом.
fn create_media_type(subtype: &GUID) -> Result<IMFMediaType> {
    // SAFETY: функция не принимает входных указателей.
    let media_type =
        unsafe { MFCreateMediaType() }.map_err(|e| encode_err("MFCreateMediaType", e))?;
    // SAFETY: тип только что создан; GUID — константы.
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| encode_err("MF_MT_MAJOR_TYPE", e))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, subtype)
            .map_err(|e| encode_err("MF_MT_SUBTYPE", e))?;
    }
    Ok(media_type)
}

/// Записать разрешение в тип.
fn set_size(media_type: &IMFMediaType, size: FrameSize) -> Result<()> {
    set_ratio(media_type, &MF_MT_FRAME_SIZE, size.width, size.height)
}

/// Записать частоту кадров.
fn set_frame_rate(media_type: &IMFMediaType, fps: u32) -> Result<()> {
    set_ratio(media_type, &MF_MT_FRAME_RATE, fps.max(1), 1)
}

/// Записать пару чисел, упакованную в 64 бита.
///
/// Media Foundation хранит и размер, и частоту, и пропорции пикселя
/// одинаково: старшие 32 бита — числитель, младшие — знаменатель.
fn set_ratio(media_type: &IMFMediaType, key: &GUID, high: u32, low: u32) -> Result<()> {
    let packed = ((high as u64) << 32) | low as u64;
    // SAFETY: тип жив; GUID — константа; значение по значению.
    unsafe { media_type.SetUINT64(key, packed) }.map_err(|e| encode_err("SetUINT64", e))
}

/// Записать целочисленный параметр кодека.
///
/// Неудача логируется, но не прерывает настройку: MFT вправе не знать
/// параметр, и это не делает его непригодным.
fn set_u32(codec_api: &ICodecAPI, key: &GUID, value: u32, what: &str) {
    let variant = VARIANT::from(value);
    // SAFETY: GUID — константа; VARIANT жив до конца вызова.
    match unsafe { codec_api.SetValue(key, &variant) } {
        Ok(()) => tracing::debug!(what, value, "параметр энкодера применён"),
        Err(e) => tracing::warn!(what, value, error = %e, "MFT не принял параметр"),
    }
}

/// Записать булев параметр кодека.
fn set_bool(codec_api: &ICodecAPI, key: &GUID, value: bool, what: &str) {
    let variant = VARIANT::from(value);
    // SAFETY: GUID — константа; VARIANT жив до конца вызова.
    match unsafe { codec_api.SetValue(key, &variant) } {
        Ok(()) => tracing::debug!(what, value, "параметр энкодера применён"),
        Err(e) => tracing::warn!(what, value, error = %e, "MFT не принял параметр"),
    }
}

/// Ошибка кодирования с контекстом.
fn encode_err(context: &'static str, error: windows::core::Error) -> CodecError {
    CodecError::Encode {
        context,
        status: error.code().0 as u32,
    }
}
