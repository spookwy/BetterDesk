//! Перечисление H.264-энкодеров, доступных на машине.
//!
//! # Зачем это нужно
//!
//! Единственный реализованный энкодер — NVENC (§5.2), то есть хостом
//! может быть только машина с NVIDIA. Для проверки «работает у всех»
//! и для двусторонней сессии этого мало: нужно знать, есть ли на
//! чужой машине **хоть какой-нибудь** аппаратный энкодер H.264 —
//! Intel QuickSync, AMD VCE или встроенный в Windows.
//!
//! Модуль ничего не кодирует. Он отвечает на один вопрос: что здесь
//! вообще есть. Ответ нужен до того, как писать бэкенд: если у машины
//! нет ни одного энкодера, бэкенд ей не поможет, а если есть — видно,
//! какой именно писать.
//!
//! # Почему перечисление, а не попытка создать
//!
//! Создать энкодер — значит выбрать формат, разрешение и битрейт, то
//! есть принять с десяток решений до того, как известно, с чем имеем
//! дело. Перечисление их не требует и не может испортить состояние
//! системы.
//!
//! Оговорка, которую нельзя забывать: **перечисление не является
//! проверкой работоспособности**. MFT в списке может отказать при
//! создании — так же, как NVENC отказал на несовместимых заголовках
//! (находка 12), хотя API загружался. Поэтому результат называется
//! «заявлено», а не «работает».

use super::runtime::MediaFoundation;
use crate::{CodecError, Result};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MFMediaType_Video, MFTEnumEx, MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_FLAG_TRANSCODE_ONLY,
    MFT_REGISTER_TYPE_INFO,
};

/// Один найденный энкодер.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderEntry {
    /// Имя, под которым его показывает система.
    pub name: String,
    /// Аппаратный ли он.
    ///
    /// Софтверный энкодер H.264 в Windows есть всегда, и он **не
    /// годится** для нашей задержки: §5.2 требует только аппаратный.
    /// Но знать о нём полезно — он объясняет, почему список
    /// непустой, а хостить всё равно нельзя.
    pub hardware: bool,
}

/// Перечислить H.264-энкодеры.
///
/// Возвращает и аппаратные, и софтверные: различать их — дело
/// вызывающего, а скрывать софтверные значило бы отвечать на вопрос
/// «что есть» неполно.
pub fn h264_encoders() -> Result<Vec<EncoderEntry>> {
    // MF должна быть инициализирована: без неё MFTEnumEx вернёт
    // ошибку, которую легко принять за «энкодеров нет».
    let _mf = MediaFoundation::startup()?;

    let mut found = Vec::new();
    // Аппаратные и софтверные перечисляются отдельно: флаг
    // HARDWARE — это фильтр, а не признак в результате, и узнать
    // постфактум, каким был найден MFT, уже нельзя.
    collect(true, &mut found)?;
    collect(false, &mut found)?;
    Ok(found)
}

/// Перечислить энкодеры одного вида и дописать в `out`.
fn collect(hardware: bool, out: &mut Vec<EncoderEntry>) -> Result<()> {
    // Вход — NV12: это формат, который отдаёт наш декодер и с которым
    // работает пайплайн. Спрашивать про другие форматы значило бы
    // получить список, к нашему случаю не относящийся.
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
    if hardware {
        flags |= MFT_ENUM_FLAG_HARDWARE;
    }
    // TRANSCODE_ONLY исключается: такие MFT предназначены для
    // перекодирования файлов и для потоковой работы не годятся.
    let _ = MFT_ENUM_FLAG_TRANSCODE_ONLY;

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;

    // SAFETY: категория и флаги — константы; описания типов живут до
    // конца вызова; `activates` и `count` — валидные выходные
    // указатели. Массив, который MF аллоцирует, освобождается ниже
    // через CoTaskMemFree.
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

    if let Err(e) = hr {
        return Err(CodecError::Unavailable(format!("MFTEnumEx: {e}")));
    }

    if activates.is_null() || count == 0 {
        // Пустой список — не ошибка: у машины может не быть
        // аппаратного энкодера, и это законный ответ.
        if !activates.is_null() {
            // SAFETY: указатель получен от MF и не освобождён.
            unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates.cast())) };
        }
        return Ok(());
    }

    // SAFETY: MF заполнила `count` элементов по указателю `activates`.
    let items = unsafe { std::slice::from_raw_parts(activates, count as usize) };

    for item in items.iter().flatten() {
        // Имя не критично: MFT без имени всё равно существует, и
        // терять его из-за отсутствующего атрибута нельзя.
        //
        // Сигнатура сверена с исходниками windows 0.62 (§12): имя
        // отдаётся через ДВА out-параметра, а не возвращается
        // кортежем, как показалось по документации. Ровно та ошибка,
        // от которой предостерегает находка 6.
        let mut value = windows::core::PWSTR::null();
        let mut length: u32 = 0;
        // SAFETY: `item` жив; GUID — константа; оба out-указателя
        // валидны и живут до конца блока.
        let name = match unsafe {
            item.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut value, &mut length)
        } {
            Ok(()) if !value.is_null() => {
                // Строка читается до нулевого терминатора, а не по
                // `length`: разбор своими руками здесь ничего не
                // выигрывает, а ошибиться позволяет.
                //
                // `length` нужен самому вызову как out-параметр, но
                // результату он не нужен.
                let _ = length;
                // SAFETY: MF вернула валидную строку с нулевым
                // терминатором.
                let text = clean_vendor_name(&unsafe { value.to_string() }.unwrap_or_default());
                // SAFETY: строка выделена аллокатором MF — им же и
                // освобождается.
                unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(value.0.cast())) };
                text
            }
            _ => "(без имени)".to_string(),
        };

        out.push(EncoderEntry { name, hardware });
    }

    // Каждый IMFActivate освобождается своим Drop при выходе из
    // цикла (windows-rs держит их как COM-объекты), а сам массив —
    // аллокатор MF.
    // SAFETY: массив получен от MFTEnumEx и больше не используется.
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates.cast())) };

    Ok(())
}

/// Убрать мусорные символы из имени, зарегистрированного вендором.
///
/// # Что здесь чинится
///
/// Intel регистрирует свой энкодер под именем, в котором перед знаком
/// «®» стоит лишняя пара байт `D0 92` — символ «Ð’». Проверено по
/// байтам: правильное имя даёт `C2 AE`, а система отдаёт
/// `D0 92 C2 AE`. Сам «®» при этом целый, то есть это не ошибка
/// раскодировки у нас, а то, что записано в системе.
///
/// Две гипотезы до этого («консоль виновата», «длина строки не та»)
/// оказались неверны и были опровергнуты сравнением байт — тем же
/// приёмом, что закрыл вопрос с формой курсора (находка 45).
///
/// Имя показывается человеку на чужой машине, поэтому мусор из него
/// убирается. Логика от имени не зависит: оно нигде не сравнивается
/// и ни на что не влияет.
pub(super) fn clean_vendor_name(raw: &str) -> String {
    raw.replace('\u{412}', "").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_artifact_is_stripped() {
        // Ровно то, что отдаёт система на машине разработчика.
        assert_eq!(
            clean_vendor_name("IntelВ® Quick Sync Video H.264 Encoder MFT"),
            "Intel® Quick Sync Video H.264 Encoder MFT"
        );
    }

    #[test]
    fn clean_names_are_left_alone() {
        // Чистка не должна трогать нормальные имена — иначе она
        // тихо портила бы то, что и так верно.
        assert_eq!(
            clean_vendor_name("NVIDIA H.264 Encoder MFT"),
            "NVIDIA H.264 Encoder MFT"
        );
        assert_eq!(clean_vendor_name("H264 Encoder MFT"), "H264 Encoder MFT");
    }
}
