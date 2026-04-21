//! Numbers.

use crate::math::{powi_f64, trunc_f64};
use crate::object::macros::object;
use crate::object::{Object, ObjectLike};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::trivia::{is_regular_character, is_white_space_character};
use core::fmt::Debug;

#[rustfmt::skip]
static POWERS_OF_10: [f64; 20] = [
    1.0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9,
    1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19,
];

/// A number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Number(pub(crate) InternalNumber);

impl Number {
    /// The number zero.
    pub const ZERO: Self = Self::from_i32(0);
    /// The number one.
    pub const ONE: Self = Self::from_i32(1);

    /// Returns the number as a f64.
    pub fn as_f64(&self) -> f64 {
        match self.0 {
            InternalNumber::Real(r) => r,
            InternalNumber::Integer(i) => i as f64,
        }
    }

    /// Returns the number as a f32.
    pub fn as_f32(&self) -> f32 {
        match self.0 {
            InternalNumber::Real(r) => r as f32,
            InternalNumber::Integer(i) => i as f32,
        }
    }

    /// Returns the number as an i64.
    pub fn as_i64(&self) -> i64 {
        match self.0 {
            InternalNumber::Real(r) => {
                let res = r as i64;

                if !(trunc_f64(r) == r) {
                    debug!("float {r} was truncated to {res}");
                }

                res
            }
            InternalNumber::Integer(i) => i,
        }
    }

    /// Create a new `Number` from an f32 number.
    pub const fn from_f32(num: f32) -> Self {
        Self(InternalNumber::Real(num as f64))
    }

    /// Create a new `Number` from an i32 number.
    pub const fn from_i32(num: i32) -> Self {
        Self(InternalNumber::Integer(num as i64))
    }

    /// Returns the lexical kind of this number.
    ///
    /// The kind reflects how the number was written in the source PDF: an
    /// `Integer` was parsed without a decimal point, whereas a `Real` was
    /// parsed with one. This distinction is required by downstream
    /// conformance tooling (for example, PDF/A integer-range checks) that
    /// must treat `1` and `1.0` differently even though they compare equal
    /// numerically.
    #[inline]
    pub const fn kind(&self) -> NumberKind {
        match self.0 {
            InternalNumber::Integer(_) => NumberKind::Integer,
            InternalNumber::Real(_) => NumberKind::Real,
        }
    }

    /// Returns `true` if this number was parsed as an integer literal.
    #[inline]
    pub const fn is_integer(&self) -> bool {
        matches!(self.0, InternalNumber::Integer(_))
    }

    /// Returns `true` if this number was parsed as a real literal (i.e. it
    /// contained a decimal point in the source).
    #[inline]
    pub const fn is_real(&self) -> bool {
        matches!(self.0, InternalNumber::Real(_))
    }
}

/// The lexical kind of a [`Number`].
///
/// Distinguishes between integer literals (e.g. `42`) and real literals
/// (e.g. `42.0`) as written in the source PDF. See [`Number::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NumberKind {
    /// An integer literal — written without a decimal point.
    Integer,
    /// A real literal — written with a decimal point.
    Real,
}

impl Skippable for Number {
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        let has_sign = r.forward_if(|b| b == b'+' || b == b'-').is_some();

        // Some PDFs have weird trailing minuses, so try to accept those as well.
        match r.peek_byte()? {
            b'.' => {
                r.read_byte()?;
                // See PDFJS-9252 - treat a single . as 0.
                r.forward_while(is_digit_or_minus);
            }
            b'0'..=b'9' | b'-' => {
                r.forward_while_1(is_digit_or_minus)?;
                if let Some(()) = r.forward_tag(b".") {
                    r.forward_while(is_digit_or_minus);
                }
            }
            // See PDFJS-bug1753983 - accept just + or - as a zero.
            // ALso see PDFJS-bug1953099, where the sign is followed by a show
            // text string operand, requiring us to allow '<' and '(' as well.
            b if has_sign && (is_white_space_character(b) || matches!(b, b'(' | b'<')) => {}
            _ => return None,
        }

        // See issue 994. Don't accept numbers that are followed by a regular character.
        if r.peek_byte().is_some_and(is_regular_character) {
            return None;
        }

        Some(())
    }
}

impl Readable<'_> for Number {
    #[inline]
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        let old_offset = r.offset();
        read_inner(r).or_else(|| {
            r.jump(old_offset);
            None
        })
    }
}

#[inline(always)]
fn read_inner(r: &mut Reader<'_>) -> Option<Number> {
    let negative = match r.peek_byte()? {
        b'-' => {
            r.forward();
            true
        }
        b'+' => {
            r.forward();
            false
        }
        _ => false,
    };

    let mut mantissa: u64 = 0;
    let mut has_dot = false;
    let mut decimal_shift: u32 = 0;
    let mut has_digits = false;

    loop {
        match r.peek_byte() {
            Some(b'0'..=b'9') => {
                let d = r.read_byte().unwrap();
                mantissa = mantissa
                    // Using `saturating` would arguably be better here, but
                    // profiling showed that it seems to be more expensive, at least
                    // on ARM. Since such large numbers shouldn't appear anyway,
                    // it doesn't really matter a lot what mode we use.
                    .wrapping_mul(10)
                    .wrapping_add((d - b'0') as u64);
                has_digits = true;
                if has_dot {
                    decimal_shift += 1;
                }
            }
            Some(b'.') if !has_dot => {
                r.forward();
                has_dot = true;
            }
            // Some weird PDFs have trailing minus in the fraction of number.
            Some(b'-') if has_digits => {
                r.forward();
                r.forward_while(is_digit_or_minus);
                break;
            }
            _ => break,
        }
    }

    if !has_digits {
        if negative || has_dot {
            // Treat numbers like just `-`, `+` or `-.` as zero.
            return Some(Number(InternalNumber::Integer(0)));
        }
        return None;
    }

    // See issue 994. Don't accept numbers that are followed by a regular character
    // without any white space in-between.
    if r.peek_byte().is_some_and(is_regular_character) {
        return None;
    }

    if !has_dot {
        let value = if negative {
            (mantissa as i64).wrapping_neg()
        } else {
            mantissa as i64
        };
        Some(Number(InternalNumber::Integer(value)))
    } else {
        let mut value = mantissa as f64;

        if decimal_shift > 0 {
            if decimal_shift < POWERS_OF_10.len() as u32 {
                value /= POWERS_OF_10[decimal_shift as usize];
            } else {
                value /= powi_f64(10.0, decimal_shift);
            }
        }

        if negative {
            value = -value;
        }

        Some(Number(InternalNumber::Real(value)))
    }
}

object!(Number, Number);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum InternalNumber {
    Real(f64),
    Integer(i64),
}

macro_rules! int_num {
    ($i:ident) => {
        impl Skippable for $i {
            fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
                r.forward_if(|b| b == b'+' || b == b'-');
                r.forward_while_1(is_digit)?;

                // We have a float instead of an integer.
                if r.peek_byte() == Some(b'.') {
                    return None;
                }

                // See issue 994. Don't accept numbers that are followed by a regular character
                // without any white space in-between.
                if r.peek_byte().is_some_and(is_regular_character) {
                    return None;
                }

                Some(())
            }
        }

        impl<'a> Readable<'a> for $i {
            fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<$i> {
                r.read::<Number>(ctx)
                    .map(|n| n.as_i64())
                    .and_then(|n| n.try_into().ok())
            }
        }

        impl TryFrom<Object<'_>> for $i {
            type Error = ();

            fn try_from(value: Object<'_>) -> core::result::Result<Self, Self::Error> {
                match value {
                    Object::Number(n) => n.as_i64().try_into().ok().ok_or(()),
                    _ => Err(()),
                }
            }
        }

        impl<'a> ObjectLike<'a> for $i {}
    };
}

int_num!(i32);
int_num!(i64);
int_num!(u32);
int_num!(u16);
int_num!(usize);
int_num!(u8);

impl Skippable for f32 {
    fn skip(r: &mut Reader<'_>, is_content_stream: bool) -> Option<()> {
        r.skip::<Number>(is_content_stream).map(|_| {})
    }
}

impl Readable<'_> for f32 {
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        r.read_without_context::<Number>()
            .map(|n| n.as_f64() as Self)
    }
}

impl TryFrom<Object<'_>> for f32 {
    type Error = ();

    fn try_from(value: Object<'_>) -> Result<Self, Self::Error> {
        match value {
            Object::Number(n) => Ok(n.as_f64() as Self),
            _ => Err(()),
        }
    }
}

impl ObjectLike<'_> for f32 {}

impl Skippable for f64 {
    fn skip(r: &mut Reader<'_>, is_content_stream: bool) -> Option<()> {
        r.skip::<Number>(is_content_stream).map(|_| {})
    }
}

impl Readable<'_> for f64 {
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        r.read_without_context::<Number>().map(|n| n.as_f64())
    }
}

impl TryFrom<Object<'_>> for f64 {
    type Error = ();

    fn try_from(value: Object<'_>) -> Result<Self, Self::Error> {
        match value {
            Object::Number(n) => Ok(n.as_f64()),
            _ => Err(()),
        }
    }
}

impl ObjectLike<'_> for f64 {}

pub(crate) fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

pub(crate) fn is_digit_or_minus(byte: u8) -> bool {
    is_digit(byte) || byte == b'-'
}

#[cfg(test)]
mod tests {
    use crate::object::{Number, NumberKind};
    use crate::reader::Reader;
    use crate::reader::ReaderExt;

    fn parse(bytes: &[u8]) -> Number {
        Reader::new(bytes)
            .read_without_context::<Number>()
            .expect("number should parse")
    }

    #[test]
    fn kind_integer_positive() {
        let n = parse(b"42");
        assert_eq!(n.kind(), NumberKind::Integer);
        assert!(n.is_integer());
        assert!(!n.is_real());
    }

    #[test]
    fn kind_integer_negative() {
        let n = parse(b"-42");
        assert_eq!(n.kind(), NumberKind::Integer);
        assert!(n.is_integer());
    }

    #[test]
    fn kind_integer_zero() {
        let n = parse(b"0");
        assert_eq!(n.kind(), NumberKind::Integer);
    }

    #[test]
    fn kind_integer_signed_zero_dash() {
        // A lone `-` is treated as zero integer by the lenient parser.
        let n = parse(b"-");
        assert_eq!(n.kind(), NumberKind::Integer);
    }

    #[test]
    fn kind_integer_dash_dot() {
        // `-.` is also treated as a zero integer despite the dot, because
        // the parser falls through to the `has_digits == false` branch which
        // returns an integer.
        let n = parse(b"-.");
        assert_eq!(n.kind(), NumberKind::Integer);
    }

    #[test]
    fn kind_real_with_dot() {
        let n = parse(b"42.0");
        assert_eq!(n.kind(), NumberKind::Real);
        assert!(n.is_real());
        assert!(!n.is_integer());
    }

    #[test]
    fn kind_real_leading_dot() {
        let n = parse(b".5");
        assert_eq!(n.kind(), NumberKind::Real);
    }

    #[test]
    fn kind_real_negative_leading_dot() {
        let n = parse(b"-.345");
        assert_eq!(n.kind(), NumberKind::Real);
    }

    #[test]
    fn kind_real_trailing_dot() {
        let n = parse(b"7.");
        assert_eq!(n.kind(), NumberKind::Real);
    }

    #[test]
    fn kind_real_when_value_is_whole() {
        // Same numeric value as `7` but written with a decimal: must report
        // `Real`, not `Integer`. This is the core invariant consumers rely on.
        let integer = parse(b"7");
        let real = parse(b"7.0");
        assert_eq!(integer.as_f64(), real.as_f64());
        assert_eq!(integer.kind(), NumberKind::Integer);
        assert_eq!(real.kind(), NumberKind::Real);
    }

    #[test]
    fn kind_const_constructors() {
        // `Number::from_i32` yields an integer; `Number::from_f32` yields a real.
        assert_eq!(Number::from_i32(1).kind(), NumberKind::Integer);
        assert_eq!(Number::from_f32(1.0).kind(), NumberKind::Real);
        assert!(Number::ZERO.is_integer());
        assert!(Number::ONE.is_integer());
    }

    #[test]
    fn kind_is_integer_and_is_real_are_mutually_exclusive() {
        for input in [
            b"0".as_slice(),
            b"1".as_slice(),
            b"-99".as_slice(),
            b"0.0".as_slice(),
            b"-0.5".as_slice(),
            b".25".as_slice(),
        ] {
            let n = parse(input);
            assert_ne!(n.is_integer(), n.is_real(), "input: {input:?}");
        }
    }

    #[test]
    fn int_1() {
        assert_eq!(
            Reader::new("0".as_bytes())
                .read_without_context::<i32>()
                .unwrap(),
            0
        );
    }

    #[test]
    fn int_3() {
        assert_eq!(
            Reader::new("+32".as_bytes())
                .read_without_context::<i32>()
                .unwrap(),
            32
        );
    }

    #[test]
    fn int_4() {
        assert_eq!(
            Reader::new("-32".as_bytes())
                .read_without_context::<i32>()
                .unwrap(),
            -32
        );
    }

    #[test]
    fn int_6() {
        assert_eq!(
            Reader::new("98349".as_bytes())
                .read_without_context::<i32>()
                .unwrap(),
            98349
        );
    }

    #[test]
    fn int_7() {
        assert_eq!(
            Reader::new("003245".as_bytes())
                .read_without_context::<i32>()
                .unwrap(),
            3245
        );
    }

    #[test]
    fn int_min_does_not_panic() {
        assert_eq!(
            Reader::new("-9223372036854775808".as_bytes())
                .read_without_context::<i64>()
                .unwrap(),
            i64::MIN
        );
    }

    #[test]
    fn real_1() {
        assert_eq!(
            Reader::new("3".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            3.0
        );
    }

    #[test]
    fn real_3() {
        assert_eq!(
            Reader::new("+32".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            32.0
        );
    }

    #[test]
    fn real_4() {
        assert_eq!(
            Reader::new("-32".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -32.0
        );
    }

    #[test]
    fn real_5() {
        assert_eq!(
            Reader::new("-32.01".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -32.01
        );
    }

    #[test]
    fn real_6() {
        assert_eq!(
            Reader::new("-.345".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -0.345
        );
    }

    #[test]
    fn real_7() {
        assert_eq!(
            Reader::new("-.00143".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -0.00143
        );
    }

    #[test]
    fn real_8() {
        assert_eq!(
            Reader::new("-12.0013".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -12.0013
        );
    }

    #[test]
    fn real_9() {
        assert_eq!(
            Reader::new("98349.432534".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            98_349.43
        );
    }

    #[test]
    fn real_10() {
        assert_eq!(
            Reader::new("-34534656.34".as_bytes())
                .read_without_context::<f32>()
                .unwrap(),
            -34534656.34
        );
    }

    #[test]
    fn real_failing() {
        assert!(
            Reader::new("+abc".as_bytes())
                .read_without_context::<f32>()
                .is_none()
        );
    }

    #[test]
    fn number_1() {
        assert_eq!(
            Reader::new("+32".as_bytes())
                .read_without_context::<Number>()
                .unwrap()
                .as_f64() as f32,
            32.0
        );
    }

    #[test]
    fn number_2() {
        assert_eq!(
            Reader::new("-32.01".as_bytes())
                .read_without_context::<Number>()
                .unwrap()
                .as_f64() as f32,
            -32.01
        );
    }

    #[test]
    fn number_3() {
        assert_eq!(
            Reader::new("-.345".as_bytes())
                .read_without_context::<Number>()
                .unwrap()
                .as_f64() as f32,
            -0.345
        );
    }

    #[test]
    fn large_number() {
        assert_eq!(
            Reader::new("38359922".as_bytes())
                .read_without_context::<Number>()
                .unwrap()
                .as_i64(),
            38359922
        );
    }

    #[test]
    fn large_number_2() {
        assert_eq!(
            Reader::new("4294966260".as_bytes())
                .read_without_context::<u32>()
                .unwrap(),
            4294966260
        );
    }
}
