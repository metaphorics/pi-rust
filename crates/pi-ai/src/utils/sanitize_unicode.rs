/// The TypeScript implementation removes unpaired UTF-16 surrogates.
/// Rust `str` contains only Unicode scalar values, so such surrogates cannot be
/// represented and the equivalent operation is a zero-cost identity.
#[inline]
pub fn sanitize_surrogates(text: &str) -> &str {
    text
}

/// Sanitizes text at a UTF-16 boundary, removing isolated surrogate code
/// units while preserving valid pairs. Use this before constructing a Rust
/// `String` from untrusted UTF-16 input.
pub fn sanitize_utf16_surrogates(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .filter_map(Result::ok)
        .collect()
}
