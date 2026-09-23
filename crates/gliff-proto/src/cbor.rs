//! Decoding helpers for values a newer peer may extend.

use minicbor::data::Type;
use minicbor::decode::{Decoder, Error};
use minicbor::Decode;

/// Decode an optional enum field, reading a value this build does not know
/// as `None`. minicbor's own `Option` decoding mishandles an unknown
/// `index_only` value and misreads the fields after it, so every optional
/// enum field uses this with `nil = "crate::cbor::none"`.
pub fn optional<'b, Ctx, T: Decode<'b, Ctx>>(
    d: &mut Decoder<'b>,
    ctx: &mut Ctx,
) -> Result<Option<T>, Error> {
    if d.datatype()? == Type::Null {
        d.skip()?;
        return Ok(None);
    }
    let start = d.position();
    match T::decode(d, ctx) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.is_unknown_variant() => {
            d.set_position(start);
            d.skip()?;
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// The value of an absent [`optional`] field.
pub fn none<T>() -> Option<Option<T>> {
    Some(None)
}

/// Decode a list, dropping entries whose enum variant this build does not
/// know. Used where a peer describes itself before any features are agreed,
/// so a newer peer's additions cannot make the whole message fail.
pub fn known<'b, Ctx, T: Decode<'b, Ctx>>(
    d: &mut Decoder<'b>,
    ctx: &mut Ctx,
) -> Result<Vec<T>, Error> {
    let len = d.array()?;
    let mut out = Vec::new();
    let mut seen = 0u64;
    loop {
        match len {
            Some(n) if seen == n => break,
            None if d.datatype()? == Type::Break => {
                d.skip()?;
                break;
            }
            _ => {}
        }
        let start = d.position();
        match T::decode(d, ctx) {
            Ok(v) => out.push(v),
            Err(e) if e.is_unknown_variant() => {
                d.set_position(start);
                d.skip()?;
            }
            Err(e) => return Err(e),
        }
        seen += 1;
    }
    Ok(out)
}
