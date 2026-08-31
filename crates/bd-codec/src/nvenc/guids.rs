//! GUID кодеков, профилей и пресетов NVENC.
//!
//! В заголовке они объявлены как `static const GUID`, и bindgen такие
//! определения не переносит — их приходится продублировать здесь.
//! Значения выписаны из `Interface/nvEncodeAPI.h` SDK 13.1.
//!
//! **Ошибка в любой цифре приводит не к ошибке компиляции, а к отказу
//! драйвера в рантайме** (или, хуже, к выбору не того пресета). Поэтому
//! каждый GUID сопровождается исходной строкой заголовка, чтобы сверка
//! глазами была возможна.

use super::sys::GUID;

/// Собрать GUID из полей в том же порядке, что в заголовке C.
const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> GUID {
    GUID {
        Data1: data1,
        Data2: data2,
        Data3: data3,
        Data4: data4,
    }
}

/// H.264 / AVC.
///
/// `{ 0x6bc82762, 0x4e63, 0x4ca4, { 0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf } }`
pub const CODEC_H264: GUID = guid(
    0x6bc8_2762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);

/// AV1. Пока не используется (CLAUDE.md §9.2), но абстракция кодека
/// должна позволять переключение без переделок.
///
/// `{ 0x0a352289, 0x0aa7, 0x4759, { 0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54 } }`
pub const CODEC_AV1: GUID = guid(
    0x0a35_2289,
    0x0aa7,
    0x4759,
    [0x86, 0x2d, 0x5d, 0x15, 0xcd, 0x16, 0xd2, 0x54],
);

/// H.264 High Profile — то, что мы используем (CLAUDE.md §1).
///
/// `{ 0xe7cbc309, 0x4f7a, 0x4b89, { 0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10 } }`
#[allow(dead_code)] // будет задан явно при настройке профиля
pub const H264_PROFILE_HIGH: GUID = guid(
    0xe7cb_c309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);

/// Пресет P1 — максимальная производительность, минимальная задержка.
///
/// `{ 0xfc0a8d3e, 0x45f8, 0x4cf8, { 0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf } }`
#[allow(dead_code)] // альтернатива P3, если понадобится ещё меньше задержки
pub const PRESET_P1: GUID = guid(
    0xfc0a_8d3e,
    0x45f8,
    0x4cf8,
    [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
);

/// Пресет P3 — компромисс: заметно лучше качество при небольшом росте
/// времени кодирования. Разумная точка старта для нашего сценария,
/// где важна чёткость текста (CLAUDE.md §5.2).
///
/// `{ 0x36850110, 0x3a07, 0x441f, { 0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6 } }`
pub const PRESET_P3: GUID = guid(
    0x3685_0110,
    0x3a07,
    0x441f,
    [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6],
);

/// Сравнение GUID по значению.
///
/// `GUID` из биндингов не реализует `PartialEq`, а сравнивать их нужно
/// при переборе поддерживаемых кодеков.
#[allow(dead_code)] // понадобится при переборе поддерживаемых кодеков
pub fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_are_distinct() {
        // Опечатка в одной цифре не даст ошибки компиляции, но сломает
        // выбор кодека в рантайме. Хотя бы различимость проверим.
        assert!(!guid_eq(&CODEC_H264, &CODEC_AV1));
        assert!(!guid_eq(&PRESET_P1, &PRESET_P3));
        assert!(!guid_eq(&CODEC_H264, &H264_PROFILE_HIGH));
    }

    #[test]
    fn guid_eq_is_reflexive() {
        assert!(guid_eq(&CODEC_H264, &CODEC_H264));
        assert!(guid_eq(&PRESET_P1, &PRESET_P1));
    }

    #[test]
    fn h264_guid_matches_header() {
        // Сверка с `Interface/nvEncodeAPI.h`: неверный GUID приведёт
        // к NV_ENC_ERR_UNSUPPORTED_PARAM в рантайме.
        assert_eq!(CODEC_H264.Data1, 0x6bc8_2762);
        assert_eq!(CODEC_H264.Data2, 0x4e63);
        assert_eq!(CODEC_H264.Data3, 0x4ca4);
        assert_eq!(
            CODEC_H264.Data4,
            [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf]
        );
    }
}
