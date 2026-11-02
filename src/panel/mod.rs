//! 面板展示契约。只包含译文与可见操作，不泄漏原文、网络请求或平台资源。

use crate::core::{
    Content, Display, Image, Line, PANEL_HEADER, PANEL_HEIGHT, PANEL_WIDTH, Painter, Term, Theme,
    Translation, View,
};
use cosmic_text::{Attrs, Buffer, Color, FontSystem, Metrics, Shaping, SwashCache};

fn view(display: &Display, pinned: bool, theme: Theme) -> View {
    let mut lines = Vec::new();
    match &display.content {
        Content::Ready(Translation::Dictionary(entries)) => {
            for entry in entries {
                if !entry.pos.is_empty() {
                    lines.push(Line {
                        text: entry.pos.clone(),
                        small: true,
                        copy: false,
                    });
                }
                lines.extend(entry.terms.iter().map(|term| Line {
                    text: term.clone(),
                    small: false,
                    copy: true,
                }));
            }
        }
        content => lines.push(Line {
            text: match content {
                Content::Loading => "翻译中…".into(),
                Content::Ready(Translation::Sentence(text)) => text.clone(),
                Content::Ready(Translation::NoResult) => "无结果".into(),
                Content::Failed { error, .. } => error.to_string(),
                _ => unreachable!(),
            },
            small: false,
            copy: false,
        }),
    }
    View {
        identity: display.identity(),
        pinned,
        theme,
        retry: matches!(display.content, Content::Failed { retry: Some(_), .. }),
        lines,
    }
}

/// 原生桌面坐标必须属于唯一输出；尺寸固定，不猜测输出、不裁切面板。
fn place(point: (f64, f64), bounds: (f64, f64, f64, f64)) -> Option<(f64, f64)> {
    let (x, y, width, height) = bounds;
    if ![point.0, point.1, x, y, width, height]
        .iter()
        .all(|n| n.is_finite())
        || width < PANEL_WIDTH
        || height < PANEL_HEIGHT
        || !(x..x + width).contains(&point.0)
        || !(y..y + height).contains(&point.1)
    {
        return None;
    }
    let below = point.1 + 20.0;
    let top = if below + PANEL_HEIGHT <= y + height {
        below
    } else {
        point.1 - 20.0 - PANEL_HEIGHT
    };
    Some((
        (point.0 - 60.0).clamp(x, x + width - PANEL_WIDTH),
        top.clamp(y, y + height - PANEL_HEIGHT),
    ))
}

struct FontResources {
    system: FontSystem,
    cache: SwashCache,
}

#[derive(Default)]
struct Renderer {
    fonts: Option<FontResources>,
}

pub(crate) fn register() -> fn() -> Box<dyn Painter> {
    fn create() -> Box<dyn Painter> {
        Box::new(Renderer::default())
    }
    create
}

impl Painter for Renderer {
    fn view(&self, display: &Display, pinned: bool, theme: Theme) -> View {
        view(display, pinned, theme)
    }
    fn place(&self, point: (f64, f64), bounds: (f64, f64, f64, f64)) -> Option<(f64, f64)> {
        place(point, bounds)
    }
    fn reset(&mut self) {
        self.fonts = None;
    }

    fn render(&mut self, view: &View, size: (u32, u32), scale: f64, scroll: f64) -> Image {
        let light = view.theme == Theme::Light;
        let mut pixels =
            vec![if light { 0x00f0f2f5 } else { 0x001f2126 }; size.0 as usize * size.1 as usize];
        for (text, x, font) in [
            (
                if view.pinned {
                    "取消固定"
                } else {
                    "固定"
                },
                8.0,
                11.0,
            ),
            ("Glosslet", 105.0, 11.0),
            (if light { "深色" } else { "浅色" }, 180.0, 11.0),
            ("×", 238.0, 16.0),
        ] {
            self.text(
                &mut pixels,
                size,
                text,
                (x, 1.0),
                font,
                scale,
                Color::rgb(89, 158, 191),
                false,
            );
        }
        let color = if light {
            Color::rgb(31, 31, 31)
        } else {
            Color::rgb(240, 240, 240)
        };
        let mut y = PANEL_HEADER + 4.0;
        let mut terms = Vec::new();
        for line in &view.lines {
            let height = self.text(
                &mut pixels,
                size,
                &line.text,
                (8.0, y - scroll),
                if line.small { 11.0 } else { 14.0 },
                scale,
                color,
                true,
            );
            if line.copy {
                terms.push(Term {
                    top: y - scroll,
                    bottom: y - scroll + height + 4.0,
                    text: line.text.clone(),
                });
            }
            y += height + if line.small { 3.0 } else { 6.0 };
        }
        Image {
            pixels,
            height: y - PANEL_HEADER,
            terms,
        }
    }
}

impl Renderer {
    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        pixels: &mut [u32],
        size: (u32, u32),
        value: &str,
        origin: (f64, f64),
        font: f64,
        scale: f64,
        color: Color,
        clip: bool,
    ) -> f64 {
        let fonts = self.fonts.get_or_insert_with(|| FontResources {
            system: FontSystem::new(),
            cache: SwashCache::new(),
        });
        let mut buffer = Buffer::new(
            &mut fonts.system,
            Metrics::new((font * scale) as f32, ((font + 4.0) * scale) as f32),
        );
        buffer.set_size(&mut fonts.system, Some((240.0 * scale) as f32), None);
        buffer.set_text(
            &mut fonts.system,
            value,
            &Attrs::new(),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut fonts.system, false);
        let height = buffer
            .layout_runs()
            .map(|run| run.line_top + run.line_height)
            .last()
            .unwrap_or(0.0) as f64
            / scale;
        // 仍计算完整文本高度供滚动使用，但不栅格化视口之外的行；字形缓存只属于可见面板。
        if clip && (origin.1 + height <= PANEL_HEADER || origin.1 >= PANEL_HEIGHT - 4.0) {
            return height;
        }
        buffer.draw(
            &mut fonts.system,
            &mut fonts.cache,
            color,
            |x, y, width, height, rgba| {
                for dy in 0..height as i32 {
                    for dx in 0..width as i32 {
                        let x = x + dx + (origin.0 * scale).round() as i32;
                        let y = y + dy + (origin.1 * scale).round() as i32;
                        if x < 0
                            || y < 0
                            || x >= size.0 as i32
                            || y >= size.1 as i32
                            || (clip
                                && (y as f64 / scale < PANEL_HEADER
                                    || y as f64 / scale >= PANEL_HEIGHT - 4.0))
                        {
                            continue;
                        }
                        let pixel = &mut pixels[y as usize * size.0 as usize + x as usize];
                        let a = rgba.a() as u32;
                        let blend = |old: u32, new: u32| (new * a + old * (255 - a)) / 255;
                        *pixel = (blend((*pixel >> 16) & 255, rgba.r() as u32) << 16)
                            | (blend((*pixel >> 8) & 255, rgba.g() as u32) << 8)
                            | blend(*pixel & 255, rgba.b() as u32);
                    }
                }
            },
        );
        height
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{DictionaryEntry, Ticket};

    #[test]
    fn shared_frame_has_logical_word_boxes_at_each_scale_and_releases_fonts() {
        let display = Display::new(
            Ticket(1),
            Content::Ready(Translation::Dictionary(vec![DictionaryEntry {
                pos: "noun".into(),
                terms: vec!["第一条释义".into(), "second meaning".into()],
            }])),
        );
        let mut renderer = Renderer::default();
        let view = renderer.view(&display, true, Theme::Dark);
        assert_eq!(view.identity, display.identity());
        assert_eq!(view.lines.len(), 3);
        assert!(!view.lines[0].copy);
        for scale in [1, 2] {
            let image = renderer.render(&view, (256 * scale, 100 * scale), scale as f64, 0.0);
            assert_eq!(image.pixels.len(), (256 * scale * 100 * scale) as usize);
            assert_eq!(image.terms.len(), 2);
            assert_eq!(image.terms[0].text, "第一条释义");
            assert!(image.terms[0].top >= PANEL_HEADER);
            assert!(image.terms[0].bottom > image.terms[0].top);
            assert!(image.height > 0.0);
            assert!(renderer.fonts.is_some());
        }
        renderer.reset();
        assert!(renderer.fonts.is_none());
    }

    #[test]
    fn idle_painter_never_loads_fonts() {
        let mut renderer = Renderer::default();
        assert!(renderer.fonts.is_none());
        renderer.reset();
        assert!(renderer.fonts.is_none());
        assert!(
            renderer
                .place((500.0, 500.0), (0.0, 0.0, 1920.0, 1080.0))
                .is_some()
        );
        assert!(renderer.fonts.is_none());
    }

    #[test]
    fn logical_placement_rejects_invalid_and_small_outputs_and_uses_space_above() {
        assert_eq!(
            place((-20.0, 1050.0), (-1920.0, 0.0, 1920.0, 1080.0)),
            Some((-256.0, 930.0))
        );
        assert_eq!(
            place((1900.0, 1050.0), (0.0, 0.0, 1920.0, 1080.0)),
            Some((1664.0, 930.0))
        );
        assert!(place((0.0, 0.0), (0.0, 0.0, 255.0, 100.0)).is_none());
        assert!(place((f64::NAN, 0.0), (0.0, 0.0, 1920.0, 1080.0)).is_none());
        assert!(place((1920.0, 0.0), (0.0, 0.0, 1920.0, 1080.0)).is_none());
    }
}
