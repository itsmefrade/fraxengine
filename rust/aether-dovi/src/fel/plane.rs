use crate::fel::{Error, Result};
/// Immutable borrowed plane of unshifted 10-bit codes, with stride in samples.
/// Construction validates all active samples; row padding is ignored.
#[derive(Clone, Copy, Debug)]
pub struct Plane<'a> {
    data: &'a [u16],
    width: usize,
    height: usize,
    stride: usize,
}
impl<'a> Plane<'a> {
    pub fn new(data: &'a [u16], width: usize, height: usize, stride: usize) -> Result<Self> {
        if width == 0 || height == 0 || width > 8192 || height > 4320 || stride < width {
            return Err(Error::Dimensions);
        }
        let required = (height - 1)
            .checked_mul(stride)
            .and_then(|v| v.checked_add(width))
            .ok_or(Error::Overflow)?;
        if data.len() < required {
            return Err(Error::Buffer);
        }
        for row in 0..height {
            if data[row * stride..row * stride + width]
                .iter()
                .any(|&v| v > 1023)
            {
                return Err(Error::SampleRange);
            }
        }
        Ok(Self {
            data,
            width,
            height,
            stride,
        })
    }
    pub fn width(self) -> usize {
        self.width
    }
    pub fn height(self) -> usize {
        self.height
    }
    pub(crate) fn get(self, x: usize, y: usize) -> u16 {
        self.data[y * self.stride + x]
    }
    pub(crate) fn edge(self, x: isize, y: isize) -> i32 {
        i32::from(self.get(
            x.clamp(0, self.width as isize - 1) as usize,
            y.clamp(0, self.height as isize - 1) as usize,
        ))
    }
}
/// Planar YCbCr 4:2:0. Frame dimensions derive from Y; chroma uses ceil division.
#[derive(Clone, Copy, Debug)]
pub struct Frame420<'a> {
    pub(crate) y: Plane<'a>,
    pub(crate) cb: Plane<'a>,
    pub(crate) cr: Plane<'a>,
}
impl<'a> Frame420<'a> {
    pub fn new(y: Plane<'a>, cb: Plane<'a>, cr: Plane<'a>) -> Result<Self> {
        if [cb, cr]
            .iter()
            .any(|p| p.width != y.width.div_ceil(2) || p.height != y.height.div_ceil(2))
        {
            return Err(Error::Dimensions);
        }
        Ok(Self { y, cb, cr })
    }
}
