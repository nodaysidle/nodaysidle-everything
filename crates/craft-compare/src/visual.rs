//! Visual parity: block-wise structural similarity between the original screenshot and a
//! screenshot of the recreation, plus a heatmap showing where they differ.

use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use std::io::Cursor;

const BLOCK: u32 = 8;
const MAX_WIDTH: u32 = 1280;

pub struct VisualDiff {
    /// 0.0 (unrelated) ..= 1.0 (pixel-identical).
    pub score: f32,
    pub heatmap_png: Vec<u8>,
    /// Where the recreation differs most, e.g. "top-left".
    pub worst_region: &'static str,
}

struct BlockStats {
    luma_mean: f64,
    luma_var: f64,
    rgb_mean: [f64; 3],
}

fn block_stats(img: &RgbImage, x0: u32, y0: u32, w: u32, h: u32) -> (BlockStats, Vec<f64>) {
    let mut lumas = Vec::with_capacity((w * h) as usize);
    let mut rgb = [0.0; 3];
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let p = img.get_pixel(x, y).0;
            for c in 0..3 {
                rgb[c] += f64::from(p[c]);
            }
            lumas.push(0.299 * f64::from(p[0]) + 0.587 * f64::from(p[1]) + 0.114 * f64::from(p[2]));
        }
    }
    let n = lumas.len() as f64;
    let mean = lumas.iter().sum::<f64>() / n;
    let var = lumas.iter().map(|l| (l - mean).powi(2)).sum::<f64>() / n;
    (
        BlockStats {
            luma_mean: mean,
            luma_var: var,
            rgb_mean: rgb.map(|c| c / n),
        },
        lumas,
    )
}

/// Compares two encoded images (PNG/JPEG). The recreation is resized to the original's size.
pub fn diff(original: &[u8], recreation: &[u8]) -> Result<VisualDiff, String> {
    let load = |bytes: &[u8], what: &str| {
        image::load_from_memory(bytes).map_err(|e| format!("cannot decode {what} screenshot: {e}"))
    };
    let mut a: DynamicImage = load(original, "original")?;
    if a.width() > MAX_WIDTH {
        a = a.resize(MAX_WIDTH, u32::MAX, FilterType::Triangle);
    }
    let (width, height) = (a.width(), a.height());
    if width < BLOCK || height < BLOCK {
        return Err("original screenshot is too small to compare".into());
    }
    let a = a.to_rgb8();
    let b = load(recreation, "recreation")?
        .resize_exact(width, height, FilterType::Triangle)
        .to_rgb8();

    // Standard SSIM stabilizers for 8-bit data.
    let (c1, c2) = ((0.01 * 255.0f64).powi(2), (0.03 * 255.0f64).powi(2));
    let mut heatmap = RgbImage::new(width, height);
    let (mut weighted_sum, mut weight_total) = (0.0, 0.0);
    // Dissimilarity accumulated per cell of a 3x3 grid, to name the worst region.
    let mut regions = [(0.0f64, 0.0f64); 9];

    for by in (0..height).step_by(BLOCK as usize) {
        for bx in (0..width).step_by(BLOCK as usize) {
            let (w, h) = (BLOCK.min(width - bx), BLOCK.min(height - by));
            let (sa, la) = block_stats(&a, bx, by, w, h);
            let (sb, lb) = block_stats(&b, bx, by, w, h);
            let covariance = la
                .iter()
                .zip(&lb)
                .map(|(x, y)| (x - sa.luma_mean) * (y - sb.luma_mean))
                .sum::<f64>()
                / la.len() as f64;
            let ssim = ((2.0 * sa.luma_mean * sb.luma_mean + c1) * (2.0 * covariance + c2))
                / ((sa.luma_mean.powi(2) + sb.luma_mean.powi(2) + c1)
                    * (sa.luma_var + sb.luma_var + c2));
            // SSIM works on brightness only; a hue change at equal brightness must still count.
            let color_distance = (0..3)
                .map(|c| (sa.rgb_mean[c] - sb.rgb_mean[c]).powi(2))
                .sum::<f64>()
                .sqrt()
                / 441.673;
            let similarity = (ssim.clamp(0.0, 1.0) * (1.0 - color_distance)).clamp(0.0, 1.0);

            // Blocks with actual content outweigh empty background, so two mostly-blank pages
            // with different content do not look alike.
            let weight = if sa.luma_var > 25.0 || sb.luma_var > 25.0 { 4.0 } else { 1.0 };
            weighted_sum += similarity * weight;
            weight_total += weight;
            let cell = ((by * 3 / height) * 3 + bx * 3 / width) as usize;
            regions[cell].0 += (1.0 - similarity) * weight;
            regions[cell].1 += weight;

            let heat = 1.0 - similarity;
            for y in by..by + h {
                for x in bx..bx + w {
                    let p = a.get_pixel(x, y).0;
                    let gray = 0.299 * f64::from(p[0]) + 0.587 * f64::from(p[1]) + 0.114 * f64::from(p[2]);
                    let dim = 96.0 + gray * 0.5;
                    let mix = |target: f64| (dim * (1.0 - heat) + target * heat) as u8;
                    heatmap.put_pixel(x, y, Rgb([mix(255.0), mix(32.0), mix(32.0)]));
                }
            }
        }
    }

    const NAMES: [&str; 9] = [
        "top-left", "top-center", "top-right", "middle-left", "center", "middle-right",
        "bottom-left", "bottom-center", "bottom-right",
    ];
    let worst = regions
        .iter()
        .enumerate()
        .max_by(|(_, x), (_, y)| (x.0 / x.1.max(1.0)).total_cmp(&(y.0 / y.1.max(1.0))))
        .map_or(4, |(i, _)| i);

    let mut heatmap_png = Vec::new();
    heatmap
        .write_to(&mut Cursor::new(&mut heatmap_png), ImageFormat::Png)
        .map_err(|e| format!("cannot encode heatmap: {e}"))?;

    Ok(VisualDiff {
        score: (weighted_sum / weight_total) as f32,
        heatmap_png,
        worst_region: NAMES[worst],
    })
}

/// Pixel size of an encoded image.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::load_from_memory(bytes).ok().map(|i| (i.width(), i.height()))
}

#[cfg(test)]
pub(crate) fn test_png(width: u32, height: u32, paint: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let img = RgbImage::from_fn(width, height, |x, y| Rgb(paint(x, y)));
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).unwrap();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(header: [u8; 3], with_card: bool) -> Vec<u8> {
        test_png(320, 240, |x, y| {
            if y < 40 {
                header
            } else if with_card && (60..160).contains(&x) && (80..180).contains(&y) {
                [(x % 16 * 16) as u8, 40, 90]
            } else {
                [250, 250, 250]
            }
        })
    }

    #[test]
    fn identical_images_score_one_and_changes_lower_it() {
        let original = page([200, 40, 40], true);
        let same = diff(&original, &original).unwrap();
        assert!(same.score > 0.999, "{}", same.score);
        assert_eq!(&same.heatmap_png[1..4], b"PNG");

        let missing_card = diff(&original, &page([200, 40, 40], false)).unwrap();
        assert!(missing_card.score < 0.9, "{}", missing_card.score);
        assert!(missing_card.worst_region.contains("left") || missing_card.worst_region == "center");

        // Same layout, different header colour: brightness-only SSIM would miss this.
        let recolored = diff(&original, &page([40, 40, 200], true)).unwrap();
        assert!(recolored.score < same.score - 0.02, "{}", recolored.score);
        assert!(recolored.worst_region.starts_with("top"), "{}", recolored.worst_region);
        assert!(missing_card.score < recolored.score);
    }

    #[test]
    fn different_sizes_are_compared_at_the_original_size() {
        let original = page([10, 10, 10], true);
        let bigger = test_png(640, 480, |_, _| [250, 250, 250]);
        let result = diff(&original, &bigger).unwrap();
        assert_eq!(dimensions(&result.heatmap_png), Some((320, 240)));
        assert!(result.score < 0.95);
        assert!(diff(b"not an image", &original).is_err());
    }
}
