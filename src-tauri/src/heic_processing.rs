use anyhow::{Result, anyhow};
use heic::{DecodedFrame, DecoderConfig, ImageInfo, Stop, StopReason};
use image::{DynamicImage, Rgb32FImage};
use moxcms::{ColorProfile, Layout, TransformOptions};
use rayon::prelude::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const COLOR_CONVERSION_ROWS: usize = 64;

struct CancelToken(Option<(Arc<AtomicUsize>, usize)>);

impl Stop for CancelToken {
    fn check(&self) -> std::result::Result<(), StopReason> {
        if let Some((tracker, generation)) = &self.0
            && tracker.load(Ordering::SeqCst) != *generation
        {
            return Err(StopReason::Cancelled);
        }
        Ok(())
    }
}

impl CancelToken {
    fn check_cancel(&self) -> Result<()> {
        self.check().map_err(|_| anyhow!("Load cancelled"))
    }
}

pub fn heic_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    ImageInfo::from_bytes(bytes)
        .ok()
        .map(|info| (info.width, info.height))
}

pub fn decode_heic(
    bytes: &[u8],
    cancel_token: Option<(Arc<AtomicUsize>, usize)>,
) -> Result<DynamicImage> {
    let cancel = CancelToken(cancel_token);
    cancel.check_cancel()?;

    let info = ImageInfo::from_bytes(bytes).map_err(|e| anyhow!("HEIC probe failed: {}", e))?;

    let decoder = DecoderConfig::new();
    let frame = decoder
        .decode_request(bytes)
        .with_stop(&cancel)
        .decode_yuv()
        .map_err(|e| {
            if cancel.should_stop() {
                anyhow!("Load cancelled")
            } else {
                anyhow!("HEIC decode failed: {}", e)
            }
        })?;

    cancel.check_cancel()?;

    let mut image = frame_to_rgb32f(&frame);
    drop(frame);

    cancel.check_cancel()?;

    if let Some(profile) = source_profile(&info) {
        convert_to_srgb(&mut image, &profile)?;
    }

    Ok(DynamicImage::ImageRgb32F(image))
}

/// Converts straight to float at the source bit depth. heic's own `to_rgb16`
/// scales full-range chroma for 8-bit, which desaturates 10-bit images.
fn frame_to_rgb32f(frame: &DecodedFrame) -> Rgb32FImage {
    let width = frame.cropped_width() as usize;
    let height = frame.cropped_height();
    let mut image = Rgb32FImage::new(width as u32, height);

    let depth_scale = (1u32 << frame.bit_depth.saturating_sub(8)) as f32;
    let max_value = ((1u32 << frame.bit_depth) - 1) as f32;
    let neutral = (1u32 << (frame.bit_depth - 1)) as f32;
    let (y_offset, y_range, c_range) = if frame.full_range {
        (0.0, max_value, max_value)
    } else {
        (16.0 * depth_scale, 219.0 * depth_scale, 224.0 * depth_scale)
    };
    let (kr, kb) = match frame.matrix_coeffs {
        1 => (0.2126, 0.0722),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.299, 0.114),
    };
    let kg = 1.0 - kr - kb;
    let (shift_x, shift_y) = match frame.chroma_format {
        1 => (1, 1),
        2 => (1, 0),
        _ => (0, 0),
    };
    let (y_stride, c_stride) = (frame.y_stride(), frame.c_stride());

    image
        .par_chunks_mut(width * 3)
        .enumerate()
        .for_each(|(row, out)| {
            let y = row + frame.crop_top as usize;
            for (col, pixel) in out.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                let x = col + frame.crop_left as usize;
                let luma = frame
                    .y_plane
                    .get(y * y_stride + x)
                    .map_or(0.0, |&v| v as f32);
                let c_idx = (y >> shift_y) * c_stride + (x >> shift_x);
                let chroma = |plane: &[u16]| plane.get(c_idx).map_or(neutral, |&v| v as f32);
                let (cb, cr) = if frame.chroma_format == 0 {
                    (neutral, neutral)
                } else {
                    (chroma(&frame.cb_plane), chroma(&frame.cr_plane))
                };

                let rgb = if frame.matrix_coeffs == 0 {
                    // Identity (GBR): planes hold G, B, R directly.
                    [cr / max_value, luma / max_value, cb / max_value]
                } else {
                    let y_norm = (luma - y_offset) / y_range;
                    let cb_norm = (cb - neutral) / c_range;
                    let cr_norm = (cr - neutral) / c_range;
                    let r = y_norm + 2.0 * (1.0 - kr) * cr_norm;
                    let b = y_norm + 2.0 * (1.0 - kb) * cb_norm;
                    let g = (y_norm - kr * r - kb * b) / kg;
                    [r, g, b]
                };
                for (out, value) in pixel.iter_mut().zip(rgb) {
                    *out = value.clamp(0.0, 1.0);
                }
            }
        });

    image
}

/// The rest of the pipeline treats non-RAW images as sRGB. iPhone HEICs are
/// Display P3, so wide-gamut sources are converted here instead.
fn source_profile(info: &ImageInfo) -> Option<ColorProfile> {
    if let Some(icc) = &info.icc_profile {
        return match ColorProfile::new_from_slice(icc) {
            Ok(profile) => Some(profile),
            Err(e) => {
                log::warn!("Ignoring unreadable HEIC ICC profile: {}", e);
                None
            }
        };
    }

    match info.color_primaries {
        12 => Some(ColorProfile::new_display_p3()),
        9 => Some(ColorProfile::new_bt2020()),
        _ => None,
    }
}

fn convert_to_srgb(image: &mut Rgb32FImage, source: &ColorProfile) -> Result<()> {
    let transform = source
        .create_transform_f32(
            Layout::Rgb,
            &ColorProfile::new_srgb(),
            Layout::Rgb,
            TransformOptions::default(),
        )
        .map_err(|e| anyhow!("Failed to build HEIC color transform: {}", e))?;

    let chunk_len = image.width() as usize * 3 * COLOR_CONVERSION_ROWS;
    image
        .par_chunks_mut(chunk_len)
        .try_for_each(|chunk| {
            let src = chunk.to_vec();
            transform.transform(&src, chunk)?;
            for value in chunk.iter_mut() {
                *value = value.clamp(0.0, 1.0);
            }
            Ok(())
        })
        .map_err(|e: moxcms::CmsError| anyhow!("HEIC color conversion failed: {}", e))
}
