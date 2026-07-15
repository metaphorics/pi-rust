/// The TypeScript implementation removes unpaired UTF-16 surrogates.
/// Rust `str` contains only Unicode scalar values, so such surrogates cannot be
/// represented and the equivalent operation is a zero-cost identity.
#[inline]
pub fn sanitize_surrogates(text: &str) -> &str {
    text
}
