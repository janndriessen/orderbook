//! Decimal strings to integer ticks and lots, with checked arithmetic.

use protocol::{MarketInfo, Price, Qty};

/// Largest accepted value in ticks or lots: the protocol's documented limit,
/// below which values stay exact when read as JavaScript numbers.
pub(crate) const MAX_VALUE: u64 = (1 << 53) - 1;

/// Why a decimal string was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecimalError {
    /// The string is empty.
    #[error("empty string")]
    Empty,
    /// There are no digits before the decimal point.
    #[error("no digits before the decimal point")]
    MissingIntegerDigits,
    /// A decimal point is not followed by a digit.
    #[error("no digits after the decimal point")]
    MissingFractionDigits,
    /// The string starts with `+` or `-`.
    #[error("explicit sign")]
    Sign,
    /// The string contains whitespace.
    #[error("whitespace")]
    Whitespace,
    /// The string uses exponent notation.
    #[error("exponent notation")]
    Exponent,
    /// The string contains a character that is not a digit or a single `.`.
    #[error("invalid character {0:?}")]
    InvalidChar(char),
    /// Non-zero digits remain beyond the market's decimal places.
    #[error("more than {0} significant decimal places")]
    TooManyDecimals(u8),
    /// The value exceeds 2^53 - 1 after scaling.
    #[error("value above {MAX_VALUE} after scaling")]
    OutOfRange,
}

/// Parses a price such as `"84579.0"` into ticks of the market.
///
/// # Errors
///
/// Returns a [`DecimalError`] if `text` is not a plain non-negative decimal,
/// needs more decimal places than `market.price_decimals`, or scales above
/// 2^53 - 1.
pub fn parse_price(text: &str, market: &MarketInfo) -> Result<Price, DecimalError> {
    let ticks = parse_scaled(text, market.price_decimals)?;
    // At most 2^53 - 1, so it fits.
    Ok(ticks as Price)
}

/// Parses a size such as `"3.07937"` into lots of the market. Zero is
/// accepted; whether a zero size is meaningful is up to the caller.
///
/// # Errors
///
/// Returns a [`DecimalError`] if `text` is not a plain non-negative decimal,
/// needs more decimal places than `market.qty_decimals`, or scales above
/// 2^53 - 1.
pub fn parse_qty(text: &str, market: &MarketInfo) -> Result<Qty, DecimalError> {
    parse_scaled(text, market.qty_decimals)
}

/// Scales `text` by 10^`decimals`. Trailing zeros beyond `decimals` are
/// allowed because the source pads whole numbers (`"84579.0"`).
fn parse_scaled(text: &str, decimals: u8) -> Result<u64, DecimalError> {
    if text.is_empty() {
        return Err(DecimalError::Empty);
    }
    for (i, c) in text.chars().enumerate() {
        match c {
            '0'..='9' | '.' => {}
            '+' | '-' if i == 0 => return Err(DecimalError::Sign),
            'e' | 'E' => return Err(DecimalError::Exponent),
            c if c.is_whitespace() => return Err(DecimalError::Whitespace),
            c => return Err(DecimalError::InvalidChar(c)),
        }
    }
    let (int, frac) = match text.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (text, None),
    };
    if int.is_empty() {
        return Err(DecimalError::MissingIntegerDigits);
    }
    let frac = frac.unwrap_or("0");
    if frac.is_empty() {
        return Err(DecimalError::MissingFractionDigits);
    }
    if frac.contains('.') {
        return Err(DecimalError::InvalidChar('.'));
    }
    let kept = usize::from(decimals).min(frac.len());
    let (frac_kept, frac_extra) = frac.split_at(kept);
    if frac_extra.bytes().any(|b| b != b'0') {
        return Err(DecimalError::TooManyDecimals(decimals));
    }
    let padding = usize::from(decimals) - kept;
    let digits = int
        .bytes()
        .chain(frac_kept.bytes())
        .chain(std::iter::repeat_n(b'0', padding));
    let mut value: u64 = 0;
    for digit in digits {
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(digit - b'0')))
            .filter(|v| *v <= MAX_VALUE)
            .ok_or(DecimalError::OutOfRange)?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market() -> MarketInfo {
        MarketInfo {
            symbol: "BTC-USD".to_string(),
            price_decimals: 0,
            qty_decimals: 5,
        }
    }

    fn price(text: &str) -> Result<Price, DecimalError> {
        parse_price(text, &market())
    }

    fn qty(text: &str) -> Result<Qty, DecimalError> {
        parse_qty(text, &market())
    }

    #[test]
    fn accepts_prices_with_and_without_padding() {
        assert_eq!(price("84579.0"), Ok(84579));
        assert_eq!(price("84579"), Ok(84579));
        assert_eq!(price("0"), Ok(0));
    }

    #[test]
    fn accepts_sizes() {
        assert_eq!(qty("3.07937"), Ok(307_937));
        assert_eq!(qty("0.003"), Ok(300));
        assert_eq!(qty("12.83161"), Ok(1_283_161));
        assert_eq!(qty("0"), Ok(0));
        assert_eq!(qty("1"), Ok(100_000));
    }

    #[test]
    fn accepts_zero_digits_beyond_the_market_decimals() {
        assert_eq!(qty("0.000010"), Ok(1));
        assert_eq!(qty("2.5000000"), Ok(250_000));
        assert_eq!(price("84579.000"), Ok(84579));
    }

    #[test]
    fn scales_by_the_decimals_of_the_given_market() {
        let market = MarketInfo {
            symbol: "TEST-USD".to_string(),
            price_decimals: 1,
            qty_decimals: 0,
        };
        assert_eq!(parse_price("84579.5", &market), Ok(845_795));
        assert_eq!(parse_price("84579", &market), Ok(845_790));
        assert_eq!(parse_qty("3", &market), Ok(3));
        assert_eq!(
            parse_qty("0.5", &market),
            Err(DecimalError::TooManyDecimals(0))
        );
    }

    #[test]
    fn accepts_leading_zeros() {
        assert_eq!(qty("007.5"), Ok(750_000));
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(price(""), Err(DecimalError::Empty));
    }

    #[test]
    fn rejects_lone_point() {
        assert_eq!(qty("."), Err(DecimalError::MissingIntegerDigits));
    }

    #[test]
    fn rejects_missing_integer_digits() {
        assert_eq!(qty(".5"), Err(DecimalError::MissingIntegerDigits));
    }

    #[test]
    fn rejects_missing_fraction_digits() {
        assert_eq!(qty("5."), Err(DecimalError::MissingFractionDigits));
    }

    #[test]
    fn rejects_signs() {
        assert_eq!(qty("+1"), Err(DecimalError::Sign));
        assert_eq!(qty("-1"), Err(DecimalError::Sign));
        assert_eq!(qty("-0"), Err(DecimalError::Sign));
    }

    #[test]
    fn rejects_whitespace_anywhere() {
        assert_eq!(qty(" 1"), Err(DecimalError::Whitespace));
        assert_eq!(qty("1 "), Err(DecimalError::Whitespace));
        assert_eq!(qty("1 .5"), Err(DecimalError::Whitespace));
        assert_eq!(qty("1\n"), Err(DecimalError::Whitespace));
    }

    #[test]
    fn rejects_exponents() {
        assert_eq!(price("1e5"), Err(DecimalError::Exponent));
        assert_eq!(qty("1.5E2"), Err(DecimalError::Exponent));
    }

    #[test]
    fn rejects_other_characters() {
        assert_eq!(qty("1a"), Err(DecimalError::InvalidChar('a')));
        assert_eq!(qty("1,5"), Err(DecimalError::InvalidChar(',')));
        assert_eq!(qty("1.2.3"), Err(DecimalError::InvalidChar('.')));
        assert_eq!(qty("1_0"), Err(DecimalError::InvalidChar('_')));
        assert_eq!(qty("1-2"), Err(DecimalError::InvalidChar('-')));
        assert_eq!(qty("٣"), Err(DecimalError::InvalidChar('٣')));
    }

    #[test]
    fn rejects_significant_digits_beyond_the_market_decimals() {
        assert_eq!(qty("0.000001"), Err(DecimalError::TooManyDecimals(5)));
        assert_eq!(qty("0.0000100001"), Err(DecimalError::TooManyDecimals(5)));
        assert_eq!(price("84579.5"), Err(DecimalError::TooManyDecimals(0)));
    }

    #[test]
    fn bound_is_exact_in_ticks() {
        assert_eq!(price("9007199254740991"), Ok((1 << 53) - 1));
        assert_eq!(price("9007199254740992"), Err(DecimalError::OutOfRange));
    }

    #[test]
    fn bound_is_exact_in_lots() {
        assert_eq!(qty("90071992547.40991"), Ok((1 << 53) - 1));
        assert_eq!(qty("90071992547.40992"), Err(DecimalError::OutOfRange));
    }

    #[test]
    fn rejects_values_that_overflow_machine_integers() {
        let long = "9".repeat(100);
        assert_eq!(price(&long), Err(DecimalError::OutOfRange));
        assert_eq!(qty(&long), Err(DecimalError::OutOfRange));
        assert_eq!(price("18446744073709551616"), Err(DecimalError::OutOfRange));
        assert_eq!(price("9223372036854775808"), Err(DecimalError::OutOfRange));
    }

    #[test]
    fn long_runs_of_leading_zeros_are_fine() {
        let zeros = format!("{}1", "0".repeat(100));
        assert_eq!(price(&zeros), Ok(1));
    }
}
