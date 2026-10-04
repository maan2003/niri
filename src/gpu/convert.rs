//! Conversions between smithay types and the wire protocol.

use smithay::utils::{Point, Rectangle, Size, Transform as STransform};

use super::protocol::{Rect, Transform};

pub fn rect<C>(r: Rectangle<i32, C>) -> Rect<i32> {
    Rect {
        x: r.loc.x,
        y: r.loc.y,
        w: r.size.w,
        h: r.size.h,
    }
}

pub fn rect_f64<C>(r: Rectangle<f64, C>) -> Rect<f64> {
    Rect {
        x: r.loc.x,
        y: r.loc.y,
        w: r.size.w,
        h: r.size.h,
    }
}

pub fn rects<C>(r: &[Rectangle<i32, C>]) -> Vec<Rect<i32>> {
    r.iter().copied().map(rect).collect()
}

pub fn to_rect<C>(r: Rect<i32>) -> Rectangle<i32, C> {
    Rectangle::new(Point::from((r.x, r.y)), Size::from((r.w, r.h)))
}

pub fn to_rect_f64<C>(r: Rect<f64>) -> Rectangle<f64, C> {
    Rectangle::new(Point::from((r.x, r.y)), Size::from((r.w, r.h)))
}

pub fn to_rects<C>(r: &[Rect<i32>]) -> Vec<Rectangle<i32, C>> {
    r.iter().copied().map(to_rect).collect()
}

pub fn transform(t: STransform) -> Transform {
    match t {
        STransform::Normal => Transform::Normal,
        STransform::_90 => Transform::_90,
        STransform::_180 => Transform::_180,
        STransform::_270 => Transform::_270,
        STransform::Flipped => Transform::Flipped,
        STransform::Flipped90 => Transform::Flipped90,
        STransform::Flipped180 => Transform::Flipped180,
        STransform::Flipped270 => Transform::Flipped270,
    }
}

pub fn to_transform(t: Transform) -> STransform {
    match t {
        Transform::Normal => STransform::Normal,
        Transform::_90 => STransform::_90,
        Transform::_180 => STransform::_180,
        Transform::_270 => STransform::_270,
        Transform::Flipped => STransform::Flipped,
        Transform::Flipped90 => STransform::Flipped90,
        Transform::Flipped180 => STransform::Flipped180,
        Transform::Flipped270 => STransform::Flipped270,
    }
}
