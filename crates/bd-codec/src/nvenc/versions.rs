//! Версии структур NVENC.
//!
//! # Зачем этот модуль существует
//!
//! Каждая структура NVENC несёт поле `version`, и драйвер **отвергает**
//! вызов, если оно не совпадает с ожидаемым. В заголовке эти значения
//! заданы функциональными макросами, которые bindgen не переносит:
//!
//! ```c
//! #define NVENCAPI_STRUCT_VERSION(ver) \
//!     ((uint32_t)NVENCAPI_VERSION | ((ver)<<16) | (0x7 << 28))
//! #define NV_ENC_INITIALIZE_PARAMS_VER (NVENCAPI_STRUCT_VERSION(7) | (1<<31))
//! ```
//!
//! Поэтому формула воспроизведена здесь, а номера версий выписаны
//! вручную из `vendor/nvcodec/nvEncodeAPI.h`.
//!
//! **Осторожно:** ошибка в номере не даёт ошибки компиляции — только
//! `NV_ENC_ERR_INVALID_VERSION` в рантайме. При обновлении заголовка
//! номера в [`num`] надо сверить заново.

use super::sys;

/// Формула `NVENCAPI_STRUCT_VERSION(ver)` для версии API заголовков.
const fn struct_version(ver: u32) -> u32 {
    sys::NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}

/// Та же формула, но с явно переданной версией API.
///
/// Версия API вшита в каждую версию структуры, поэтому она берётся
/// из [`NvencApi`](super::NvencApi), а не из констант заголовка —
/// так значение остаётся в одном месте.
pub fn struct_version_for(api_version: u32, ver: u32) -> u32 {
    api_version | (ver << 16) | (0x7 << 28)
}

/// [`struct_version_for`] с установленным старшим битом.
///
/// Часть структур помечена дополнительным `(1<<31)` — так драйвер
/// отличает новую раскладку полей от старой.
pub fn struct_version_ex_for(api_version: u32, ver: u32) -> u32 {
    struct_version_for(api_version, ver) | (1 << 31)
}

/// Номера версий структур — второй аргумент `NVENCAPI_STRUCT_VERSION`.
///
/// Выписаны из `vendor/nvcodec/nvEncodeAPI.h` (API 13.0).
///
/// Часть констант ещё не задействована: они нужны кодированию кадров
/// и регистрации ресурсов — следующий шаг этапа 1. Выписаны сразу,
/// пока заголовок под рукой, потому что сверка номеров вручную —
/// именно то место, где легко ошибиться незаметно.
#[allow(dead_code)]
pub mod num {
    /// `NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER` = `STRUCT_VERSION(1)`
    pub const OPEN_SESSION_EX_PARAMS: u32 = 1;
    /// `NV_ENC_INITIALIZE_PARAMS_VER` = `STRUCT_VERSION(7) | (1<<31)`
    pub const INITIALIZE_PARAMS: u32 = 7;
    /// `NV_ENC_CONFIG_VER` = `STRUCT_VERSION(9) | (1<<31)`
    pub const CONFIG: u32 = 9;
    /// `NV_ENC_PRESET_CONFIG_VER` = `STRUCT_VERSION(5) | (1<<31)`
    pub const PRESET_CONFIG: u32 = 5;
    /// `NV_ENC_PIC_PARAMS_VER` = `STRUCT_VERSION(7) | (1<<31)`
    pub const PIC_PARAMS: u32 = 7;
    /// `NV_ENC_REGISTER_RESOURCE_VER` = `STRUCT_VERSION(5)`
    pub const REGISTER_RESOURCE: u32 = 5;
    /// `NV_ENC_MAP_INPUT_RESOURCE_VER` = `STRUCT_VERSION(4)`
    pub const MAP_INPUT_RESOURCE: u32 = 4;
    /// `NV_ENC_CREATE_BITSTREAM_BUFFER_VER` = `STRUCT_VERSION(1)`
    pub const CREATE_BITSTREAM_BUFFER: u32 = 1;
    /// `NV_ENC_LOCK_BITSTREAM_VER` = `STRUCT_VERSION(2)`
    pub const LOCK_BITSTREAM: u32 = 2;
    /// `NV_ENC_RECONFIGURE_PARAMS_VER` = `STRUCT_VERSION(1) | (1<<31)`
    pub const RECONFIGURE_PARAMS: u32 = 1;
}

/// `NV_ENCODE_API_FUNCTION_LIST_VER`.
///
/// Единственная константа, нужная до загрузки API: с ней вызывается
/// `NvEncodeAPICreateInstance`. Все прочие версии строятся уже от
/// версии, отданной драйвером — см. [`struct_version_for`].
pub const FUNCTION_LIST: u32 = struct_version(2);

#[cfg(test)]
mod tests {
    use super::*;

    /// Версия API из заголовков — то, что подставляется в рантайме.
    fn api_ver() -> u32 {
        sys::NVENCAPI_VERSION
    }

    #[test]
    fn version_formula_matches_header() {
        // NVENCAPI_VERSION = MAJOR | (MINOR << 24).
        let expected = sys::NVENCAPI_MAJOR_VERSION | (sys::NVENCAPI_MINOR_VERSION << 24);
        assert_eq!(
            sys::NVENCAPI_VERSION,
            expected,
            "формула NVENCAPI_VERSION разошлась с заголовком"
        );
    }

    #[test]
    fn struct_versions_carry_api_version() {
        // Версия структуры обязана нести версию API в младших битах,
        // иначе драйвер отвергнет вызов с NV_ENC_ERR_INVALID_VERSION.
        let mask = 0x0000_FFFF;
        let cases = [
            ("FUNCTION_LIST", FUNCTION_LIST),
            (
                "INITIALIZE_PARAMS",
                struct_version_ex_for(api_ver(), num::INITIALIZE_PARAMS),
            ),
            ("CONFIG", struct_version_ex_for(api_ver(), num::CONFIG)),
            (
                "PIC_PARAMS",
                struct_version_ex_for(api_ver(), num::PIC_PARAMS),
            ),
        ];
        for (name, value) in cases {
            assert_eq!(
                value & mask,
                sys::NVENCAPI_VERSION & mask,
                "{name}: версия API не совпадает"
            );
        }
    }

    #[test]
    fn ex_versions_have_high_bit_set() {
        // Признак новой раскладки структур.
        assert_ne!(
            struct_version_ex_for(api_ver(), num::INITIALIZE_PARAMS) & (1 << 31),
            0
        );
        assert_ne!(struct_version_ex_for(api_ver(), num::CONFIG) & (1 << 31), 0);
        // А эти — без него.
        assert_eq!(FUNCTION_LIST & (1 << 31), 0);
        assert_eq!(
            struct_version_for(api_ver(), num::REGISTER_RESOURCE) & (1 << 31),
            0
        );
    }

    #[test]
    fn struct_version_for_matches_header_formula() {
        // При совпадении версий обе формулы обязаны давать одно и то же.
        for ver in [1u32, 2, 5, 7, 9] {
            assert_eq!(
                struct_version_for(sys::NVENCAPI_VERSION, ver),
                struct_version(ver),
                "расхождение формул для ver={ver}"
            );
        }
    }
}
