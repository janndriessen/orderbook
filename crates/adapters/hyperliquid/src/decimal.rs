//! Decimal strings to integer ticks and lots: exact, no floats, and bounded at
//! 2^53 - 1.
//!
//! The accepted format is digits, optionally followed by `.` and more digits:
//! no sign, whitespace, exponent or other character. Zeros beyond the market's
//! decimals are allowed because the source pads whole numbers (`"84579.0"`).

use protocol::{MarketInfo, Price, Qty};

/// Largest accepted value in ticks or lots: the protocol's documented limit,
/// below which values stay exact when read as JavaScript numbers.
pub(crate) const MAX_VALUE: u64 = (1 << 53) - 1;

/// Why a decimal string was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecimalError {
    /// The string is not a plain non-negative decimal.
    #[error("not a plain decimal")]
    Invalid,
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
/// Returns a [`DecimalError`] if `text` is malformed, needs more decimal places
/// than `market.price_decimals`, or scales above 2^53 - 1.
pub(crate) fn parse_price(text: &str, market: &MarketInfo) -> Result<Price, DecimalError> {
    Price::try_from(parse_scaled(text, market.price_decimals)?)
        .map_err(|_| DecimalError::OutOfRange)
}

/// Parses a size such as `"3.07937"` into lots of the market. Zero is
/// accepted; whether a zero size is meaningful is up to the caller.
///
/// # Errors
///
/// Returns a [`DecimalError`] if `text` is malformed, needs more decimal places
/// than `market.qty_decimals`, or scales above 2^53 - 1.
pub(crate) fn parse_qty(text: &str, market: &MarketInfo) -> Result<Qty, DecimalError> {
    parse_scaled(text, market.qty_decimals)
}

fn parse_scaled(text: &str, decimals: u8) -> Result<u64, DecimalError> {
    let (int, frac) = match text.split_once('.') {
        Some((_, "")) => return Err(DecimalError::Invalid),
        Some(parts) => parts,
        None => (text, ""),
    };
    let digits_only = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if int.is_empty() || !digits_only(int) || !digits_only(frac) {
        return Err(DecimalError::Invalid);
    }
    let kept = usize::from(decimals).min(frac.len());
    let (frac_kept, frac_extra) = frac.split_at(kept);
    if frac_extra.bytes().any(|b| b != b'0') {
        return Err(DecimalError::TooManyDecimals(decimals));
    }
    let padding = "0".repeat(usize::from(decimals) - kept);
    let value: u64 = format!("{int}{frac_kept}{padding}")
        .parse()
        .map_err(|_| DecimalError::OutOfRange)?;
    if value > MAX_VALUE {
        return Err(DecimalError::OutOfRange);
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
    fn rejects_text_that_is_not_a_plain_decimal() {
        for text in [
            "", ".", ".5", "5.", "+1", "-1", "-0", " 1", "1 ", "1 .5", "1\n", "1e5", "1.5E2", "1a",
            "1,5", "1.2.3", "1_0", "1-2", "٣",
        ] {
            assert_eq!(qty(text), Err(DecimalError::Invalid), "{text:?}");
        }
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
    fn huge_decimals_do_not_panic() {
        let market = MarketInfo {
            symbol: "TEST-USD".to_string(),
            price_decimals: 255,
            qty_decimals: 255,
        };
        assert_eq!(parse_price("0", &market), Ok(0));
        assert_eq!(parse_qty("0", &market), Ok(0));
        assert_eq!(parse_price("1", &market), Err(DecimalError::OutOfRange));
        assert_eq!(parse_qty("1", &market), Err(DecimalError::OutOfRange));
    }

    #[test]
    fn long_runs_of_leading_zeros_are_fine() {
        let zeros = format!("{}1", "0".repeat(100));
        assert_eq!(price(&zeros), Ok(1));
    }
}
