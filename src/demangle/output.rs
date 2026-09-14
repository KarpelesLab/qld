//! A bounded string writer shared by the demanglers.
//!
//! Substitutions let a short mangled name expand to an exponentially long
//! demangled one, so every write is checked against a size limit, and every
//! printing step spends "fuel". Once either runs out, the writer stops
//! accepting text and the demangler reports failure.

use super::{MAX_OUTPUT, PRINT_FUEL};

/// Output buffer with a size cap and a step budget.
#[derive(Debug)]
pub(crate) struct Output {
    buf: String,
    limit: usize,
    fuel: usize,
    failed: bool,
    /// The last character appended. Truncation does not reset it, which
    /// `c++filt`'s spacing depends on.
    last: Option<char>,
}

impl Output {
    /// An empty buffer with the crate-wide limits, and a step budget that
    /// grows with the length of the name being demangled (real names need
    /// well under 16 steps per input byte).
    pub(crate) fn for_input(len: usize) -> Self {
        let fuel = len.saturating_mul(256).saturating_add(1 << 14);
        Self::with_limits(MAX_OUTPUT, fuel.min(PRINT_FUEL))
    }

    /// An empty buffer with explicit limits (tests use small ones).
    pub(crate) fn with_limits(limit: usize, fuel: usize) -> Self {
        Self {
            buf: String::new(),
            limit,
            fuel,
            failed: false,
            last: None,
        }
    }

    /// Appends `text`, or marks the output failed if it would exceed the cap.
    pub(crate) fn push(&mut self, text: &str) {
        if self.failed {
            return;
        }
        match self.buf.len().checked_add(text.len()) {
            Some(len) if len <= self.limit => {
                self.buf.push_str(text);
                if let Some(c) = text.chars().next_back() {
                    self.last = Some(c);
                }
            }
            _ => self.failed = true,
        }
    }

    /// Appends one character.
    pub(crate) fn push_char(&mut self, c: char) {
        let mut tmp = [0u8; 4];
        self.push(c.encode_utf8(&mut tmp));
    }

    /// Appends bytes that are expected to be ASCII identifiers; anything else
    /// is replaced lossily.
    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
        match std::str::from_utf8(bytes) {
            Ok(text) => self.push(text),
            Err(_) => {
                let text = String::from_utf8_lossy(bytes);
                self.push(&text);
            }
        }
    }

    /// Appends a decimal number.
    pub(crate) fn push_u64(&mut self, value: u64) {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        let mut rest = value;
        loop {
            at = at.saturating_sub(1);
            if let Some(slot) = digits.get_mut(at) {
                // `rest % 10` is below 10, so the addition cannot overflow.
                *slot = b'0'.saturating_add((rest % 10) as u8);
            }
            rest /= 10;
            if rest == 0 || at == 0 {
                break;
            }
        }
        self.push_bytes(digits.get(at..).unwrap_or_default());
    }

    /// Spends one unit of fuel; returns `false` (and fails the output) when
    /// none is left.
    pub(crate) fn step(&mut self) -> bool {
        if self.failed {
            return false;
        }
        match self.fuel.checked_sub(1) {
            Some(rest) => {
                self.fuel = rest;
                true
            }
            None => {
                self.failed = true;
                false
            }
        }
    }

    /// Marks the output as failed.
    pub(crate) fn fail(&mut self) {
        self.failed = true;
    }

    /// Whether a limit was hit or an error was reported.
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }

    /// The last character appended, if any, even if it was since removed
    /// by [`Output::truncate`].
    pub(crate) fn last_char(&self) -> Option<char> {
        self.last
    }

    /// Current length in bytes.
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// Truncates back to `len` bytes (a length previously returned by
    /// [`Output::len`]).
    pub(crate) fn truncate(&mut self, len: usize) {
        if len <= self.buf.len() && self.buf.is_char_boundary(len) {
            self.buf.truncate(len);
        }
    }

    /// Consumes the buffer, returning the text unless a limit was hit.
    pub(crate) fn finish(self) -> Option<String> {
        if self.failed { None } else { Some(self.buf) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_size_and_fuel() {
        let mut out = Output::with_limits(4, 2);
        out.push("abc");
        out.push("de");
        assert!(out.failed());
        let mut out = Output::with_limits(100, 2);
        assert!(out.step());
        assert!(out.step());
        assert!(!out.step());
        assert!(out.finish().is_none());
    }

    #[test]
    fn numbers() {
        let mut out = Output::for_input(0);
        out.push_u64(0);
        out.push(" ");
        out.push_u64(u64::MAX);
        assert_eq!(out.finish().as_deref(), Some("0 18446744073709551615"));
    }
}
