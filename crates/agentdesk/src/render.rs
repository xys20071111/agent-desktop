//! 渲染与截图：pixman 软件渲染 + 离屏导出（png/jpeg/raw）。
//!
//! 管线：RenderElements（Space 或单窗口）→ 离屏 Image → copy_framebuffer → 编码。
//! 约定：帧数据为 BGRA8888（Fourcc::Argb8888 小端内存序），stride = width*4。

use std::io::Cursor;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::utils::draw_render_elements;
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Frame, Offscreen, Renderer};
use smithay::desktop::space::SpaceRenderElements;
use smithay::utils::{Buffer, Rectangle, Scale, Size, Transform};
use pixman::Image;

use ad_ipc::error::{codes, RpcError};
use ad_ipc::protocol::{CaptureOptions, ImageEncoding, ImageResultJson};

use crate::state::AgentDesk;

smithay::render_elements! {
    pub CaptureElements<=PixmanRenderer>;
    Space = SpaceRenderElements<PixmanRenderer, WaylandSurfaceRenderElement<PixmanRenderer>>,
    Surface = WaylandSurfaceRenderElement<PixmanRenderer>,
}

/// 截图原始帧（BGRA8888）。
pub struct RawFrame {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

impl AgentDesk {
    /// 整屏截图（指定输出或默认主输出）。
    pub fn screen_capture(&mut self, output_name: Option<&str>) -> Result<RawFrame, RpcError> {
        let output = {
            let found = self.outputs.iter().find(|o| match output_name {
                Some(n) => o.name == n,
                None => o.name == self.outputs.first().map(|f| f.name.clone()).unwrap_or_default(),
            });
            found.map(|o| o.output.clone()).ok_or_else(|| {
                RpcError::new(codes::OUTPUT_NOT_FOUND, format!("输出 {output_name:?} 不存在"))
            })?
        };
        let size = output.current_mode().map(|m| m.size).unwrap_or_else(|| Size::from((1280, 720)));

        let Some(renderer) = self.renderer.as_mut() else {
            return Err(RpcError::internal("渲染器不可用"));
        };
        let elements: Vec<CaptureElements> = self
            .space
            .render_elements_for_output(renderer, &output, 1.0)
            .map_err(|e| RpcError::new(codes::CAPTURE_FAILED, format!("收集渲染元素失败: {e}")))?
            .into_iter()
            .map(CaptureElements::from)
            .collect();
        self.render_elements_to_buffer(size, &elements, false)
    }

    /// 单窗口截图（不受遮挡影响，保留 alpha；弹窗未跟踪，见设计 §7.4）。
    pub fn window_capture(&mut self, id: u64) -> Result<RawFrame, RpcError> {
        let window = self
            .windows
            .get(&id)
            .ok_or_else(|| RpcError::window_not_found(id))?
            .element
            .clone();
        let bbox = window.bbox();
        let size = Size::from((bbox.size.w.max(1), bbox.size.h.max(1)));

        let Some(renderer) = self.renderer.as_mut() else {
            return Err(RpcError::internal("渲染器不可用"));
        };
        let elements: Vec<CaptureElements> =
            window.render_elements(renderer, (0, 0).into(), Scale::from(1.0), 1.0);
        self.render_elements_to_buffer(size, &elements, true)
    }

    /// 把渲染元素画到离屏缓冲并读回像素。
    fn render_elements_to_buffer(
        &mut self,
        size: Size<i32, smithay::utils::Physical>,
        elements: &[CaptureElements],
        transparent: bool,
    ) -> Result<RawFrame, RpcError> {
        let (w, h) = (size.w, size.h);
        let Some(renderer) = self.renderer.as_mut() else {
            return Err(RpcError::internal("渲染器不可用"));
        };

        let mut buffer: Image<'static, 'static> = Offscreen::<Image>::create_buffer(
            renderer,
            Fourcc::Argb8888,
            Size::from((w, h)),
        )
        .map_err(|e| capture_err("离屏缓冲创建失败", e))?;
        let mut target = renderer.bind(&mut buffer).map_err(|e| capture_err("绑定失败", e))?;
        let mut frame = renderer
            .render(&mut target, size, Transform::Normal)
            .map_err(|e| capture_err("开始渲染失败", e))?;
        let clear = if transparent {
            Color32F::new(0.0, 0.0, 0.0, 0.0)
        } else {
            Color32F::new(0.1, 0.1, 0.12, 1.0)
        };
        frame
            .clear(clear, &[Rectangle::from_size(size)])
            .map_err(|e| capture_err("清屏失败", e))?;
        draw_render_elements(&mut frame, 1.0f64, elements, &[Rectangle::from_size(size)])
            .map_err(|e| capture_err("渲染元素失败", e))?;
        frame.finish().map_err(|e| capture_err("帧完成失败", e))?;

        let mapping = renderer
            .copy_framebuffer(
                &target,
                Rectangle::from_size(Size::<i32, Buffer>::from((w, h))),
                Fourcc::Argb8888,
            )
            .map_err(|e| capture_err("读回像素失败", e))?;
        let data = renderer.map_texture(&mapping).map_err(|e| capture_err("映射失败", e))?.to_vec();
        Ok(RawFrame { width: w, height: h, data })
    }

    /// 编码截图并组装 IPC 响应（png/jpeg/raw；base64 或 to_path）。
    pub fn encode_capture(
        &self,
        raw: RawFrame,
        opts: &CaptureOptions,
    ) -> Result<ImageResultJson, RpcError> {
        if opts.via_fd == Some(true) {
            return Err(RpcError::invalid_params("via_fd 尚未实现，请使用 base64 或 to_path"));
        }
        let format = opts.format.as_deref().unwrap_or("png").to_ascii_lowercase();
        let encoded: Vec<u8> = match format.as_str() {
            "png" => encode_png(&raw)?,
            "jpeg" | "jpg" => encode_jpeg(&raw, opts.quality.unwrap_or(85))?,
            "raw" => raw.data.clone(),
            other => {
                return Err(RpcError::invalid_params(format!(
                    "未知格式 {other}（可选 png|jpeg|raw）"
                )))
            }
        };
        match &opts.to_path {
            Some(path) => {
                std::fs::write(path, &encoded)
                    .map_err(|e| RpcError::internal(format!("写文件失败: {e}")))?;
                Ok(ImageResultJson {
                    format,
                    width: raw.width as u32,
                    height: raw.height as u32,
                    encoding: ImageEncoding::File,
                    data: None,
                    path: Some(path.clone()),
                    sha256: None,
                })
            }
            None => {
                use base64::Engine as _;
                let data = base64::engine::general_purpose::STANDARD.encode(&encoded);
                Ok(ImageResultJson {
                    format,
                    width: raw.width as u32,
                    height: raw.height as u32,
                    encoding: ImageEncoding::Base64,
                    data: Some(data),
                    path: None,
                    sha256: None,
                })
            }
        }
    }
}

/// 截图管线错误统一封装。
fn capture_err(what: &str, e: impl std::fmt::Debug) -> RpcError {
    RpcError::new(codes::CAPTURE_FAILED, format!("{what}: {e:?}"))
}

fn encode_png(raw: &RawFrame) -> Result<Vec<u8>, RpcError> {
    let img = image::RgbaImage::from_raw(raw.width as u32, raw.height as u32, bgra_to_rgba(raw))
        .ok_or_else(|| RpcError::internal("像素数据与尺寸不匹配"))?;
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| RpcError::new(codes::CAPTURE_FAILED, format!("PNG 编码失败: {e}")))?;
    Ok(out)
}

fn encode_jpeg(raw: &RawFrame, quality: u8) -> Result<Vec<u8>, RpcError> {
    let mut rgba = bgra_to_rgba(raw);
    // JPEG 无 alpha：与黑底合成并转为 Rgb8（编码器不支持 Rgba8）
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        for i in 0..3 {
            rgb.push(((u32::from(px[i]) * a) / 255) as u8);
        }
    }
    let img = image::RgbImage::from_raw(raw.width as u32, raw.height as u32, rgb)
        .ok_or_else(|| RpcError::internal("像素数据与尺寸不匹配"))?;
    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100));
    img.write_with_encoder(encoder)
        .map_err(|e| RpcError::new(codes::CAPTURE_FAILED, format!("JPEG 编码失败: {e}")))?;
    Ok(out)
}

/// BGRA（Fourcc::Argb8888 内存序）→ RGBA（image crate 期望）。
fn bgra_to_rgba(raw: &RawFrame) -> Vec<u8> {
    let mut v = raw.data.clone();
    for px in v.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    v
}
