use tower_lsp_server::ls_types::Color;

pub fn catalog_color(index: usize, count: usize, alpha: f32) -> Color {
    let hue = index as f32 / count.max(1) as f32 * 360.0;
    let (red, green, blue) = hsl_to_rgb(hue, 0.65, 0.55);
    Color {
        red,
        green,
        blue,
        alpha,
    }
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (f32, f32, f32) {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = hue / 60.0;
    let x = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let (r, g, b) = match sector as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = lightness - chroma / 2.0;
    (r + m, g + m, b + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_are_spread_around_the_wheel_and_keep_alpha() {
        let first = catalog_color(0, 3, 0.6);
        let second = catalog_color(1, 3, 0.6);
        let third = catalog_color(2, 3, 0.6);
        assert_eq!(first.alpha, 0.6);
        assert!(first.red > first.green && first.red > first.blue);
        assert!(second.green > second.red && second.green > second.blue);
        assert!(third.blue > third.red && third.blue > third.green);
        for color in [first, second, third] {
            for channel in [color.red, color.green, color.blue] {
                assert!((0.0..=1.0).contains(&channel));
            }
        }
    }
}
