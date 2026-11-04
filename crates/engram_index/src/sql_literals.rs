use std::fmt;

/// The native planner parses a bare integer above i64::MAX through f64.
/// Casting decimal text retains the full UInt64 storage value.
pub(crate) fn u64_literal(value: u64) -> impl fmt::Display {
    struct Literal(u64);
    impl fmt::Display for Literal {
        fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
            if self.0 <= i64::MAX as u64 {
                write!(output, "{}", self.0)
            } else {
                write!(output, "CAST('{}' AS BIGINT UNSIGNED)", self.0)
            }
        }
    }
    Literal(value)
}
