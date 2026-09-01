//! Помехоустойчивое кодирование видео: Reed-Solomon поверх фрагментов.
//!
//! # Зачем это обязательно, а не желательно
//!
//! Кадр не переспрашивается: ретрансмит устаревшего кадра хуже его
//! потери (CLAUDE.md §5.3). Значит потеря **одного** датаграма убивает
//! **весь** кадр — и арифметика здесь беспощадна. Кадр 1080p идёт
//! ~22 фрагментами, ключевой при 2560x1600 — 63:
//!
//! | фрагментов | 1 % потерь | 3 % потерь |
//! |---|---|---|
//! | 22 | 19.8 % кадров | 48.8 % |
//! | 63 | 46.9 % кадров | 85.3 % |
//!
//! Это не гипотеза: замер `--link mobile` потерял 663 кадра из 1290,
//! то есть 51 % при предсказанных 48 % (находка 28). Без FEC мобильный
//! интернет даёт не «слегка хуже картинку», а неработоспособную
//! сессию.
//!
//! # Почему Reed-Solomon, а не XOR
//!
//! XOR по группам заманчив: он тривиален, не тянет зависимость и
//! чинит одну потерю в группе. Арифметика опровергла его **до**
//! написания кода — тот же приём, что в находках 41 и 44:
//!
//! | фрагментов | без FEC | XOR по 4 | RS +20 % |
//! |---|---|---|---|
//! | 22 | 48.8 % | 4.4 % | 0.1 % |
//! | 63 | 85.3 % | **12.4 %** | 0.0 % |
//!
//! При одинаковых накладных (25 %) XOR укладывается в критерий этапа 6
//! (≤ 5 % потерь кадров при 3 % потерь датаграмов) на обычных кадрах и
//! **проваливает** его на крупных: 12.4 % против цели 5 %. Причина в
//! том, что XOR чинит ровно одну потерю в группе, а при 16 группах
//! шанс словить две в одной группе велик. Reed-Solomon тратит ту же
//! избыточность на кадр целиком, поэтому переживает любые k потерь.
//!
//! Крупные кадры — это ровно ключевые, то есть те, чья потеря стоит
//! дороже всего: без ключевого декодер не начнёт, и находка 53
//! измерила цену — p95 восстановления 767 мс при пороге 200 мс.
//!
//! # Устройство
//!
//! Паритетные фрагменты едут **обычными датаграмами** того же формата,
//! с индексами `data_count..count`. Число паритетных лежит во флагах
//! заголовка. Это даёт два свойства:
//!
//! - инвариант `index < count` остаётся жёстким, и разбор недоверенных
//!   данных не ослабляется (§4.3.5);
//! - сторона без поддержки FEC отбросит паритет как обычный лишний
//!   фрагмент, а не сломается на неизвестном виде пакета.

use crate::error::{Result, TransportError};

/// Максимум паритетных фрагментов на кадр.
///
/// Ограничение снизу диктует смысл: больше данных чинить нечем.
/// Ограничение сверху — защита от недоверенного заголовка, где число
/// паритетных приходит из сети.
pub const MAX_PARITY: usize = 255;

/// Доля избыточности по умолчанию, в процентах от числа фрагментов.
///
/// 20 % выбраны не на глаз, а по таблице выше: при 3 % потерь они
/// дают ≤ 0.1 % потерянных кадров на любом размере кадра, то есть
/// с запасом к критерию этапа 6 (≤ 5 %). Меньше — 10 % — уже даёт
/// 3.4 % на 22 фрагментах, что укладывается, но без запаса на
/// всплески потерь.
///
/// Цена прямая и понятная: +20 % трафика. Она платится всегда, а
/// не только при потерях, — поэтому FEC включается по состоянию
/// канала, а не по умолчанию (см. [`FecPolicy`]).
pub const DEFAULT_REDUNDANCY_PERCENT: u32 = 20;

/// Сколько паритетных фрагментов нужно кадру из `data_count` штук.
///
/// Всегда хотя бы один, пока избыточность включена: кадр из одного
/// фрагмента без паритета не защищён вовсе, а его потеря стоит
/// столько же, сколько потеря большого кадра.
pub fn parity_count(data_count: usize, redundancy_percent: u32) -> usize {
    if data_count == 0 || redundancy_percent == 0 {
        return 0;
    }
    let raw = data_count * redundancy_percent as usize;
    let parity = raw.div_ceil(100).max(1);
    parity.min(MAX_PARITY).min(data_count)
}

/// Политика применения FEC.
///
/// # Почему не «включить и забыть»
///
/// Избыточность стоит трафика всегда, а спасает только при потерях.
/// На чистом канале 20 % — это 20 % битрейта, отданных ни за что, и
/// при CBR они отнимаются у картинки: энкодеру достаётся меньше бит,
/// то есть FEC на идеальном канале **ухудшает** изображение.
///
/// Это тот же размен, что у джиттер-буфера (находка 42), и решается
/// так же: по умолчанию выключено, включается по измеренному
/// состоянию канала.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecPolicy {
    /// Не добавлять паритет вовсе.
    Off,
    /// Постоянная избыточность в процентах.
    Fixed(u32),
}

impl FecPolicy {
    /// Сколько процентов избыточности давать сейчас.
    pub fn redundancy_percent(self) -> u32 {
        match self {
            FecPolicy::Off => 0,
            FecPolicy::Fixed(p) => p,
        }
    }

    /// Включён ли FEC.
    pub fn is_on(self) -> bool {
        self.redundancy_percent() > 0
    }
}

/// Закодировать паритетные фрагменты для кадра.
///
/// `shards` — данные, уже нарезанные на равные куски. Reed-Solomon
/// требует одинаковой длины: короткий хвост дополняется нулями
/// вызывающим, а настоящая длина кадра известна из заголовка.
///
/// Возвращает `parity` векторов той же длины, что и входные.
pub fn encode_parity(shards: &[Vec<u8>], parity: usize) -> Result<Vec<Vec<u8>>> {
    if parity == 0 || shards.is_empty() {
        return Ok(Vec::new());
    }
    let shard_len = shards[0].len();
    if shard_len == 0 || !shard_len.is_multiple_of(2) {
        // Требование библиотеки: длина куска чётная. Нечётную мы
        // никогда не подаём (куски выравниваются вызывающим), но
        // проверка здесь дешевле, чем паника внутри библиотеки.
        return Err(TransportError::Malformed(
            "длина куска FEC должна быть чётной",
        ));
    }
    if shards.iter().any(|s| s.len() != shard_len) {
        return Err(TransportError::Malformed("куски FEC разной длины"));
    }

    let encoded = reed_solomon_simd::encode(shards.len(), parity, shards)
        .map_err(|_| TransportError::Malformed("FEC: кодирование не удалось"))?;
    Ok(encoded)
}

/// Восстановить недостающие фрагменты данных.
///
/// `data` и `parity` — слоты: `None` там, где фрагмент не пришёл.
/// Возвращает `Ok(None)`, если восстановление невозможно или не нужно.
/// Невозможность — не ошибка, а обычный исход при сильных потерях.
pub fn recover(
    data: &[Option<Vec<u8>>],
    parity: &[Option<Vec<u8>>],
) -> Result<Option<Vec<Vec<u8>>>> {
    let data_count = data.len();
    let parity_len = parity.len();
    if data_count == 0 || parity_len == 0 {
        return Ok(None);
    }

    let present_data: Vec<(usize, &[u8])> = data
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().map(|v| (i, v.as_slice())))
        .collect();
    let present_parity: Vec<(usize, &[u8])> = parity
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().map(|v| (i, v.as_slice())))
        .collect();

    // Всё на месте — чинить нечего. Проверка первой: восстановление
    // стоит процессора, и платить его на каждом целом кадре незачем.
    if present_data.len() == data_count {
        return Ok(None);
    }

    // Reed-Solomon чинит стирания, пока суммарно пришло не меньше
    // кусков, чем было данных. Меньше — арифметически невозможно, и
    // это штатный исход, а не сбой.
    if present_data.len() + present_parity.len() < data_count {
        return Ok(None);
    }

    let restored = reed_solomon_simd::decode(
        data_count,
        parity_len,
        present_data.iter().copied(),
        present_parity.iter().copied(),
    )
    .map_err(|_| TransportError::Malformed("FEC: восстановление не удалось"))?;

    let mut out = Vec::with_capacity(data_count);
    for (i, slot) in data.iter().enumerate() {
        match slot {
            Some(v) => out.push(v.clone()),
            None => match restored.get(&i) {
                Some(v) => out.push(v.clone()),
                // Библиотека не вернула кусок, который мы ждали:
                // считаем восстановление несостоявшимся, а не отдаём
                // наверх дыру. Тихая дыра в H.264 — это мусор на
                // экране без единой ошибки.
                None => return Ok(None),
            },
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Нарезать данные на `n` кусков одинаковой чётной длины.
    fn split(data: &[u8], n: usize) -> Vec<Vec<u8>> {
        let len = data.len().div_ceil(n);
        let len = if len.is_multiple_of(2) { len } else { len + 1 };
        (0..n)
            .map(|i| {
                let start = (i * len).min(data.len());
                let end = (start + len).min(data.len());
                let mut v = data[start..end].to_vec();
                v.resize(len, 0);
                v
            })
            .collect()
    }

    #[test]
    fn parity_count_scales_and_never_zero_when_on() {
        assert_eq!(parity_count(10, 20), 2);
        assert_eq!(parity_count(63, 20), 13);
        // Хотя бы один паритетный: кадр из одного фрагмента иначе
        // остался бы без защиты вовсе.
        assert_eq!(parity_count(1, 20), 1);
        assert_eq!(parity_count(3, 1), 1);
        // Ноль данных — нечего защищать.
        assert_eq!(parity_count(0, 20), 0);
        // Выключенная избыточность не даёт паритета: иначе `Off`
        // всё равно стоил бы трафика.
        assert_eq!(parity_count(10, 0), 0);
    }

    #[test]
    fn parity_never_exceeds_data() {
        // Больше паритета, чем данных, бессмысленно: избыточность
        // выше 100 % не добавляет стойкости, только трафик.
        assert!(parity_count(4, 500) <= 4);
    }

    #[test]
    fn recovers_single_loss() {
        let data = b"BetterDesk frame payload, long enough to split".to_vec();
        let shards = split(&data, 4);
        let parity = encode_parity(&shards, 2).unwrap();

        let mut slots: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
        slots[2] = None;
        let par: Vec<Option<Vec<u8>>> = parity.iter().cloned().map(Some).collect();

        let restored = recover(&slots, &par)
            .unwrap()
            .expect("должно восстановиться");
        assert_eq!(restored, shards);
    }

    #[test]
    fn recovers_up_to_parity_count() {
        let data: Vec<u8> = (0..400u32).map(|i| (i % 251) as u8).collect();
        let shards = split(&data, 8);
        let parity = encode_parity(&shards, 3).unwrap();

        // Теряем ровно столько, сколько паритета — граница возможного.
        let mut slots: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
        slots[0] = None;
        slots[4] = None;
        slots[7] = None;
        let par: Vec<Option<Vec<u8>>> = parity.iter().cloned().map(Some).collect();

        let restored = recover(&slots, &par)
            .unwrap()
            .expect("три потери при трёх паритетных");
        assert_eq!(restored, shards);
    }

    #[test]
    fn gives_up_beyond_parity_count() {
        // Проверка обязана уметь отвечать «нет»: FEC, который всегда
        // «восстанавливает», тихо отдал бы наверх мусор (находка 4).
        let data: Vec<u8> = (0..400u32).map(|i| (i % 251) as u8).collect();
        let shards = split(&data, 8);
        let parity = encode_parity(&shards, 2).unwrap();

        let mut slots: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
        slots[0] = None;
        slots[3] = None;
        slots[6] = None; // три потери при двух паритетных
        let par: Vec<Option<Vec<u8>>> = parity.iter().cloned().map(Some).collect();

        assert!(recover(&slots, &par).unwrap().is_none());
    }

    #[test]
    fn recovers_when_parity_itself_is_lost() {
        // Паритет теряется наравне с данными — он едет теми же
        // датаграмами. Схема обязана это переживать.
        let data: Vec<u8> = (0..300u32).map(|i| (i % 97) as u8).collect();
        let shards = split(&data, 6);
        let parity = encode_parity(&shards, 3).unwrap();

        let mut slots: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
        slots[1] = None;
        let mut par: Vec<Option<Vec<u8>>> = parity.iter().cloned().map(Some).collect();
        par[0] = None;
        par[2] = None;

        let restored = recover(&slots, &par)
            .unwrap()
            .expect("одна потеря, один живой паритет");
        assert_eq!(restored, shards);
    }

    #[test]
    fn nothing_to_do_when_all_present() {
        let shards = split(b"complete frame data here", 4);
        let parity = encode_parity(&shards, 2).unwrap();
        let slots: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
        let par: Vec<Option<Vec<u8>>> = parity.iter().cloned().map(Some).collect();
        // Целый кадр не требует работы — и не должен её делать:
        // восстановление стоит процессора на каждом кадре.
        assert!(recover(&slots, &par).unwrap().is_none());
    }

    #[test]
    fn rejects_uneven_shards() {
        let bad = vec![vec![0u8; 4], vec![0u8; 6]];
        assert!(encode_parity(&bad, 1).is_err());
    }

    #[test]
    fn policy_off_means_no_overhead() {
        assert!(!FecPolicy::Off.is_on());
        assert_eq!(FecPolicy::Off.redundancy_percent(), 0);
        assert!(FecPolicy::Fixed(DEFAULT_REDUNDANCY_PERCENT).is_on());
    }
}
