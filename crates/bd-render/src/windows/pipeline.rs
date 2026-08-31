//! Конверсия NV12 → RGB на GPU и вывод кадра.
//!
//! # Почему шейдер, а не копия
//!
//! Декодер отдаёт NV12-текстуру, дисплею нужен RGB. Преобразование на
//! CPU означало бы копию 3 МБ туда и обратно — 5–10 мс, треть бюджета
//! (CLAUDE.md §4.2.3). Шейдер делает то же самое за десятые доли
//! миллисекунды, не вынимая пиксели из GPU.
//!
//! # Как NV12 попадает в семплеры
//!
//! Это одна текстура с двумя плоскостями, но D3D11 умеет отдавать их
//! как два представления: `R8_UNORM` видит плоскость Y, `R8G8_UNORM` —
//! плоскость UV в половинном разрешении. Ни одного лишнего байта не
//! копируется, семплер сам интерполирует хроматический план.

use crate::{RenderError, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCOMPILE_WARNINGS_ARE_ERRORS,
};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_SRV_DIMENSION_TEXTURE2DARRAY};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11SamplerState,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD,
    D3D11_SAMPLER_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_TEX2D_ARRAY_SRV, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DYNAMIC,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM};

/// Исходник шейдера. Компилируется при старте, а не при сборке:
/// `fxc` не входит в обязательные требования к машине разработчика
/// (CLAUDE.md §10.1), а `d3dcompiler_47.dll` есть в любой Windows.
const SHADER_SOURCE: &str = include_str!("shader.hlsl");

/// Константы шейдера: доля кадра, занятая настоящей картинкой.
///
/// Раскладка обязана совпадать с `cbuffer Crop` в HLSL, а размер —
/// быть кратен 16 байтам (требование D3D11 к константным буферам).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CropConstants {
    uv_scale: [f32; 2],
    _padding: [f32; 2],
}

/// Конвейер вывода NV12-кадра.
pub struct NvToRgbPipeline {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
    /// Кеш представлений: одна пара (текстура, срез) — одна пара
    /// представлений.
    ///
    /// Декодер гоняет по кругу небольшой пул поверхностей (обычно
    /// 8 срезов одной текстуры), а `CreateShaderResourceView` —
    /// не бесплатный вызов. Создавать их заново на каждом кадре
    /// значило бы отдавать время впустую по 60 раз в секунду. Та же
    /// причина, по которой кешируются регистрации в NVENC.
    views: RefCell<HashMap<(usize, u32), PlaneViews>>,
}

/// Пара представлений на плоскости Y и UV одного среза.
#[derive(Clone)]
struct PlaneViews {
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
}

impl NvToRgbPipeline {
    /// Скомпилировать шейдеры и создать состояния.
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let vs_code = compile(SHADER_SOURCE, c"VsMain", c"vs_5_0")?;
        let ps_code = compile(SHADER_SOURCE, c"PsMain", c"ps_5_0")?;

        let mut vertex_shader = None;
        // SAFETY: устройство живо; байт-код получен от компилятора и
        // живёт до конца вызова; связывание классов не используется.
        unsafe { device.CreateVertexShader(blob_bytes(&vs_code), None, Some(&mut vertex_shader)) }
            .map_err(|e| RenderError::SwapChain(format!("CreateVertexShader: {e}")))?;

        let mut pixel_shader = None;
        // SAFETY: то же самое.
        unsafe { device.CreatePixelShader(blob_bytes(&ps_code), None, Some(&mut pixel_shader)) }
            .map_err(|e| RenderError::SwapChain(format!("CreatePixelShader: {e}")))?;

        // Линейная фильтрация: кадр может выводиться в окно другого
        // размера, и точечная выборка дала бы рваные края на тексте.
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        // SAFETY: устройство живо; описание живёт до конца вызова.
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler)) }
            .map_err(|e| RenderError::SwapChain(format!("CreateSamplerState: {e}")))?;

        // DYNAMIC + WRITE_DISCARD: буфер обновляется каждый кадр,
        // когда меняется разрешение потока.
        let buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<CropConstants>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut constants = None;
        // SAFETY: устройство живо; описание живёт до конца вызова;
        // начальные данные не нужны — буфер заполняется перед выводом.
        unsafe { device.CreateBuffer(&buffer_desc, None, Some(&mut constants)) }
            .map_err(|e| RenderError::SwapChain(format!("CreateBuffer: {e}")))?;

        tracing::debug!("конвейер NV12→RGB готов");

        Ok(Self {
            vertex_shader: vertex_shader
                .ok_or_else(|| RenderError::SwapChain("пустой вершинный шейдер".into()))?,
            pixel_shader: pixel_shader
                .ok_or_else(|| RenderError::SwapChain("пустой пиксельный шейдер".into()))?,
            sampler: sampler.ok_or_else(|| RenderError::SwapChain("пустой семплер".into()))?,
            constants: constants
                .ok_or_else(|| RenderError::SwapChain("пустой буфер констант".into()))?,
            views: RefCell::new(HashMap::new()),
        })
    }

    /// Представления плоскостей для среза, создавая их при первой встрече.
    fn views_for(
        &self,
        device: &ID3D11Device,
        texture: &ID3D11Texture2D,
        subresource: u32,
    ) -> Result<PlaneViews> {
        let key = (texture.as_raw() as usize, subresource);

        if let Some(cached) = self.views.borrow().get(&key) {
            return Ok(cached.clone());
        }

        let views = PlaneViews {
            luma: self.plane_view(device, texture, subresource, DXGI_FORMAT_R8_UNORM)?,
            chroma: self.plane_view(device, texture, subresource, DXGI_FORMAT_R8G8_UNORM)?,
        };

        let mut cache = self.views.borrow_mut();
        tracing::debug!(subresource, total = cache.len() + 1, "срез кеширован");
        cache.insert(key, views.clone());
        Ok(views)
    }

    /// Нарисовать кадр в текущую цель вывода.
    ///
    /// `subresource` — индекс среза в текстуре-массиве: декодер отдаёт
    /// кадры срезами одной поверхности, и игнорировать его нельзя,
    /// иначе на экране будет вечно нулевой слой (CLAUDE.md §0.1,
    /// находка 19).
    ///
    /// `visible` — настоящее разрешение картинки. Декодер выравнивает
    /// высоту до кратной 16, и лишние строки показывать нельзя
    /// (находка 18).
    pub fn draw(
        &self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        subresource: u32,
        visible: (u32, u32),
    ) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: текстура жива; GetDesc заполняет переданную структуру.
        unsafe { texture.GetDesc(&mut desc) };

        if desc.Width == 0 || desc.Height == 0 {
            return Err(RenderError::Platform {
                context: "текстура кадра нулевого размера",
                hresult: 0,
            });
        }

        let views = self.views_for(device, texture, subresource)?;

        self.update_constants(context, &desc, visible)?;

        // SAFETY: все ресурсы живы до конца вызова; слоты соответствуют
        // объявленным в шейдере регистрам t0/t1, s0 и b0.
        unsafe {
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(&self.pixel_shader, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(views.luma), Some(views.chroma)]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.IASetPrimitiveTopology(
                windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            );
            // Три вершины без буфера: шейдер строит треугольник по
            // SV_VertexID.
            context.Draw(3, 0);
        }

        Ok(())
    }

    /// Представление одной плоскости NV12.
    fn plane_view(
        &self,
        device: &ID3D11Device,
        texture: &ID3D11Texture2D,
        subresource: u32,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> Result<ID3D11ShaderResourceView> {
        // Представление на срез массива: декодер держит кадры в одной
        // текстуре-массиве, и нужный слой выбирается здесь.
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: subresource,
                    ArraySize: 1,
                },
            },
        };

        let mut view = None;
        // SAFETY: устройство и текстура живы; описание живёт до конца
        // вызова и ссылается на существующий срез.
        unsafe { device.CreateShaderResourceView(texture, Some(&desc), Some(&mut view)) }.map_err(
            |e| RenderError::Platform {
                context: "CreateShaderResourceView",
                hresult: e.code().0 as u32,
            },
        )?;

        view.ok_or(RenderError::Platform {
            context: "CreateShaderResourceView вернул пустое представление",
            hresult: 0,
        })
    }

    /// Записать в константный буфер долю кадра, занятую картинкой.
    fn update_constants(
        &self,
        context: &ID3D11DeviceContext,
        desc: &D3D11_TEXTURE2D_DESC,
        visible: (u32, u32),
    ) -> Result<()> {
        let scale = CropConstants {
            uv_scale: crop_scale((desc.Width, desc.Height), visible),
            _padding: [0.0; 2],
        };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: буфер создан как DYNAMIC с CPU_ACCESS_WRITE — только
        // такие допускают WRITE_DISCARD.
        unsafe {
            context.Map(
                &self.constants,
                0,
                D3D11_MAP_WRITE_DISCARD,
                0,
                Some(&mut mapped),
            )
        }
        .map_err(|e| RenderError::Platform {
            context: "Map(константы)",
            hresult: e.code().0 as u32,
        })?;

        // SAFETY: отображение действительно до Unmap; буфер создан
        // ровно под размер `CropConstants`, выравнивание соблюдено —
        // Map возвращает адрес, пригодный для любого типа.
        unsafe { std::ptr::write(mapped.pData as *mut CropConstants, scale) };

        // SAFETY: парный Unmap к успешному Map выше.
        unsafe { context.Unmap(&self.constants, 0) };
        Ok(())
    }
}

/// Скомпилировать шейдер из исходника.
fn compile(source: &str, entry: &CStr, target: &CStr) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;

    // SAFETY: исходник и строки живут до конца вызова; выходные
    // параметры — живые локальные переменные. Имя файла не нужно:
    // шейдер один и без директив include.
    let result = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            None,
            None,
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            // Предупреждения как ошибки: шейдер зашит в бинарь, и
            // «почти правильный» здесь не нужен.
            D3DCOMPILE_OPTIMIZATION_LEVEL3 | D3DCOMPILE_WARNINGS_ARE_ERRORS,
            0,
            &mut code,
            Some(&mut errors),
        )
    };

    if let Err(e) = result {
        // Сообщение компилятора важнее HRESULT: оно указывает строку.
        let details = errors
            .as_ref()
            .map(|blob| String::from_utf8_lossy(blob_bytes(blob)).into_owned())
            .unwrap_or_else(|| e.to_string());
        return Err(RenderError::ShaderCompilation(details));
    }

    code.ok_or_else(|| RenderError::ShaderCompilation("компилятор вернул пустой байт-код".into()))
}

/// Доля текстуры, занятая настоящей картинкой.
///
/// Декодер выравнивает размеры до кратных 16 (1080 → 1088), и лишние
/// строки — служебное дополнение, которое показывать нельзя
/// (CLAUDE.md §0.1, находка 18).
///
/// Вынесено отдельной функцией ради теста: ошибка здесь не даёт ни
/// ошибки компиляции, ни ошибки вызова — только чуть растянутую
/// картинку, которую легко не заметить глазом.
fn crop_scale(texture: (u32, u32), visible: (u32, u32)) -> [f32; 2] {
    let axis = |visible: u32, full: u32| {
        if full == 0 {
            return 1.0;
        }
        // Картинка не может быть больше текстуры: при рассинхроне
        // размеров лучше показать всё, чем выйти за пределы.
        (visible.min(full) as f32) / full as f32
    };

    [axis(visible.0, texture.0), axis(visible.1, texture.1)]
}

/// Байты блоба.
fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: блоб жив; указатель и размер он же и сообщает, а
    // время жизни среза связано с временем жизни блоба сигнатурой.
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_hlsl_layout() {
        // Раскладка обязана совпадать с cbuffer Crop, а размер —
        // быть кратен 16 байтам (требование D3D11). Ошибка здесь
        // не даёт ошибки компиляции — только съехавшую картинку.
        assert_eq!(std::mem::size_of::<CropConstants>(), 16);
    }

    #[test]
    fn crop_cuts_decoder_padding() {
        // Реальный случай 1080p: декодер выравнивает высоту до 1088,
        // и нижние 8 строк показывать нельзя.
        let scale = crop_scale((1920, 1088), (1920, 1080));
        assert_eq!(scale[0], 1.0, "ширина совпадает, обрезать нечего");
        assert!(
            (scale[1] - 1080.0 / 1088.0).abs() < 1e-6,
            "получено {}",
            scale[1]
        );
    }

    #[test]
    fn crop_is_identity_without_padding() {
        // 720p делится на 16 нацело — дополнения нет.
        assert_eq!(crop_scale((1280, 720), (1280, 720)), [1.0, 1.0]);
    }

    #[test]
    fn crop_never_exceeds_texture() {
        // Рассинхрон размеров не должен выводить выборку за пределы
        // текстуры: там мусор соседнего кадра из пула.
        let scale = crop_scale((1920, 1088), (2560, 1440));
        assert_eq!(scale, [1.0, 1.0]);
    }

    #[test]
    fn crop_survives_zero_sized_texture() {
        // Деления на ноль быть не должно: при смене разрешения
        // описание может прийти вырожденным.
        assert_eq!(crop_scale((0, 0), (1920, 1080)), [1.0, 1.0]);
    }
}
