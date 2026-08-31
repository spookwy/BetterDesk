//! Курсор: позиция и форма.
//!
//! # Почему курсор передаётся отдельно от кадра
//!
//! Курсор **не** должен быть частью видеопотока, хотя технически он
//! туда влезает. Причина — задержка: картинка идёт через энкодер,
//! транспорт и декодер, и в LAN это десятки миллисекунд. Курсор,
//! вмороженный в кадр, отстаёт ровно на эту величину, и человек видит,
//! как стрелка тянется за его рукой.
//!
//! Отдельный канал решает это: позиция курсора — 5 байт, она успевает
//! прийти раньше кадра, и клиент рисует стрелку там, где она есть
//! **сейчас**, поверх картинки, которая слегка устарела. Так делают
//! Parsec и Moonlight, и это заметная часть ощущения «работаю за тем
//! компьютером» (docs/roadmap.md, этап 2).
//!
//! # Почему форма отдельно от позиции
//!
//! Позиция меняется каждым движением мыши — сотни раз в секунду.
//! Форма меняется редко: стрелка → текстовый курсор → рука над
//! ссылкой. Форма при этом весит килобайты, позиция — байты.
//!
//! Слать их вместе значило бы гнать килобайты на каждое движение.
//! Поэтому форма нумеруется и шлётся только при смене, а позиция
//! ссылается на неё номером.

use crate::input::MousePosition;

/// Позиция курсора на экране хоста.
///
/// Отдельный тип, а не просто [`MousePosition`]: курсор может быть
/// скрыт (полноэкранная игра, ввод текста), и это состояние надо
/// передавать — иначе клиент нарисует стрелку там, где её нет.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorPosition {
    /// Где курсор в долях экрана.
    pub position: MousePosition,
    /// Виден ли курсор сейчас.
    pub visible: bool,
    /// Номер формы, которую надо рисовать.
    ///
    /// Ссылка, а не сама форма: формы весят килобайты и меняются
    /// редко. Клиент держит последнюю известную и перерисовывает её
    /// на каждой новой позиции.
    pub shape_id: u32,
}

/// Размер [`CursorPosition`] в wire-формате.
const POSITION_SIZE: usize = 4 + 4 + 1;

impl CursorPosition {
    /// Записать в wire-формате.
    ///
    /// Девять байт: две координаты `u16`, номер формы `u32`, флаг
    /// видимости. Помещается в один датаграм с огромным запасом,
    /// поэтому фрагментация к позиции курсора не применяется никогда.
    pub fn encode(&self) -> [u8; POSITION_SIZE] {
        let mut out = [0u8; POSITION_SIZE];
        let x = (self.position.x() * f32::from(u16::MAX)) as u16;
        let y = (self.position.y() * f32::from(u16::MAX)) as u16;
        out[0..2].copy_from_slice(&x.to_le_bytes());
        out[2..4].copy_from_slice(&y.to_le_bytes());
        out[4..8].copy_from_slice(&self.shape_id.to_le_bytes());
        out[8] = u8::from(self.visible);
        out
    }

    /// Разобрать из wire-формата. Недоверенные данные (CLAUDE.md §8.5).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != POSITION_SIZE {
            return None;
        }
        let x = u16::from_le_bytes([bytes[0], bytes[1]]);
        let y = u16::from_le_bytes([bytes[2], bytes[3]]);
        Some(Self {
            position: MousePosition::new(
                f32::from(x) / f32::from(u16::MAX),
                f32::from(y) / f32::from(u16::MAX),
            ),
            shape_id: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            visible: bytes[8] != 0,
        })
    }

    /// Размер в wire-формате.
    pub const fn wire_size() -> usize {
        POSITION_SIZE
    }
}

/// Как закодированы пиксели формы курсора.
///
/// Windows отдаёт курсоры в трёх видах, и каждый требует своей
/// отрисовки. Свести их к одному на хосте можно, но это была бы
/// работа на каждой смене формы; проще передать вид как есть.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShapeKind {
    /// Монохромный: две склеенные маски 1 бит/пиксель — AND и XOR.
    ///
    /// Высота буфера **вдвое больше** высоты курсора: сначала маска
    /// AND, затем XOR. Это классический курсор Windows, которым
    /// рисуется в том числе мигающий текстовый штрих.
    Monochrome,
    /// Цветной BGRA с альфа-каналом. Обычный современный курсор.
    Color,
    /// Цветной с маскированной прозрачностью.
    ///
    /// Альфа-канал здесь означает не прозрачность, а инверсию: где
    /// байт альфы ненулевой, пиксель инвертирует фон. Так сделан
    /// курсор «I-beam» поверх произвольного фона.
    MaskedColor,
}

impl CursorShapeKind {
    /// Код вида в wire-формате.
    const fn code(self) -> u8 {
        match self {
            CursorShapeKind::Monochrome => 1,
            CursorShapeKind::Color => 2,
            CursorShapeKind::MaskedColor => 3,
        }
    }

    /// Разобрать код вида.
    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(CursorShapeKind::Monochrome),
            2 => Some(CursorShapeKind::Color),
            3 => Some(CursorShapeKind::MaskedColor),
            _ => None,
        }
    }
}

/// Наибольший допустимый размер стороны курсора в пикселях.
///
/// Windows не создаёт курсоров больше 256×256, но размер приходит
/// **из сети**, и доверять ему нельзя: без предела заявленные
/// 65535×65535 привели бы к попытке выделить 17 ГБ.
pub const MAX_CURSOR_DIMENSION: u32 = 256;

/// Форма курсора: пиксели и точка привязки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    /// Номер формы. Позиция ссылается на неё этим номером.
    pub id: u32,
    /// Как закодированы пиксели.
    pub kind: CursorShapeKind,
    /// Ширина в пикселях.
    pub width: u32,
    /// Высота в пикселях.
    ///
    /// Для [`CursorShapeKind::Monochrome`] это высота **курсора**,
    /// а буфер вдвое выше: в нём две маски подряд.
    pub height: u32,
    /// Байт на строку в буфере пикселей.
    pub pitch: u32,
    /// Точка привязки: где именно внутри картинки «остриё» курсора.
    ///
    /// Без неё стрелка рисуется со смещением, а крестик прицела —
    /// углом вместо центра.
    pub hotspot_x: u32,
    /// Точка привязки по вертикали.
    pub hotspot_y: u32,
    /// Пиксели в формате, заданном [`Self::kind`].
    pub pixels: Vec<u8>,
}

/// Размер заголовка формы в wire-формате.
const SHAPE_HEADER_SIZE: usize = 4 + 1 + 4 + 4 + 4 + 4 + 4 + 4;

impl CursorShape {
    /// Сколько байт пикселей требует эта форма.
    ///
    /// Считается по объявленным размерам, а не по длине `pixels`:
    /// именно так проверяется, что пришедший из сети буфер
    /// соответствует заявленной геометрии.
    pub const fn expected_pixel_bytes(&self) -> usize {
        let rows = match self.kind {
            // Две маски подряд: AND и XOR.
            CursorShapeKind::Monochrome => self.height * 2,
            _ => self.height,
        };
        (self.pitch as usize) * (rows as usize)
    }

    /// Согласованы ли поля между собой.
    ///
    /// Проверяет то, что нельзя проверить типами: размеры в пределах
    /// разумного, длина буфера соответствует геометрии, точка
    /// привязки внутри картинки.
    pub fn is_consistent(&self) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }
        if self.width > MAX_CURSOR_DIMENSION || self.height > MAX_CURSOR_DIMENSION {
            return false;
        }
        // Точка привязки вне картинки означала бы отрисовку со
        // смещением в неизвестную сторону.
        if self.hotspot_x >= self.width || self.hotspot_y >= self.height {
            return false;
        }
        // Строка не может быть уже, чем требует ширина.
        let min_pitch = match self.kind {
            // 1 бит на пиксель, округление вверх до байта.
            CursorShapeKind::Monochrome => self.width.div_ceil(8),
            _ => self.width * 4,
        };
        if self.pitch < min_pitch {
            return false;
        }
        self.pixels.len() == self.expected_pixel_bytes()
    }

    /// Записать в wire-формате.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SHAPE_HEADER_SIZE + self.pixels.len());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.push(self.kind.code());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.pitch.to_le_bytes());
        out.extend_from_slice(&self.hotspot_x.to_le_bytes());
        out.extend_from_slice(&self.hotspot_y.to_le_bytes());
        out.extend_from_slice(&(self.pixels.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.pixels);
        out
    }

    /// Разобрать из wire-формата.
    ///
    /// **Парсер недоверенных данных** (CLAUDE.md §8.5). Размеры и
    /// длина буфера приходят от постороннего, поэтому проверяется
    /// всё: заявленная длина против фактической, геометрия против
    /// длины, размеры против предела. Несогласованность даёт `None`.
    ///
    /// Именно здесь предотвращается выделение памяти по чужому
    /// числу: буфер не резервируется под заявленный размер, а
    /// нарезается из того, что реально пришло.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < SHAPE_HEADER_SIZE {
            return None;
        }

        let id = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        let kind = CursorShapeKind::from_code(bytes[4])?;
        let width = u32::from_le_bytes(bytes[5..9].try_into().ok()?);
        let height = u32::from_le_bytes(bytes[9..13].try_into().ok()?);
        let pitch = u32::from_le_bytes(bytes[13..17].try_into().ok()?);
        let hotspot_x = u32::from_le_bytes(bytes[17..21].try_into().ok()?);
        let hotspot_y = u32::from_le_bytes(bytes[21..25].try_into().ok()?);
        let pixel_len = u32::from_le_bytes(bytes[25..29].try_into().ok()?) as usize;

        // Заявленная длина обязана совпасть с фактической. Больше —
        // пакет обрезан; меньше — в хвосте мусор, и это не наш формат.
        if bytes.len() != SHAPE_HEADER_SIZE + pixel_len {
            return None;
        }

        let shape = Self {
            id,
            kind,
            width,
            height,
            pitch,
            hotspot_x,
            hotspot_y,
            pixels: bytes[SHAPE_HEADER_SIZE..].to_vec(),
        };

        // Согласованность проверяется после сборки: так правило
        // задано в одном месте и одинаково для приёма и для отправки.
        if !shape.is_consistent() {
            return None;
        }
        Some(shape)
    }
}

/// Хранилище форм курсора на стороне клиента.
///
/// Держит последние известные формы, чтобы позиция могла ссылаться на
/// них номером. Размер ограничен: формы приходят из сети, и без
/// предела сторона, шлющая новую форму на каждый кадр, исчерпала бы
/// память.
#[derive(Debug, Default)]
pub struct CursorShapeCache {
    shapes: Vec<CursorShape>,
}

/// Сколько форм помнить.
///
/// Реально их единицы: стрелка, текстовый курсор, рука, песочные
/// часы, стрелки изменения размера. Восьми хватает с запасом, а
/// вытеснение самой старой безвредно — хост пришлёт форму снова.
const MAX_CACHED_SHAPES: usize = 8;

impl CursorShapeCache {
    /// Пустое хранилище.
    pub fn new() -> Self {
        Self::default()
    }

    /// Запомнить форму.
    ///
    /// Форма с уже известным номером заменяет прежнюю: номера
    /// назначает хост, и повтор номера означает, что форма
    /// изменилась.
    pub fn insert(&mut self, shape: CursorShape) {
        if let Some(existing) = self.shapes.iter_mut().find(|s| s.id == shape.id) {
            *existing = shape;
            return;
        }
        if self.shapes.len() >= MAX_CACHED_SHAPES {
            self.shapes.remove(0);
        }
        self.shapes.push(shape);
    }

    /// Найти форму по номеру.
    ///
    /// `None` означает, что форма ещё не дошла — обычная ситуация
    /// в начале сессии и после потери пакета. Клиент в этом случае
    /// рисует свой курсор или ничего, но **не** ждёт: ожидание формы
    /// заморозило бы курсор на месте.
    pub fn get(&self, id: u32) -> Option<&CursorShape> {
        self.shapes.iter().find(|s| s.id == id)
    }

    /// Сколько форм запомнено.
    pub fn len(&self) -> usize {
        self.shapes.len()
    }

    /// Пусто ли хранилище.
    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color_shape(id: u32) -> CursorShape {
        let (w, h) = (4u32, 4u32);
        CursorShape {
            id,
            kind: CursorShapeKind::Color,
            width: w,
            height: h,
            pitch: w * 4,
            hotspot_x: 1,
            hotspot_y: 1,
            pixels: vec![0xAB; (w * 4 * h) as usize],
        }
    }

    fn mono_shape(id: u32) -> CursorShape {
        let (w, h) = (8u32, 8u32);
        let pitch = w.div_ceil(8);
        CursorShape {
            id,
            kind: CursorShapeKind::Monochrome,
            width: w,
            height: h,
            pitch,
            hotspot_x: 0,
            hotspot_y: 0,
            // Две маски подряд: AND и XOR.
            pixels: vec![0xFF; (pitch * h * 2) as usize],
        }
    }

    #[test]
    fn position_survives_roundtrip() {
        let p = CursorPosition {
            position: MousePosition::new(0.25, 0.75),
            visible: true,
            shape_id: 7,
        };
        let back = CursorPosition::parse(&p.encode()).expect("разбор своего формата");
        assert_eq!(back.shape_id, 7);
        assert!(back.visible);
        // Координаты кодируются в u16 — точность 1/65536 (находка 35).
        assert!((back.position.x() - 0.25).abs() < 1e-4);
        assert!((back.position.y() - 0.75).abs() < 1e-4);
    }

    #[test]
    fn hidden_cursor_stays_hidden() {
        // Флаг видимости важен не меньше координат: без него клиент
        // нарисует стрелку поверх полноэкранной игры, где её нет.
        let p = CursorPosition {
            position: MousePosition::new(0.5, 0.5),
            visible: false,
            shape_id: 0,
        };
        let back = CursorPosition::parse(&p.encode()).unwrap();
        assert!(!back.visible);
    }

    #[test]
    fn position_rejects_wrong_length() {
        assert!(CursorPosition::parse(&[]).is_none());
        assert!(CursorPosition::parse(&[0u8; POSITION_SIZE - 1]).is_none());
        assert!(CursorPosition::parse(&[0u8; POSITION_SIZE + 1]).is_none());
    }

    #[test]
    fn color_shape_survives_roundtrip() {
        let shape = color_shape(3);
        let back = CursorShape::parse(&shape.encode()).expect("разбор своего формата");
        assert_eq!(back, shape);
    }

    #[test]
    fn monochrome_shape_expects_double_height_buffer() {
        // Монохромный курсор несёт две маски подряд. Посчитать высоту
        // как обычную — значит принять буфер вдвое короче нужного и
        // прочитать мусор при отрисовке.
        let shape = mono_shape(1);
        assert_eq!(
            shape.expected_pixel_bytes(),
            (shape.pitch * shape.height * 2) as usize
        );
        assert!(shape.is_consistent());

        let back = CursorShape::parse(&shape.encode()).unwrap();
        assert_eq!(back, shape);
    }

    #[test]
    fn shape_with_short_buffer_is_rejected() {
        // Главная защита разбора: заявленная геометрия против
        // фактической длины. Без неё отрисовка вышла бы за буфер.
        let mut shape = color_shape(1);
        shape.pixels.truncate(shape.pixels.len() - 4);
        assert!(!shape.is_consistent());
        assert!(CursorShape::parse(&shape.encode()).is_none());
    }

    #[test]
    fn oversized_shape_is_rejected() {
        // Размер приходит из сети. Без предела заявленные 65535×65535
        // означали бы попытку выделить 17 ГБ.
        let mut shape = color_shape(1);
        shape.width = 100_000;
        shape.height = 100_000;
        assert!(!shape.is_consistent());
    }

    #[test]
    fn zero_sized_shape_is_rejected() {
        let mut shape = color_shape(1);
        shape.width = 0;
        assert!(!shape.is_consistent());
    }

    #[test]
    fn hotspot_outside_image_is_rejected() {
        // Точка привязки вне картинки дала бы отрисовку со смещением
        // в неизвестную сторону.
        let mut shape = color_shape(1);
        shape.hotspot_x = shape.width;
        assert!(!shape.is_consistent());
    }

    #[test]
    fn pitch_smaller_than_width_is_rejected() {
        // Строка уже, чем требует ширина, — верный признак либо
        // подделки, либо расхождения в понимании формата.
        let mut shape = color_shape(1);
        shape.pitch = 4;
        assert!(!shape.is_consistent());
    }

    #[test]
    fn truncated_shape_packet_is_rejected() {
        let shape = color_shape(1);
        let encoded = shape.encode();
        for len in 0..encoded.len() {
            assert!(
                CursorShape::parse(&encoded[..len]).is_none(),
                "обрезанный до {len} байт пакет принят"
            );
        }
    }

    #[test]
    fn shape_parser_never_panics_on_arbitrary_bytes() {
        // Дешёвая замена фаззингу до появления cargo-fuzz (§10.3).
        for filler in [0u8, 1, 0x7F, 0x80, 0xFF] {
            for len in 0..80 {
                let bytes = vec![filler; len];
                let _ = CursorShape::parse(&bytes);
                let _ = CursorPosition::parse(&bytes);
            }
        }
    }

    #[test]
    fn unknown_shape_kind_is_rejected() {
        let mut encoded = color_shape(1).encode();
        encoded[4] = 99;
        assert!(CursorShape::parse(&encoded).is_none());
    }

    #[test]
    fn cache_replaces_shape_with_same_id() {
        // Хост переиспользует номера: тот же номер означает, что
        // форма изменилась, а не что пришёл дубликат.
        let mut cache = CursorShapeCache::new();
        cache.insert(color_shape(1));
        let mut updated = color_shape(1);
        updated.pixels = vec![0x11; updated.pixels.len()];
        cache.insert(updated.clone());

        assert_eq!(cache.len(), 1, "номер должен заменять, а не добавлять");
        assert_eq!(cache.get(1), Some(&updated));
    }

    #[test]
    fn cache_evicts_oldest_when_full() {
        // Формы приходят из сети: сторона, шлющая новую на каждый
        // кадр, не должна исчерпать память.
        let mut cache = CursorShapeCache::new();
        for id in 0..MAX_CACHED_SHAPES as u32 + 4 {
            cache.insert(color_shape(id));
        }
        assert_eq!(cache.len(), MAX_CACHED_SHAPES);
        // Самые старые вытеснены, свежие на месте.
        assert!(cache.get(0).is_none());
        assert!(cache.get(MAX_CACHED_SHAPES as u32 + 3).is_some());
    }

    #[test]
    fn missing_shape_is_reported_not_faked() {
        // Клиент не должен ждать формы: ожидание заморозило бы
        // курсор. `None` — это сигнал рисовать своё.
        let cache = CursorShapeCache::new();
        assert!(cache.get(42).is_none());
        assert!(cache.is_empty());
    }
}
