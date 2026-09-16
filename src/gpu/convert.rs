//! Conversions between smithay types and the wire protocol.

use std::borrow::Cow;

use smithay::backend::renderer::gles::{Uniform as GlesUniform, UniformValue};
use smithay::utils::{Point, Rectangle, Size, Transform as STransform};

use super::protocol::{Rect, Transform, Uniform, UniformVal};

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

pub fn uniform(u: &GlesUniform<'_>) -> Uniform {
    let value = match &u.value {
        UniformValue::_1f(a) => UniformVal::F1(*a),
        UniformValue::_2f(a, b) => UniformVal::F2(*a, *b),
        UniformValue::_3f(a, b, c) => UniformVal::F3(*a, *b, *c),
        UniformValue::_4f(a, b, c, d) => UniformVal::F4(*a, *b, *c, *d),
        UniformValue::_1i(a) => UniformVal::I1(*a),
        UniformValue::_2i(a, b) => UniformVal::I2(*a, *b),
        UniformValue::_3i(a, b, c) => UniformVal::I3(*a, *b, *c),
        UniformValue::_4i(a, b, c, d) => UniformVal::I4(*a, *b, *c, *d),
        UniformValue::_1ui(a) => UniformVal::U1(*a),
        UniformValue::_2ui(a, b) => UniformVal::U2(*a, *b),
        UniformValue::_3ui(a, b, c) => UniformVal::U3(*a, *b, *c),
        UniformValue::_4ui(a, b, c, d) => UniformVal::U4(*a, *b, *c, *d),
        UniformValue::Matrix2x2 {
            matrices,
            transpose,
        } => UniformVal::Mat2x2 {
            matrices: matrices.clone(),
            transpose: *transpose,
        },
        UniformValue::Matrix2x3 {
            matrices,
            transpose,
        } => UniformVal::Mat2x3 {
            matrices: matrices.clone(),
            transpose: *transpose,
        },
        UniformValue::Matrix2x4 {
            matrices,
            transpose,
        } => UniformVal::Mat2x4 {
            matrices: matrices.clone(),
            transpose: *transpose,
        },
        UniformValue::Matrix3x2 {
            matrices,
            transpose,
        } => UniformVal::Mat3x2 {
            matrices: matrices.clone(),
            transpose: *transpose,
        },
        UniformValue::Matrix3x3 {
            matrices,
            transpose,
        } => UniformVal::Mat3x3 {
            matrices: matrices.clone(),
            transpose: *transpose,
        },
        UniformValue::Matrix3x4 {
            matrices,
            transpose,
        } => UniformVal::Mat3x4 {
            matrices: matrices.clone(),
            transpose: *transpose,
        },
        UniformValue::Matrix4x2 {
            matrices,
            transpose,
        } => UniformVal::Mat4x2 {
            matrices: matrices.clone(),
            transpose: *transpose,
        },
        UniformValue::Matrix4x3 {
            matrices,
            transpose,
        } => UniformVal::Mat4x3 {
            matrices: matrices.clone(),
            transpose: *transpose,
        },
        UniformValue::Matrix4x4 {
            matrices,
            transpose,
        } => UniformVal::Mat4x4 {
            matrices: matrices.clone(),
            transpose: *transpose,
        },
    };
    Uniform {
        name: u.name.to_string(),
        value,
    }
}

pub fn uniforms(u: &[GlesUniform<'_>]) -> Vec<Uniform> {
    u.iter().map(uniform).collect()
}

pub fn to_uniform(u: Uniform) -> GlesUniform<'static> {
    let value = match u.value {
        UniformVal::F1(a) => UniformValue::_1f(a),
        UniformVal::F2(a, b) => UniformValue::_2f(a, b),
        UniformVal::F3(a, b, c) => UniformValue::_3f(a, b, c),
        UniformVal::F4(a, b, c, d) => UniformValue::_4f(a, b, c, d),
        UniformVal::I1(a) => UniformValue::_1i(a),
        UniformVal::I2(a, b) => UniformValue::_2i(a, b),
        UniformVal::I3(a, b, c) => UniformValue::_3i(a, b, c),
        UniformVal::I4(a, b, c, d) => UniformValue::_4i(a, b, c, d),
        UniformVal::U1(a) => UniformValue::_1ui(a),
        UniformVal::U2(a, b) => UniformValue::_2ui(a, b),
        UniformVal::U3(a, b, c) => UniformValue::_3ui(a, b, c),
        UniformVal::U4(a, b, c, d) => UniformValue::_4ui(a, b, c, d),
        UniformVal::Mat2x2 {
            matrices,
            transpose,
        } => UniformValue::Matrix2x2 {
            matrices,
            transpose,
        },
        UniformVal::Mat2x3 {
            matrices,
            transpose,
        } => UniformValue::Matrix2x3 {
            matrices,
            transpose,
        },
        UniformVal::Mat2x4 {
            matrices,
            transpose,
        } => UniformValue::Matrix2x4 {
            matrices,
            transpose,
        },
        UniformVal::Mat3x2 {
            matrices,
            transpose,
        } => UniformValue::Matrix3x2 {
            matrices,
            transpose,
        },
        UniformVal::Mat3x3 {
            matrices,
            transpose,
        } => UniformValue::Matrix3x3 {
            matrices,
            transpose,
        },
        UniformVal::Mat3x4 {
            matrices,
            transpose,
        } => UniformValue::Matrix3x4 {
            matrices,
            transpose,
        },
        UniformVal::Mat4x2 {
            matrices,
            transpose,
        } => UniformValue::Matrix4x2 {
            matrices,
            transpose,
        },
        UniformVal::Mat4x3 {
            matrices,
            transpose,
        } => UniformValue::Matrix4x3 {
            matrices,
            transpose,
        },
        UniformVal::Mat4x4 {
            matrices,
            transpose,
        } => UniformValue::Matrix4x4 {
            matrices,
            transpose,
        },
    };
    GlesUniform {
        name: Cow::Owned(u.name),
        value,
    }
}

pub fn to_uniforms(u: Vec<Uniform>) -> Vec<GlesUniform<'static>> {
    u.into_iter().map(to_uniform).collect()
}
