use crate::fel::Plane;
/// Centre-aligned bilinear luma, half-up rounding, replicated borders.
pub(crate) fn enhancement_luma(
    p: Plane<'_>,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> u16 {
    bilinear(p, x, y, width, height, 0.5)
}
fn bilinear(p: Plane<'_>, x: usize, y: usize, width: usize, height: usize, centre: f64) -> u16 {
    let u = (x as f64 + centre) * p.width() as f64 / width as f64 - centre;
    let v = (y as f64 + centre) * p.height() as f64 / height as f64 - centre;
    let (a, b) = (u.floor() as isize, v.floor() as isize);
    let (fx, fy) = (u - u.floor(), v - v.floor());
    let top = f64::from(p.edge(a, b)) * (1.0 - fx) + f64::from(p.edge(a + 1, b)) * fx;
    let bot = f64::from(p.edge(a, b + 1)) * (1.0 - fx) + f64::from(p.edge(a + 1, b + 1)) * fx;
    (top * (1.0 - fy) + bot * fy + 0.5)
        .floor()
        .clamp(0.0, 1023.0) as u16
}
fn vertical(p: Plane<'_>, x: isize, y: usize) -> i32 {
    let n = (y / 2) as isize;
    if y & 1 == 0 {
        (64 * p.edge(x, n - 1) + 192 * p.edge(x, n) + 128) >> 8
    } else {
        (192 * p.edge(x, n) + 64 * p.edge(x, n + 1) + 128) >> 8
    }
}
/// Half-size chroma follows the upstream Annex B separable integer filter;
/// matched grids are exact. Negative filter lobes are clamped for NLQ indexing.
pub(crate) fn enhancement_chroma(
    p: Plane<'_>,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> u16 {
    if width.div_ceil(2) == p.width() && height.div_ceil(2) == p.height() {
        let m = (x / 2) as isize;
        let value = if x & 1 == 0 {
            vertical(p, m, y)
        } else {
            let taps = [22, 94, -524, 2456, 2456, -524, 94, 22];
            (taps
                .iter()
                .enumerate()
                .map(|(i, t)| t * vertical(p, m + i as isize - 3, y))
                .sum::<i32>()
                + 2048)
                >> 12
        };
        value.clamp(0, 1023) as u16
    } else {
        bilinear(p, x, y, width, height, 0.0)
    }
}
/// Luma at a chroma site: horizontal [1,2,1] rounded per row, then rounded average.
pub(crate) fn chroma_site_luma(p: Plane<'_>, x: usize, y: usize) -> u16 {
    let x = (x * 2) as isize;
    let y = (y * 2) as isize;
    let row = |y| (p.edge(x - 1, y) + 2 * p.edge(x, y) + p.edge(x + 1, y) + 2) >> 2;
    ((row(y) + row(y + 1) + 1) >> 1) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bilinear_half_up_centres_and_edges() {
        let p = Plane::new(&[0, 2, 4, 6], 2, 2, 2).unwrap();
        assert_eq!(
            (0..4)
                .map(|x| enhancement_luma(p, x, 0, 4, 4))
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 2]
        );
        assert_eq!(enhancement_luma(p, 3, 3, 4, 4), 6);
        assert_eq!(enhancement_luma(p, 1, 1, 2, 2), 6);
    }
    #[test]
    fn annexb_constant_preserving_odd_grid_and_clamped_lobes() {
        let flat = [876; 6];
        let p = Plane::new(&flat, 3, 2, 3).unwrap();
        for y in 0..3 {
            for x in 0..5 {
                assert_eq!(enhancement_chroma(p, x, y, 5, 3), 876);
            }
        }
        let sharp = [0, 0, 1023, 1023, 0, 0, 1023, 1023];
        let p = Plane::new(&sharp, 4, 2, 4).unwrap();
        assert_eq!(enhancement_chroma(p, 0, 0, 8, 4), 0);
        assert_eq!(enhancement_chroma(p, 3, 0, 8, 4), 512);
        assert_eq!(enhancement_chroma(p, 5, 0, 8, 4), 1023);
        assert_eq!(enhancement_chroma(p, 1, 0, 8, 4), 0);
    }
    #[test]
    fn chroma_site_uses_rounded_horizontal_filter_then_rows() {
        let p = Plane::new(&[0, 100, 0, 0, 0, 0, 0, 0, 0, 0], 5, 2, 5).unwrap();
        assert_eq!(chroma_site_luma(p, 0, 0), 13);
        assert_eq!(chroma_site_luma(p, 1, 0), 13);
        assert_eq!(chroma_site_luma(p, 2, 0), 0);
    }
}
