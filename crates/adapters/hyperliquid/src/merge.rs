//! Merging the `l2` feed's price groupings into one book of single-tick levels.
//!
//! Each grouping shows the 20 best non-empty buckets per side: the finest about
//! $20 of the book, the coarsest about $20,000. No single feed has both the
//! detail near the touch and the depth further out, so the engine is given one
//! book that agrees with all of them: grouped by any of the inputs' rules, it
//! shows exactly that input's buckets.
//!
//! Going outward from the best price, every bucket a grouping lists must end
//! up holding its size. The finest grouping fixes single prices; what a
//! coarser bucket holds beyond the finer levels inside it is placed at the
//! bucket's far end, its last price going outward. Whatever is placed there is
//! capped by what every finer bucket around that price may still hold: a
//! nested finer bucket ends there too, and a $2 bucket around a $5 bucket's
//! far end (the two do not nest) may still have to receive its own other
//! price. For inputs that agree, the cap never binds. Only listed prices can
//! receive anything, so the walk visits those and nothing in between.
//!
//! A grouping that lists fewer than 20 buckets on a side shows the whole side,
//! so nothing may be placed beyond its last bucket.
//!
//! Buckets follow the source's rounding. A grouping's bucket width is one unit
//! of the reference price rounded to its significant figures, times its
//! mantissa: $1 at 5 figures for a price below $100,000, $10 from there. The
//! width is the same across the whole book, as recorded books show: at the
//! $1,000 grouping with the touch near $81,500, asks run to $101,000 in steps
//! of $1,000. The reference is taken to be the best bid of the finest input
//! that has bids: a bid label is rounded down, so it never has more digits
//! than the touch. A book with no bids at all uses the finest input's best
//! ask. An ask label rounded up can reach the next power of ten from below,
//! so a power of ten from a rounded grouping is read as just below it; the
//! finest grouping (5 figures, no mantissa) rounds a touch below a power of
//! ten to itself, so its label is taken as it is. Which price the
//! source uses only matters when the touch is next to a change in the number
//! of digits, and that has not been observed. Bids are labeled by rounding
//! down and asks by rounding up, measured exactly on recorded data. Prices are
//! whole ticks of $1, as the BTC market's are.

use protocol::{Price, Ts};

use crate::decimal::MAX_VALUE;
use crate::message::{BookLevel, BookSnapshot};

/// Buckets per side that the source shows at every grouping; fewer means the
/// side holds no more.
const WINDOW: usize = 20;

/// One price grouping of the `l2` feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Grouping {
    /// Significant figures the reference price is rounded to, 2 to 5.
    pub sig_figs: u32,
    /// Multiple of the rounded unit a bucket spans: 1, or 2 or 5 at five
    /// significant figures, as the source offers them.
    pub mantissa: u32,
}

impl Grouping {
    /// Whether the source offers this grouping: 2 to 4 significant figures
    /// with a mantissa of 1, or 5 with a mantissa of 1, 2 or 5.
    pub fn is_offered(self) -> bool {
        matches!((self.sig_figs, self.mantissa), (2..=4, 1) | (5, 1 | 2 | 5))
    }

    /// The width of this grouping's buckets, in ticks, for a book whose
    /// reference price is `reference`. With prices below 2^53 (16 digits) and
    /// at least 2 significant figures, it stays below 5 * 10^14.
    fn width(self, reference: Price) -> Price {
        let digits = reference
            .unsigned_abs()
            .checked_ilog10()
            .map_or(1, |log| log + 1);
        10_i64.pow(digits.saturating_sub(self.sig_figs)) * Price::from(self.mantissa)
    }

    /// Finer groupings sort first: more significant figures, then a smaller
    /// mantissa.
    fn fineness(self) -> (std::cmp::Reverse<u32>, u32) {
        (std::cmp::Reverse(self.sig_figs), self.mantissa)
    }
}

fn is_power_of_ten(price: Price) -> bool {
    price > 1
        && price
            .checked_ilog10()
            .is_some_and(|log| 10_i64.pow(log) == price)
}

/// The label of the bucket of `width` that holds `price`, in bid space: bids
/// as they are, asks negated, so that both round down and the best price is
/// the highest. The label is the bucket's lowest price there, the one an
/// outward walk reaches last.
fn label(price: Price, width: Price) -> Price {
    price.div_euclid(width) * width
}

/// Why groupings could not be merged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MergeError {
    /// No groupings were given.
    #[error("no groupings to merge")]
    Empty,
    /// The groupings are of different pushes.
    #[error("groupings to merge are from different times ({0} and {1})")]
    Times(Ts, Ts),
    /// A grouping is not one the source offers.
    #[error("unsupported grouping {0:?}")]
    Grouping(Grouping),
    /// The same grouping was given twice.
    #[error("grouping {0:?} given twice")]
    Duplicate(Grouping),
    /// A level's price is outside the protocol's range, 1 to 2^53 - 1.
    #[error("level price {0} is out of range")]
    Price(Price),
    /// A level has a size of zero, which no grouping lists.
    #[error("level at {0} has size zero")]
    ZeroSize(Price),
    /// A side lists more buckets than the source shows.
    #[error("a side of {grouping:?} lists {len} buckets, more than 20")]
    TooDeep {
        /// The grouping whose side is too deep.
        grouping: Grouping,
        /// Buckets listed on that side.
        len: usize,
    },
    /// A level's price is not a bucket of its grouping, or the levels of a
    /// side are not strictly best first.
    #[error("level {price} is not in best-first bucket order of {grouping:?}")]
    Bucket {
        /// The grouping whose levels are out of shape.
        grouping: Grouping,
        /// The level's price as given.
        price: Price,
    },
}

/// The merged book and how far the inputs disagreed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// Single-tick levels, best first. Order counts are 0, since they are not
    /// known for levels made by merging.
    pub book: BookSnapshot,
    /// How far the merged book misses the inputs, in lots: for every input
    /// bucket, what it ends up holding too much or too little, summed. Inputs
    /// that disagree, as views of slightly different moments do, give a
    /// number above 0; the finer grouping wins, so one lot a fine grouping
    /// disagrees on counts once for every coarser grouping it puts off. 0 for
    /// consistent inputs.
    pub conflicting_qty: u64,
}

/// Merges grouped snapshots of one push into one book of single-tick levels.
///
/// # Errors
///
/// Returns a [`MergeError`] when there are no inputs, their times differ, a
/// grouping is not one the source offers or is given twice, a price is out of
/// range, a size is zero, a side lists more than 20 buckets, or a level is not
/// a bucket of its grouping in best-first order.
pub fn merge(groupings: &[(Grouping, &BookSnapshot)]) -> Result<Merged, MergeError> {
    let (_, first) = groupings.first().ok_or(MergeError::Empty)?;
    for (index, &(grouping, book)) in groupings.iter().enumerate() {
        if book.ts != first.ts {
            return Err(MergeError::Times(first.ts, book.ts));
        }
        if !grouping.is_offered() {
            return Err(MergeError::Grouping(grouping));
        }
        if groupings[..index]
            .iter()
            .any(|&(other, _)| other == grouping)
        {
            return Err(MergeError::Duplicate(grouping));
        }
        let in_range = |level: &BookLevel| (1..=MAX_VALUE as Price).contains(&level.price);
        if let Some(level) = book.bids.iter().chain(&book.asks).find(|l| !in_range(l)) {
            return Err(MergeError::Price(level.price));
        }
        if let Some(level) = book.bids.iter().chain(&book.asks).find(|l| l.qty == 0) {
            return Err(MergeError::ZeroSize(level.price));
        }
        if let Some(len) = [book.bids.len(), book.asks.len()]
            .into_iter()
            .find(|&len| len > WINDOW)
        {
            return Err(MergeError::TooDeep { grouping, len });
        }
    }
    let mut by_fineness: Vec<&(Grouping, &BookSnapshot)> = groupings.iter().collect();
    by_fineness.sort_by_key(|(grouping, _)| grouping.fineness());
    let best_bid = by_fineness.iter().find_map(|(_, book)| book.bids.first());
    let best_ask = by_fineness
        .iter()
        .find_map(|(grouping, book)| book.asks.first().map(|ask| (*grouping, ask.price)));
    let exact = Grouping {
        sig_figs: 5,
        mantissa: 1,
    };
    let reference = match (best_bid, best_ask) {
        (Some(bid), _) => bid.price,
        (None, Some((grouping, ask))) if grouping != exact && is_power_of_ten(ask) => ask - 1,
        (None, Some((_, ask))) => ask,
        (None, None) => 1,
    };
    let mut conflicting_qty = 0;
    let side = |levels: fn(&BookSnapshot) -> &[BookLevel]| {
        groupings
            .iter()
            .map(|&(grouping, book)| (grouping, grouping.width(reference), levels(book)))
            .collect::<Vec<_>>()
    };
    let bids = merge_side(&side(|book| &book.bids), 1, &mut conflicting_qty)?;
    let asks = merge_side(&side(|book| &book.asks), -1, &mut conflicting_qty)?;
    Ok(Merged {
        book: BookSnapshot {
            ts: first.ts,
            bids,
            asks,
        },
        conflicting_qty,
    })
}

/// How far one grouping's buckets are filled during the walk.
struct Walk {
    /// The grouping's bucket width in ticks.
    width: Price,
    /// Its buckets in bid space, best first.
    buckets: Vec<(Price, u64)>,
    /// The next listed bucket not yet entered.
    next: usize,
    /// The bucket the walk is in and what it still has to receive.
    current: Option<(Price, u64)>,
    /// Lowest price, in bid space, that the grouping says anything about:
    /// the low end of its last listed bucket, or no end when it lists fewer
    /// than a full window and so shows the whole side.
    end: Price,
}

/// Merges one side. `sign` is 1 for bids and -1 for asks, which turns prices
/// into bid space and back.
fn merge_side(
    groupings: &[(Grouping, Price, &[BookLevel])],
    sign: Price,
    conflicting_qty: &mut u64,
) -> Result<Vec<BookLevel>, MergeError> {
    let mut walks = Vec::with_capacity(groupings.len());
    for &(grouping, width, levels) in groupings {
        let buckets: Vec<(Price, u64)> = levels
            .iter()
            .map(|level| (sign * level.price, level.qty))
            .collect();
        let bad = |price: Price| MergeError::Bucket {
            grouping,
            price: sign * price,
        };
        if let Some(&(price, _)) = buckets.iter().find(|&&(p, _)| label(p, width) != p) {
            return Err(bad(price));
        }
        if let Some(pair) = buckets.windows(2).find(|pair| pair[0].0 <= pair[1].0) {
            return Err(bad(pair[1].0));
        }
        let end = match buckets.last() {
            Some(&(last, _)) if buckets.len() >= WINDOW => last,
            _ => Price::MIN,
        };
        walks.push(Walk {
            width,
            buckets,
            next: 0,
            current: None,
            end,
        });
    }
    let mut prices: Vec<Price> = walks
        .iter()
        .flat_map(|walk| walk.buckets.iter().map(|&(price, _)| price))
        .collect();
    prices.sort_unstable_by(|a, b| b.cmp(a));
    prices.dedup();
    let mut merged = Vec::new();
    let mut here = Vec::with_capacity(walks.len());
    for price in prices {
        // Enter the bucket each grouping has at this price, and note how wide
        // it is, what it still has to receive and whether it ends here.
        here.clear();
        for walk in walks.iter_mut().filter(|walk| price >= walk.end) {
            let label = label(price, walk.width);
            if walk.current.map(|(current, _)| current) != Some(label) {
                let listed = walk.buckets.get(walk.next).filter(|&&(l, _)| l == label);
                walk.next += usize::from(listed.is_some());
                walk.current = Some((label, listed.map_or(0, |&(_, qty)| qty)));
            }
            let remaining = walk.current.map_or(0, |(_, qty)| qty);
            here.push((walk.width, remaining, label == price));
        }
        // The finest bucket that ends here takes what it still lacks, but no
        // more than any finer bucket around this price may hold; a coarser
        // bucket ending here could only take less.
        let qty = here
            .iter()
            .filter(|&&(_, _, ends)| ends)
            .map(|&(width, _, _)| width)
            .min()
            .map_or(0, |finest| {
                here.iter()
                    .filter(|&&(width, _, ends)| width < finest || (width == finest && ends))
                    .map(|&(_, remaining, _)| remaining)
                    .min()
                    .unwrap_or(0)
            });
        for walk in walks.iter_mut().filter(|walk| price >= walk.end) {
            if let Some((label, remaining)) = walk.current.as_mut() {
                // A bucket that holds less than this is overfilled, and one
                // that ends with something missing is underfilled; either is a
                // disagreement between the inputs.
                *conflicting_qty = conflicting_qty.saturating_add(qty.saturating_sub(*remaining));
                *remaining = remaining.saturating_sub(qty);
                if *label == price {
                    *conflicting_qty = conflicting_qty.saturating_add(*remaining);
                    *remaining = 0;
                }
            }
        }
        if qty > 0 {
            merged.push(BookLevel {
                price: sign * price,
                qty,
                orders: 0,
            });
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use protocol::MarketInfo;

    use super::*;
    use crate::{L2Decoder, Message};

    const fn grouping(sig_figs: u32, mantissa: u32) -> Grouping {
        Grouping { sig_figs, mantissa }
    }

    const ONE: Grouping = grouping(5, 1);
    const TWO: Grouping = grouping(5, 2);
    const FIVE: Grouping = grouping(5, 5);
    const TEN: Grouping = grouping(4, 1);
    const HUNDRED: Grouping = grouping(3, 1);
    const THOUSAND: Grouping = grouping(2, 1);
    const ALL: [Grouping; 6] = [ONE, TWO, FIVE, TEN, HUNDRED, THOUSAND];

    fn levels(pairs: &[(Price, u64)]) -> Vec<BookLevel> {
        pairs
            .iter()
            .map(|&(price, qty)| BookLevel {
                price,
                qty,
                orders: 0,
            })
            .collect()
    }

    fn book(bids: &[(Price, u64)], asks: &[(Price, u64)]) -> BookSnapshot {
        BookSnapshot {
            ts: 7,
            bids: levels(bids),
            asks: levels(asks),
        }
    }

    fn pairs(levels: &[BookLevel]) -> Vec<(Price, u64)> {
        levels.iter().map(|l| (l.price, l.qty)).collect()
    }

    /// The source's view of one side at a grouping, written independently of
    /// the module: the width from the reference price's decimal digits, bids
    /// rounded down and asks up, the 20 best non-empty buckets.
    fn view(
        side: &[BookLevel],
        grouping: Grouping,
        is_bid: bool,
        reference: Price,
    ) -> Vec<(Price, u64)> {
        let digits = reference.to_string().len() as u32;
        let unit = 10_i64.pow(digits.saturating_sub(grouping.sig_figs));
        let width = unit * Price::from(grouping.mantissa);
        let mut buckets: Vec<(Price, u64)> = Vec::new();
        for level in side {
            let label = if is_bid {
                level.price / width * width
            } else {
                (level.price + width - 1) / width * width
            };
            match buckets.iter_mut().find(|(l, _)| *l == label) {
                Some((_, qty)) => *qty += level.qty,
                None => buckets.push((label, level.qty)),
            }
        }
        if is_bid {
            buckets.sort_by_key(|&(label, _)| std::cmp::Reverse(label));
        } else {
            buckets.sort_by_key(|&(label, _)| label);
        }
        buckets.truncate(20);
        buckets
    }

    /// The source's best bid, or its best ask when it has no bids.
    fn reference(source: &BookSnapshot) -> Price {
        source
            .bids
            .first()
            .or(source.asks.first())
            .map_or(1, |l| l.price)
    }

    /// The source's views of a book at the given groupings.
    fn views(source: &BookSnapshot, groupings: &[Grouping]) -> Vec<(Grouping, BookSnapshot)> {
        let reference = reference(source);
        groupings
            .iter()
            .map(|&g| {
                let bids = view(&source.bids, g, true, reference);
                let asks = view(&source.asks, g, false, reference);
                (g, book(&bids, &asks))
            })
            .collect()
    }

    fn merged(inputs: &[(Grouping, BookSnapshot)]) -> Merged {
        let borrowed: Vec<(Grouping, &BookSnapshot)> =
            inputs.iter().map(|(g, b)| (*g, b)).collect();
        merge(&borrowed).expect("merges")
    }

    /// Asserts that grouping `merged` by each input's rule, with the width
    /// the views were made with, gives that input.
    fn assert_round_trip(inputs: &[(Grouping, BookSnapshot)], merged: &Merged, reference: Price) {
        for (grouping, input) in inputs {
            assert_eq!(
                view(&merged.book.bids, *grouping, true, reference),
                pairs(&input.bids),
                "bids at {grouping:?}"
            );
            assert_eq!(
                view(&merged.book.asks, *grouping, false, reference),
                pairs(&input.asks),
                "asks at {grouping:?}"
            );
        }
    }

    #[test]
    fn widths_follow_the_reference_price_and_the_grouping() {
        let widths: Vec<Price> = ALL.iter().map(|g| g.width(81_234)).collect();
        assert_eq!(widths, vec![1, 2, 5, 10, 100, 1_000]);
        assert_eq!(ONE.width(99_999), 1);
        assert_eq!(ONE.width(100_000), 10);
        assert_eq!(FIVE.width(100_000), 50);
        assert_eq!(THOUSAND.width(100_000), 10_000);
    }

    #[test]
    fn the_finest_grouping_alone_is_the_book() {
        let only = book(&[(81_000, 5), (80_998, 2)], &[(81_001, 3)]);
        let result = merged(&[(ONE, only.clone())]);
        assert_eq!(result.book, only);
        assert_eq!(result.conflicting_qty, 0);
    }

    #[test]
    fn a_coarse_bucket_beyond_the_fine_window_goes_to_its_far_end() {
        // The $1 view is a full window: bids 81,019 down to 81,000 and asks
        // 81,020 up to 81,039, one lot each. The $10 view also sees what lies
        // further out: the bid bucket 80,990 holds 6 beyond the $1 window,
        // placed at its lowest price; the ask bucket 81,040 holds 3 more than
        // the $1 levels inside it, placed at its highest price, and the ask
        // bucket 81,050 holds 7.
        let fine_bids: Vec<(Price, u64)> = (81_000..81_020).rev().map(|p| (p, 1)).collect();
        let fine_asks: Vec<(Price, u64)> = (81_020..81_040).map(|p| (p, 1)).collect();
        let fine = book(&fine_bids, &fine_asks);
        let coarse = book(
            &[(81_010, 10), (81_000, 10), (80_990, 6)],
            &[(81_020, 1), (81_030, 10), (81_040, 12), (81_050, 7)],
        );
        let result = merged(&[(ONE, fine), (TEN, coarse)]);
        let mut bids = fine_bids.clone();
        bids.push((80_990, 6));
        let mut asks = fine_asks.clone();
        asks.extend([(81_040, 3), (81_050, 7)]);
        assert_eq!(pairs(&result.book.bids), bids);
        assert_eq!(pairs(&result.book.asks), asks);
        assert_eq!(result.conflicting_qty, 0);
    }

    #[test]
    fn a_side_shorter_than_a_window_is_the_whole_side() {
        // The $1 view lists two bids, so the book has no bid below 80,999.
        // The $10 bucket 80,990 holds 6, of which only the 2 at 80,999 fit;
        // the other 4 contradict the $1 view and are not placed.
        let fine = book(&[(81_000, 5), (80_999, 2)], &[(81_001, 3)]);
        let coarse = book(&[(81_000, 5), (80_990, 6)], &[(81_010, 3)]);
        let result = merged(&[(ONE, fine), (TEN, coarse)]);
        assert_eq!(pairs(&result.book.bids), vec![(81_000, 5), (80_999, 2)]);
        assert_eq!(pairs(&result.book.asks), vec![(81_001, 3)]);
        assert_eq!(result.conflicting_qty, 4);
        // A side that a grouping lists as empty is empty.
        let no_bids = book(&[], &[(81_001, 3)]);
        let result = merged(&[(ONE, no_bids), (TEN, book(&[(81_000, 4)], &[(81_010, 3)]))]);
        assert_eq!(result.book.bids, vec![]);
        assert_eq!(result.conflicting_qty, 4);
    }

    #[test]
    fn two_and_five_dollar_buckets_are_kept_although_they_do_not_nest() {
        // Bids from 81,009 down: $2 buckets {81,008-81,009}, {81,006-81,007},
        // {81,004-81,005}; $5 buckets {81,005-81,009} and {81,000-81,004}.
        // The $2 bucket at 81,004 straddles the two $5 buckets.
        let source = book(
            &[
                (81_009, 1),
                (81_007, 2),
                (81_005, 4),
                (81_004, 8),
                (81_001, 16),
            ],
            &[],
        );
        let inputs: Vec<(Grouping, BookSnapshot)> = [TWO, FIVE, TEN]
            .iter()
            .map(|&g| (g, book(&view(&source.bids, g, true, 81_009), &[])))
            .collect();
        let result = merged(&inputs);
        assert_eq!(result.conflicting_qty, 0);
        assert_round_trip(&inputs, &result, 81_009);
    }

    #[test]
    fn inconsistent_inputs_keep_the_finer_view_and_are_counted() {
        // The $10 bucket claims 3 where the $1 levels inside it add up to 7.
        let fine = book(&[(81_005, 4), (81_001, 3)], &[]);
        let coarse = book(&[(81_000, 3)], &[]);
        let result = merged(&[(ONE, fine), (TEN, coarse)]);
        assert_eq!(pairs(&result.book.bids), vec![(81_005, 4), (81_001, 3)]);
        assert_eq!(result.conflicting_qty, 4);
        // A coarse bucket that claims more than fits beside a fine window
        // that covers all of it is underfilled, and counted too.
        let fine = book(&[(81_009, 1), (81_000, 1)], &[]);
        let coarse = book(&[(81_000, 5)], &[]);
        let result = merged(&[(ONE, fine), (TEN, coarse)]);
        assert_eq!(pairs(&result.book.bids), vec![(81_009, 1), (81_000, 1)]);
        assert_eq!(result.conflicting_qty, 3);
    }

    #[test]
    fn empty_sides_merge_to_empty_sides() {
        let empty = book(&[], &[]);
        let result = merged(&[(ONE, empty.clone()), (THOUSAND, empty.clone())]);
        assert_eq!(result.book, empty);
        assert_eq!(result.conflicting_qty, 0);
    }

    #[test]
    fn inputs_are_checked() {
        let a = book(&[(81_000, 1)], &[]);
        assert_eq!(merge(&[]), Err(MergeError::Empty));
        let mut later = a.clone();
        later.ts = 8;
        assert_eq!(
            merge(&[(ONE, &a), (TEN, &later)]),
            Err(MergeError::Times(7, 8))
        );
        for unsupported in [
            grouping(6, 1),
            grouping(1, 1),
            grouping(4, 2),
            grouping(5, 3),
        ] {
            assert_eq!(
                merge(&[(unsupported, &a)]),
                Err(MergeError::Grouping(unsupported))
            );
        }
        let bucket = |price| {
            Err(MergeError::Bucket {
                grouping: TEN,
                price,
            })
        };
        let not_a_bid_bucket = book(&[(81_000, 1), (80_995, 1)], &[]);
        assert_eq!(merge(&[(TEN, &not_a_bid_bucket)]), bucket(80_995));
        let not_an_ask_bucket = book(&[(81_000, 1)], &[(81_005, 1)]);
        assert_eq!(merge(&[(TEN, &not_an_ask_bucket)]), bucket(81_005));
        let unsorted = book(&[(80_990, 1), (81_000, 1)], &[]);
        assert_eq!(merge(&[(TEN, &unsorted)]), bucket(81_000));
        assert_eq!(
            merge(&[(TEN, &a), (TEN, &a)]),
            Err(MergeError::Duplicate(TEN))
        );
        for price in [0, -5, 1 << 53] {
            let out_of_range = book(&[(81_000, 1)], &[(price, 1)]);
            assert_eq!(
                merge(&[(ONE, &out_of_range)]),
                Err(MergeError::Price(price))
            );
        }
        let zero = book(&[(81_000, 0)], &[]);
        assert_eq!(merge(&[(ONE, &zero)]), Err(MergeError::ZeroSize(81_000)));
        let deep: Vec<(Price, u64)> = (0..21).map(|i| (81_000 - i, 1)).collect();
        assert_eq!(
            merge(&[(ONE, &book(&deep, &[]))]),
            Err(MergeError::TooDeep {
                grouping: ONE,
                len: 21
            })
        );
        let full: Vec<(Price, u64)> = (0..20).map(|i| (81_000 - i, 1)).collect();
        assert!(merge(&[(ONE, &book(&full, &[]))]).is_ok());
    }

    #[test]
    fn an_asks_only_book_at_a_power_of_ten_keeps_its_touchs_width() {
        // Asks only, from 99,995 upward: the $10 view rounds the touch up to
        // 100,000, a digit more than the touch has.
        let asks: Vec<(Price, u64)> = (0..30).map(|i| (99_995 + 7 * i, 1)).collect();
        let source = book(&[], &asks);
        let inputs = views(&source, &[TEN]);
        assert_eq!(inputs[0].1.asks[0].price, 100_000);
        let result = merged(&inputs);
        assert_eq!(result.conflicting_qty, 0);
        assert_round_trip(&inputs, &result, reference(&source));
        assert!(is_power_of_ten(100_000) && is_power_of_ten(10));
        assert!(!is_power_of_ten(1) && !is_power_of_ten(99_999) && !is_power_of_ten(100_001));
    }

    #[test]
    fn an_asks_only_touch_at_a_power_of_ten_is_exact_at_the_finest_grouping() {
        // Asks only, the touch at 100,000 itself: six digits, so the
        // 5-figure grouping is $10 wide and the 4-figure one $100.
        let asks: Vec<(Price, u64)> = (0..30).map(|i| (100_000 + 10 * i, 1)).collect();
        let source = book(&[], &asks);
        let inputs = views(&source, &[ONE, TEN]);
        let result = merged(&inputs);
        assert_eq!(result.conflicting_qty, 0);
        assert_round_trip(&inputs, &result, 100_000);
    }

    #[test]
    fn the_reference_is_the_finest_inputs_touch() {
        // Asks only, from a touch at 99,500: the $1,000 view rounds the touch
        // up to 100,000, which has a digit more, but the finest input still
        // gives the width of a five-digit price.
        let asks: Vec<(Price, u64)> = (0..30).map(|i| (99_500 + 100 * i, 1)).collect();
        let source = book(&[], &asks);
        let inputs = views(&source, &[TEN, THOUSAND]);
        assert_eq!(inputs[1].1.asks[0].price, 100_000);
        let result = merged(&inputs);
        assert_eq!(result.conflicting_qty, 0);
        assert_round_trip(&inputs, &result, reference(&source));
    }

    #[test]
    fn the_finer_bucket_wins_where_both_end_at_the_same_price() {
        let fine = book(&[(81_000, 4)], &[]);
        let coarse = book(&[(81_000, 3)], &[]);
        let result = merged(&[(ONE, fine), (TEN, coarse)]);
        assert_eq!(pairs(&result.book.bids), vec![(81_000, 4)]);
        assert_eq!(result.conflicting_qty, 1);
    }

    #[test]
    fn a_level_far_out_costs_no_more_than_a_near_one() {
        // A $1,000 bucket near the top of the protocol's price range: walking
        // every price up to it would never finish, so the merge runs on its
        // own thread and must answer within seconds.
        let far = 9_007_199_254_740_000;
        let fine = book(&[(81_497, 1)], &[(81_498, 1)]);
        let coarse = book(&[(81_000, 1)], &[(82_000, 1), (far, 1)]);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(merge(&[(ONE, &fine), (THOUSAND, &coarse)]));
        });
        let result = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the merge answers within seconds")
            .expect("merges");
        assert_eq!(pairs(&result.book.asks), vec![(81_498, 1)]);
        // The $1 view shows the whole ask side, so the far bucket conflicts.
        assert_eq!(result.conflicting_qty, 1);
    }

    #[test]
    fn a_finer_bucket_around_a_coarse_far_end_caps_what_goes_there() {
        // The $5 bucket 81,005 ends at 81,005, which lies inside the $2
        // bucket 81,004 that holds only 3; the $5 bucket's claim of 10
        // cannot all go there without overfilling the $2 bucket.
        let two = book(&[(81_004, 3)], &[]);
        let five = book(&[(81_005, 10)], &[]);
        let result = merged(&[(TWO, two), (FIVE, five)]);
        assert_eq!(pairs(&result.book.bids), vec![(81_005, 3)]);
        assert_eq!(result.conflicting_qty, 7);
    }

    #[test]
    fn an_empty_finest_input_leaves_the_reference_to_the_next_finest() {
        let empty = book(&[], &[]);
        let ten = book(&[(81_000, 4)], &[(81_010, 2)]);
        let result = merged(&[(ONE, empty), (TEN, ten)]);
        // The $1 view says both sides are empty, so nothing is placed; the
        // $10 view is still read with a $10 width.
        assert_eq!(result.book.bids, vec![]);
        assert_eq!(result.conflicting_qty, 6);
        // With the width of an empty reference, 1, a price that is no $10
        // bucket would pass unnoticed.
        let empty = book(&[], &[]);
        let not_a_bucket = book(&[(81_005, 4)], &[]);
        assert_eq!(
            merge(&[(ONE, &empty), (TEN, &not_a_bucket)]),
            Err(MergeError::Bucket {
                grouping: TEN,
                price: 81_005
            })
        );
    }

    #[test]
    fn each_coarser_grouping_counts_what_it_is_put_off_by() {
        // The $1 level holds 4 where three coarser views each claim 1.
        let fine = book(&[(81_005, 4)], &[]);
        let coarse = book(&[(81_000, 1)], &[]);
        let result = merged(&[
            (ONE, fine),
            (TEN, coarse.clone()),
            (HUNDRED, coarse.clone()),
            (THOUSAND, coarse),
        ]);
        assert_eq!(pairs(&result.book.bids), vec![(81_005, 4)]);
        assert_eq!(result.conflicting_qty, 9);
    }

    /// A small seeded generator, so that the random books are the same on
    /// every run.
    struct SplitMix(u64);

    impl SplitMix {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: u64) -> Price {
            (self.next() % bound) as Price
        }
    }

    /// A random book around `mid` with up to `depth` levels per side: gaps
    /// mostly small and sometimes wide, so that each grouping's window ends
    /// somewhere else.
    fn random_book(rng: &mut SplitMix, mid: Price, depth: Price) -> BookSnapshot {
        let mut side = |direction: Price| {
            let mut levels = Vec::new();
            let mut price = mid + direction * (1 + rng.below(3));
            for _ in 0..rng.below(depth as u64 + 1) {
                levels.push((price, 1 + rng.below(1_000_000) as u64));
                let gap = if rng.below(10) == 0 {
                    1 + rng.below(300)
                } else {
                    1 + rng.below(4)
                };
                price += direction * gap;
            }
            levels
        };
        let bids = side(-1);
        let asks = side(1);
        book(&bids, &asks)
    }

    #[test]
    fn random_books_survive_the_round_trip() {
        let mut rng = SplitMix(20_261_023);
        let mut past_100k = 0;
        for run in 0..600 {
            // Some books have asks running past $100,000, where a price gains
            // a digit but the width stays the touch's; some are thin; some
            // have one side empty; each uses a random subset of the groupings.
            let mid = if run % 3 == 0 {
                99_990
            } else {
                81_000 + rng.below(500)
            };
            let depth = if run % 4 == 0 { 30 } else { 450 };
            let mut source = random_book(&mut rng, mid, depth);
            match run % 10 {
                7 => source.asks.clear(),
                8 => source.bids.clear(),
                _ => {}
            }
            past_100k += usize::from(source.asks.iter().any(|l| l.price > 100_000));
            let subset: Vec<Grouping> = loop {
                let mask = rng.below(64);
                let chosen: Vec<Grouping> = ALL
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| mask & (1 << index) != 0)
                    .map(|(_, &g)| g)
                    .collect();
                if !chosen.is_empty() {
                    break chosen;
                }
            };
            let inputs = views(&source, &subset);
            let result = merged(&inputs);
            assert_eq!(result.conflicting_qty, 0, "run {run}");
            assert_round_trip(&inputs, &result, reference(&source));
        }
        assert!(
            past_100k > 100,
            "only {past_100k} books reach past $100,000"
        );
    }

    #[test]
    fn a_recorded_push_survives_the_round_trip() {
        let market = MarketInfo {
            symbol: "BTC-USD".to_string(),
            price_decimals: 0,
            qty_decimals: 5,
        };
        let mut inputs = Vec::new();
        for line in include_str!("../tests/fixtures/l2-six-groupings.jsonl").lines() {
            let value: serde_json::Value = serde_json::from_str(line).expect("fixture parses");
            let sig_figs = value["subscription"]["s"].as_u64().expect("has s");
            let mantissa = value["subscription"]["m"].as_u64().unwrap_or(1);
            let Message::L2(wire) = serde_json::from_str(line).expect("is a message") else {
                panic!("expected an l2 line");
            };
            let book = L2Decoder::new()
                .apply(&wire, &market)
                .expect("applies")
                .clone();
            let grouping = grouping(sig_figs as u32, mantissa as u32);
            inputs.push((grouping, book));
        }
        assert_eq!(inputs.len(), 6);
        let one_reference = inputs
            .iter()
            .find(|(g, _)| *g == ONE)
            .map(|(_, book)| book.bids[0].price)
            .expect("has the $1 grouping");
        let result = merged(&inputs);
        assert_eq!(result.conflicting_qty, 0);
        assert_round_trip(&inputs, &result, one_reference);
        // The $1,000 view reaches past $100,000 in steps of $1,000.
        assert!(
            inputs
                .iter()
                .any(|(_, book)| book.asks.iter().any(|l| l.price == 101_000))
        );
    }
}
