//! Уменьшение кадра на GPU перед кодированием.
//!
//! # Зачем уменьшать картинку
//!
//! Снижение битрейта помогает не бесконечно. Кадр 1080p при 1 Мбит/с
//! — это около 2 КБ на кадр при 60 fps, и энкодер вынужден огрублять
//! так, что текст перестаёт читаться. Дальше правильный ответ не
//! «сжимать сильнее», а **уменьшить картинку**: та же полоса,
//! поделённая на вчетверо меньше пикселей, даёт вчетверо больше бит
//! на каждый.
//!
//! Решение о том, когда это делать, принимает не этот модуль, а
//! [`bd_core::RateController`] — здесь только исполнение.
//!
//! # Почему шейдер, а не копия на CPU
//!
//! Кадр живёт в GPU-текстуре, и вынимать его в системную память
//! ради `resize` значило бы платить 5–10 мс на кадр — прямой запрет
//! §4.2.3. Проход шейдера стоит десятые доли миллисекунды и оставляет
//! пиксели там, где они есть: следующим шагом их забирает энкодер,
//! тоже не выходя из GPU.
//!
//! # Почему только степени двойки
//!
//! Уменьшение вдвое — это ровно четыре исходных пикселя на один
//! выходной, и билинейный семплер берёт их без остатка. Произвольная
//! доля (скажем, 0.7) даёт неравномерную выборку, а на тексте это
//! видно как дрожание тонких линий.

use crate::error::{CaptureError, Result};
use std::ffi::CStr;
use windows::core::PCSTR;
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCOMPILE_WARNINGS_ARE_ERRORS,
};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_COMPARISON_NEVER,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_SAMPLER_DESC, D3D11_TEXTURE2D_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

/// Исходник шейдера. Компилируется при старте, как и в `bd-render`:
/// `fxc` не входит в требования к машине (§10.1), а
/// `d3dcompiler_47.dll` есть в любой Windows.
const SHADER_SOURCE: &str = include_str!("downscale.hlsl");

/// Уменьшитель кадров.
///
/// Держит цель нужного размера и переиспользует её: создавать
/// текстуру 60 раз в секунду значило бы мусорить в видеопамяти на
/// ровном месте.
pub struct Downscaler {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    /// Текущая цель и её размер. `None` — ещё не создавали.
    target: Option<Target>,
}

struct Target {
    texture: ID3D11Texture2D,
    view: ID3D11RenderTargetView,
    width: u32,
    height: u32,
}

impl Downscaler {
    /// Собрать шейдеры на устройстве захвата.
    ///
    /// Устройство именно то, на котором живёт кадр: копия между
    /// адаптерами стоила бы дороже самого уменьшения (§5.1).
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let vs_blob = compile(SHADER_SOURCE, "vs_main", "vs_5_0")?;
        let ps_blob = compile(SHADER_SOURCE, "ps_main", "ps_5_0")?;

        let mut vertex_shader = None;
        let mut pixel_shader = None;

        // SAFETY: блобы живы до конца вызова; размеры берутся у них
        // самих, а не задаются нами.
        unsafe {
            device
                .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vertex_shader))
                .map_err(|e| platform("CreateVertexShader", e))?;
            device
                .CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut pixel_shader))
                .map_err(|e| platform("CreatePixelShader", e))?;
        }

        // Линейная фильтрация — то, ради чего этот проход и делается:
        // выборка одного пикселя из четырёх дала бы рябь на тексте.
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
        // SAFETY: описание заполнено целиком и живёт до конца вызова.
        unsafe {
            device
                .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .map_err(|e| platform("CreateSamplerState", e))?;
        }

        Ok(Self {
            vertex_shader: vertex_shader.ok_or_else(|| missing("вершинный шейдер"))?,
            pixel_shader: pixel_shader.ok_or_else(|| missing("пиксельный шейдер"))?,
            sampler: sampler.ok_or_else(|| missing("семплер"))?,
            target: None,
        })
    }

    /// Уменьшить кадр в `divisor` раз по каждой стороне.
    ///
    /// Возвращает текстуру, пригодную энкодеру. При `divisor == 1`
    /// вызывать не нужно вовсе — проход впустую стоил бы времени и
    /// видеопамяти; вызывающий обязан проверить сам.
    ///
    /// Результат живёт до следующего вызова: текстура одна и
    /// переиспользуется. Энкодер забирает кадр синхронно, поэтому
    /// держать несколько незачем.
    pub fn downscale(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source: &ID3D11Texture2D,
        divisor: u32,
    ) -> Result<&ID3D11Texture2D> {
        debug_assert!(divisor > 1, "уменьшение в 1 раз — это проход впустую");

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: текстура жива; GetDesc заполняет переданную структуру.
        unsafe { source.GetDesc(&mut desc) };

        // Размер округляется вниз до чётного: H.264 кодирует
        // макроблоками, и нечётная сторона заставила бы энкодер
        // выравнивать её самому — с полосой мусора по краю, как в
        // находке 18.
        let width = (desc.Width / divisor).max(2) & !1;
        let height = (desc.Height / divisor).max(2) & !1;

        self.ensure_target(device, width, height)?;
        let target = self.target.as_ref().ok_or_else(|| missing("цель вывода"))?;

        let srv = self.source_view(device, source)?;

        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };

        // SAFETY: все ресурсы живы до конца вызова; слоты те же, что
        // объявлены в шейдере (t0, s0), вершинный буфер не нужен —
        // треугольник строится по SV_VertexID.
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(target.view.clone())]), None);
            context.RSSetViewports(Some(&[viewport]));
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(&self.pixel_shader, None);
            context.PSSetShaderResources(0, Some(&[Some(srv)]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.Draw(3, 0);

            // Цель отвязывается сразу: оставленная привязанной, она не
            // даст создать на эту же текстуру представление ресурса, и
            // энкодер получит E_INVALIDARG вместо кадра.
            context.OMSetRenderTargets(None, None);
            context.PSSetShaderResources(0, Some(&[None]));
        }

        // Ссылка, а не копия хендла: текстура принадлежит
        // уменьшителю и переиспользуется между кадрами. Отдавать
        // владение значило бы заставлять вызывающего держать её у
        // себя, а он и так знает, что кадр живёт до следующего.
        Ok(&self.target.as_ref().expect("создана выше").texture)
    }

    /// Создать цель нужного размера, если её ещё нет.
    fn ensure_target(&mut self, device: &ID3D11Device, width: u32, height: u32) -> Result<()> {
        if let Some(existing) = &self.target {
            if existing.width == width && existing.height == height {
                return Ok(());
            }
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            // Формат тот же, что у захвата: конверсию цвета делает
            // энкодер, и вмешиваться в неё здесь незачем.
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // Оба флага обязательны: RENDER_TARGET — чтобы мы могли в
            // неё рисовать, SHADER_RESOURCE — чтобы энкодер мог её
            // прочитать (та же причина, что в находке 23).
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };

        let mut texture = None;
        // SAFETY: описание заполнено целиком; начальных данных нет.
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(|e| platform("CreateTexture2D для уменьшения", e))?;
        }
        let texture = texture.ok_or_else(|| missing("текстура цели"))?;

        let mut view = None;
        // SAFETY: текстура создана выше и жива; описание по умолчанию
        // означает «всё представление целиком».
        unsafe {
            device
                .CreateRenderTargetView(&texture, None, Some(&mut view))
                .map_err(|e| platform("CreateRenderTargetView", e))?;
        }

        self.target = Some(Target {
            texture,
            view: view.ok_or_else(|| missing("представление цели"))?,
            width,
            height,
        });
        Ok(())
    }

    /// Представление исходного кадра для шейдера.
    fn source_view(
        &self,
        device: &ID3D11Device,
        source: &ID3D11Texture2D,
    ) -> Result<ID3D11ShaderResourceView> {
        let mut view = None;
        // SAFETY: текстура жива до конца вызова; описание по умолчанию
        // берёт формат и размерность у самой текстуры.
        unsafe {
            device
                .CreateShaderResourceView(source, None, Some(&mut view))
                .map_err(|e| platform("CreateShaderResourceView для кадра", e))?;
        }
        view.ok_or_else(|| missing("представление кадра"))
    }
}

/// Скомпилировать одну точку входа шейдера.
fn compile(source: &str, entry: &str, target: &str) -> Result<ID3DBlob> {
    let entry = std::ffi::CString::new(entry).map_err(|_| missing("имя точки входа"))?;
    let target_c = std::ffi::CString::new(target).map_err(|_| missing("имя профиля"))?;

    let mut code = None;
    let mut errors = None;

    // SAFETY: исходник и строки живут до конца вызова; выходные
    // указатели наши и инициализированы `None`.
    let result = unsafe {
        D3DCompile(
            source.as_ptr() as *const _,
            source.len(),
            None,
            None,
            None,
            PCSTR(entry.as_ptr() as *const u8),
            PCSTR(target_c.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3 | D3DCOMPILE_WARNINGS_ARE_ERRORS,
            0,
            &mut code,
            Some(&mut errors),
        )
    };

    if let Err(e) = result {
        // Текст ошибки компилятора важнее кода: он называет строку и
        // причину, а HRESULT говорит лишь «не вышло».
        //
        // В `CaptureError::Platform` места под него нет, поэтому он
        // уходит в лог. Терять его нельзя: шейдер компилируется на
        // ЧУЖОЙ машине с чужим драйвером, и «не вышло» без строки —
        // это сообщение, по которому нельзя сделать ничего (находки
        // 61, 63).
        let detail = errors
            .as_ref()
            .map(|blob| {
                // SAFETY: блоб жив; компилятор кладёт туда строку,
                // завершённую нулём.
                let ptr = unsafe { blob.GetBufferPointer() } as *const i8;
                unsafe { CStr::from_ptr(ptr) }
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default();
        tracing::error!(entry = %entry.to_string_lossy(), %detail, "шейдер уменьшения не скомпилировался");
        return Err(CaptureError::Platform {
            context: "компиляция шейдера уменьшения",
            code: e.code().0 as u32,
        });
    }

    code.ok_or_else(|| missing("скомпилированный шейдер"))
}

/// Байты скомпилированного блоба.
fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: блоб жив, пока жива ссылка; размер даёт он сам.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

fn platform(context: &'static str, e: windows::core::Error) -> CaptureError {
    CaptureError::Platform {
        context,
        code: e.code().0 as u32,
    }
}

/// D3D11 вернул успех, но не отдал объект.
///
/// Случай теоретический, но молчать о нём нельзя: `None` там, где
/// ожидался ресурс, дальше превратился бы в панику при разыменовании.
fn missing(what: &'static str) -> CaptureError {
    tracing::error!(what, "D3D11 вернул успех, но не отдал объект");
    CaptureError::Platform {
        context: what,
        code: 0,
    }
}
