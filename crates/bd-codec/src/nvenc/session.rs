//! Сессия кодирования NVENC.
//!
//! # RAII
//!
//! [`NvencSession`] владеет хендлом энкодера и освобождает его в `Drop`
//! (CLAUDE.md §4.3.4). Публичного способа не освободить ресурс нет:
//! сырой указатель не покидает модуль.
//!
//! # Zero-copy
//!
//! D3D11-текстура регистрируется в NVENC напрямую через
//! `nvEncRegisterResource`. Кадр не проходит через системную память —
//! это основа бюджета задержки (CLAUDE.md §4.2.3, §5.2).

use super::api::{check_status, NvencApi};
use super::{guids, sys, versions};
use crate::{CodecError, EncoderConfig, RateControl, Result};
use std::ffi::c_void;
use std::ptr;

/// Открытая сессия кодирования.
pub struct NvencSession {
    /// Хендл энкодера. Не `pub`: сырой указатель наружу не отдаётся.
    encoder: *mut c_void,
    api: &'static NvencApi,
    config: EncoderConfig,
}

// SAFETY: сессию NVENC можно использовать из одного потока
// одновременно, но перемещать между потоками допустимо — драйвер
// не привязывает её к потоку создания. Поэтому Send, но не Sync.
unsafe impl Send for NvencSession {}

impl NvencSession {
    /// Открыть сессию для D3D11-устройства.
    ///
    /// `device` — указатель на `ID3D11Device`. Устройство обязано
    /// оставаться живым всё время жизни сессии; это обеспечивается
    /// вызывающей стороной, которая хранит их вместе.
    ///
    /// # Safety
    ///
    /// `device` должен быть валидным указателем на живой `ID3D11Device`,
    /// переживающий возвращённую сессию.
    pub unsafe fn open_d3d11(device: *mut c_void, config: EncoderConfig) -> Result<Self> {
        let api = NvencApi::get()?;

        let mut params = sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: versions::struct_version_for(
                api.api_version(),
                versions::num::OPEN_SESSION_EX_PARAMS,
            ),
            deviceType: sys::_NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device,
            // Версия, согласованная с драйвером, а не версия SDK:
            // драйвер отвергает вызов с неизвестной ему версией.
            apiVersion: api.api_version(),
            ..Default::default()
        };

        let open = api.functions().nvEncOpenEncodeSessionEx.ok_or_else(|| {
            CodecError::Unavailable("nvEncOpenEncodeSessionEx отсутствует".into())
        })?;

        let mut encoder: *mut c_void = ptr::null_mut();
        // SAFETY: `params` заполнена по контракту (version и apiVersion
        // обязательны), `device` валиден по требованию функции,
        // `encoder` — валидный указатель для записи результата.
        let status = unsafe { open(&mut params, &mut encoder) };
        check_status(status, "nvEncOpenEncodeSessionEx")
            .inspect_err(|e| tracing::error!(?e, "открытие сессии не удалось"))?;

        if encoder.is_null() {
            return Err(CodecError::InitFailed(
                "драйвер вернул пустой хендл энкодера".into(),
            ));
        }

        let session = Self {
            encoder,
            api,
            config,
        };
        session.initialize()?;
        Ok(session)
    }

    /// GUID кодека из настроек.
    fn codec_guid(&self) -> sys::GUID {
        match self.config.codec {
            crate::Codec::H264 => guids::CODEC_H264,
            crate::Codec::Av1 => guids::CODEC_AV1,
        }
    }

    /// Пресет: компромисс между скоростью и чёткостью текста (§5.2).
    const PRESET: sys::GUID = guids::PRESET_P3;

    /// Профиль настройки — низкая задержка.
    const TUNING: sys::NV_ENC_TUNING_INFO::Type =
        sys::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;

    /// Собрать `NV_ENC_CONFIG` для заданного управления битрейтом.
    ///
    /// За основу берётся готовая конфигурация пресета: заполнять
    /// `NV_ENC_CONFIG` с нуля нельзя — в ней десятки полей, и пропуск
    /// любого даёт неверную настройку.
    ///
    /// Используется и при инициализации, и при смене битрейта на лету.
    pub(crate) fn build_config(&self, rate_control: RateControl) -> Result<sys::NV_ENC_CONFIG> {
        let api_ver = self.api.api_version();

        let mut preset_config = sys::NV_ENC_PRESET_CONFIG {
            version: versions::struct_version_ex_for(api_ver, versions::num::PRESET_CONFIG),
            presetCfg: sys::NV_ENC_CONFIG {
                version: versions::struct_version_ex_for(api_ver, versions::num::CONFIG),
                ..Default::default()
            },
            ..Default::default()
        };

        let get_preset = self
            .api
            .functions()
            .nvEncGetEncodePresetConfigEx
            .ok_or_else(|| {
                CodecError::Unavailable("nvEncGetEncodePresetConfigEx отсутствует".into())
            })?;

        // SAFETY: `encoder` валиден (проверен при открытии сессии);
        // GUID-ы — константы из заголовка; `preset_config` заполнена
        // требуемыми полями version.
        let status = unsafe {
            get_preset(
                self.encoder,
                self.codec_guid(),
                Self::PRESET,
                Self::TUNING,
                &mut preset_config,
            )
        };
        check_status(status, "nvEncGetEncodePresetConfigEx")
            .inspect_err(|e| tracing::error!(?e, "получение пресета не удалось"))?;

        let mut enc_config = preset_config.presetCfg;
        enc_config.version = versions::struct_version_ex_for(api_ver, versions::num::CONFIG);

        // --- Настройки, критичные для задержки (CLAUDE.md §5.2) ---

        // Бесконечный GOP: полные IDR дают всплеск битрейта и пик
        // задержки. Обновление картинки идёт через Intra Refresh.
        enc_config.gopLength = sys::NVENC_INFINITE_GOPLENGTH;
        // Без B-кадров: они требуют кадров из будущего, то есть
        // добавляют минимум один кадр задержки.
        enc_config.frameIntervalP = 1;

        // --- Intra Refresh ---
        //
        // Настройка была объявлена в `EncoderConfig` с самого начала,
        // проверялась тестом как «обязательная по §5.2» — и **не
        // доходила до NVENC вообще**. CLAUDE.md честно называл её
        // декоративной; здесь она наконец применяется.
        //
        // # Что она чинит
        //
        // Полный IDR при 2560×1600 весит 88 КБ при бюджете кадра
        // 61 КБ. В VBV он не помещается **никогда** — ни при полутора
        // кадрах буфера, ни при двух: при 1080p/15 Мбит/с это 63 КБ
        // против 46 КБ, та же пропорция. Энкодер, обязанный уложиться
        // в CBR, огрубляет картинку — это и есть пикселизация,
        // которую видно глазом.
        //
        // Intra Refresh размазывает обновление по `intraRefreshCnt`
        // кадрам: вместо одного кадра в 88 КБ идёт полоса макроблоков
        // в каждом из N кадров. Всплеска нет, в VBV укладывается,
        // опорная точка для декодера появляется всё равно.
        //
        // # Почему период именно такой
        //
        // Период — 4 секунды: чаще значит платить битрейтом ни за
        // что (при бесконечном GOP обновление нужно не для сжатия, а
        // для восстановления после потерь), реже — дольше ждать
        // выздоровления после обрыва.
        //
        // Длина полосы — половина секунды. Растянуть сильнее значит
        // размазать всплеск тоньше, но и восстановление после потери
        // затянется на столько же: клиент, потерявший опору, увидит
        // мусор, пока полоса не пройдёт весь кадр.
        //
        // # Ограничение, записанное в заголовке
        //
        // «Will be disabled if gopLength is not set to
        // NVENC_INFINITE_GOPLENGTH» — у нас он выставлен строкой
        // выше, поэтому настройка действует. Поменяв GOP, надо
        // помнить, что Intra Refresh отключится молча.
        if self.config.intra_refresh {
            let fps = self.config.fps.max(1);
            // SAFETY: объединение `encodeCodecConfig` читается как
            // `h264Config`, потому что кодек сессии — H.264: он задан
            // `codec_guid()` при открытии, и другой вариант тут
            // невозможен. Поле `_bitfield_1` в структуре уже
            // проинициализировано пресетом, поэтому запись идёт через
            // сеттер, а не через `Default`.
            unsafe {
                let h264 = &mut enc_config.encodeCodecConfig.h264Config;
                h264.set_enableIntraRefresh(1);
                h264.intraRefreshPeriod = fps * 4;
                h264.intraRefreshCnt = fps / 2;
            }

            // Настройка записывается в лог именно потому, что до сих
            // пор она была декоративной: объявлена в конфиге, покрыта
            // тестом «обязательна по §5.2» — и не доходила до NVENC.
            //
            // Молчаливая настройка не имеет свидетеля, а значит
            // ничем не отличается от отсутствующей (тот же урок, что
            // находка 26: ноль срабатываний — это «неизвестно»).
            tracing::info!(
                period = fps * 4,
                count = fps / 2,
                "Intra Refresh включён вместо полных IDR"
            );
        }

        // Look-ahead выключается явно, а не «по умолчанию»: он
        // требует накопить несколько кадров до кодирования, то есть
        // прямо добавляет задержку (CLAUDE.md §5.2). Пресет может
        // включить его сам, и тогда бюджет уехал бы незаметно.
        enc_config.rcParams.set_enableLookahead(0);
        enc_config.rcParams.lookaheadDepth = 0;

        // Адаптивное квантование: биты перераспределяются в пользу
        // участков, где артефакты заметнее глазу. Для рабочего стола
        // это в первую очередь текст — главный сценарий продукта
        // (§7.2). Без AQ энкодер при нехватке битрейта огрубляет
        // кадр равномерно, и текст «плывёт» первым.
        enc_config.rcParams.set_enableAQ(1);

        match rate_control {
            RateControl::Cbr { bitrate } => {
                enc_config.rcParams.rateControlMode =
                    sys::_NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
                enc_config.rcParams.averageBitRate = bitrate;
                // При CBR максимум обязан равняться среднему. Пресет
                // оставляет здесь своё значение, и если оно меньше
                // нашего среднего, контроллер молча зажимает поток —
                // ошибки при этом нет, есть только просевшее качество.
                enc_config.rcParams.maxBitRate = bitrate;
                // VBV размером в один кадр — ключ к низкой задержке:
                // энкодер не накапливает данные впрок.
                //
                // Но буфер ровно в один кадр слишком жёсток: любой
                // всплеск сложности сцены упирается в потолок в тот
                // же кадр, и качество проседает рывком — это видно
                // как пикселизация всей картинки. Полтора кадра
                // дают запас на всплеск, оставаясь много меньше
                // кадра задержки.
                let frame_bits = bitrate / self.config.fps.max(1);
                enc_config.rcParams.vbvBufferSize = frame_bits * 3 / 2;
                enc_config.rcParams.vbvInitialDelay = enc_config.rcParams.vbvBufferSize;

                // Потолок огрубления. Без него энкодер при нехватке
                // бит уходит в QP 45–51, где картинка распадается на
                // крупные квадраты. Порог 38 держит текст читаемым:
                // энкодер скорее пропустит детали движения, чем
                // разрушит весь кадр.
                //
                // Цена: в редких сценах поток может превысить
                // заданный битрейт. Для рабочего стола это верный
                // размен — нечитаемый текст хуже, чем короткий
                // всплеск трафика.
                enc_config.rcParams.set_enableMaxQP(1);
                enc_config.rcParams.maxQP = sys::NV_ENC_QP {
                    qpInterP: 38,
                    qpInterB: 38,
                    qpIntra: 38,
                };
            }
            RateControl::Vbr { average, max } => {
                enc_config.rcParams.rateControlMode =
                    sys::_NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
                enc_config.rcParams.averageBitRate = average;
                enc_config.rcParams.maxBitRate = max;
            }
        }

        Ok(enc_config)
    }

    /// Собрать `NV_ENC_INITIALIZE_PARAMS`, ссылающиеся на `config`.
    ///
    /// Возвращаемая структура держит указатель на `config`, поэтому
    /// та обязана пережить использование результата. Время жизни
    /// связывает их на уровне сигнатуры.
    pub(crate) fn build_init_params(
        &self,
        config: &mut sys::NV_ENC_CONFIG,
    ) -> sys::NV_ENC_INITIALIZE_PARAMS {
        sys::NV_ENC_INITIALIZE_PARAMS {
            version: versions::struct_version_ex_for(
                self.api.api_version(),
                versions::num::INITIALIZE_PARAMS,
            ),
            encodeGUID: self.codec_guid(),
            presetGUID: Self::PRESET,
            encodeWidth: self.config.size.width,
            encodeHeight: self.config.size.height,
            darWidth: self.config.size.width,
            darHeight: self.config.size.height,
            frameRateNum: self.config.fps,
            frameRateDen: 1,
            // Асинхронный режим требует событий Windows; синхронный
            // проще и для одного потока кадров не медленнее.
            enableEncodeAsync: 0,
            enablePTD: 1,
            encodeConfig: config as *mut _,
            tuningInfo: Self::TUNING,
            ..Default::default()
        }
    }

    /// Обновить запомненное управление битрейтом после реконфигурации.
    pub(crate) fn set_rate_control(&mut self, rate_control: RateControl) {
        self.config.rate_control = rate_control;
    }

    /// Настроить сессию под низкую задержку.
    fn initialize(&self) -> Result<()> {
        let mut enc_config = self.build_config(self.config.rate_control)?;
        let mut init_params = self.build_init_params(&mut enc_config);

        let initialize =
            self.api.functions().nvEncInitializeEncoder.ok_or_else(|| {
                CodecError::Unavailable("nvEncInitializeEncoder отсутствует".into())
            })?;

        // SAFETY: `encoder` валиден; `init_params` заполнена по контракту,
        // включая version и указатель на живую `enc_config`, которая
        // существует до конца вызова.
        let status = unsafe { initialize(self.encoder, &mut init_params) };
        check_status(status, "nvEncInitializeEncoder")
            .inspect_err(|e| tracing::error!(?e, "инициализация энкодера не удалась"))?;

        tracing::info!(
            width = self.config.size.width,
            height = self.config.size.height,
            fps = self.config.fps,
            bitrate = self.config.rate_control.target_bitrate(),
            "сессия NVENC инициализирована"
        );

        Ok(())
    }

    /// Настройки сессии.
    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// Хендл энкодера для внутренних вызовов.
    #[allow(dead_code)] // используется кодированием кадров (следующий шаг)
    pub(crate) fn handle(&self) -> *mut c_void {
        self.encoder
    }

    /// Таблица функций.
    #[allow(dead_code)] // используется кодированием кадров (следующий шаг)
    pub(crate) fn api(&self) -> &'static NvencApi {
        self.api
    }
}

impl Drop for NvencSession {
    fn drop(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        let Some(destroy) = self.api.functions().nvEncDestroyEncoder else {
            // Драйвер без этой функции — аномалия; утечка здесь лучше,
            // чем вызов по нулевому указателю.
            tracing::error!("nvEncDestroyEncoder отсутствует, сессия не освобождена");
            return;
        };

        // SAFETY: `encoder` получен из nvEncOpenEncodeSessionEx, не был
        // уничтожен ранее (Drop вызывается ровно один раз) и не равен null.
        let status = unsafe { destroy(self.encoder) };
        if status != sys::_NVENCSTATUS::NV_ENC_SUCCESS {
            tracing::warn!(status, "nvEncDestroyEncoder вернул ошибку");
        }
        self.encoder = ptr::null_mut();
    }
}
