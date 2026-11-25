//! Canonical numeric grammars shared by the task HTTP faces.

use silverhand_backend_core::conversation_messaging::messaging::task::TaskNumber;

/// Query coordinates use the canonical ASCII spelling of a positive base-10
/// integer. JSON numbers have no spelling once decoded, but query strings do:
/// exponent, hex, signs, whitespace, and leading zeroes are deliberately not
/// aliases for the canonical decimal coordinate.
pub(super) fn parse_canonical_task_number(raw: &str) -> Option<TaskNumber> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() || bytes[0] == b'0' || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    TaskNumber::new(raw.parse::<i64>().ok()?).ok()
}

pub(super) fn parse_canonical_limit(raw: &str, max: usize) -> Option<usize> {
    let value = parse_canonical_task_number(raw)?.get();
    let value = usize::try_from(value).ok()?;
    (value <= max).then_some(value)
}

/// An epoch-millisecond instant in the same canonical decimal spelling.
pub(super) fn parse_canonical_epoch_ms(raw: &str) -> Option<i64> {
    parse_canonical_task_number(raw).map(TaskNumber::get)
}
