//! The publisher's aggregated view of the book: total quantity per price.

use std::collections::{BTreeMap, BTreeSet};

use protocol::{Level, Price, Qty, Side};

/// Total resting quantity per price on both sides, with change tracking.
#[derive(Debug, Default)]
pub struct AggregatedBook {
    bids: BTreeMap<Price, Qty>,
    asks: BTreeMap<Price, Qty>,
    dirty_bids: BTreeSet<Price>,
    dirty_asks: BTreeSet<Price>,
}

impl AggregatedBook {
    /// Sets the total at a level; zero removes it. Marks the level changed.
    pub fn set(&mut self, side: Side, price: Price, qty: Qty) {
        let (levels, dirty) = match side {
            Side::Buy => (&mut self.bids, &mut self.dirty_bids),
            Side::Sell => (&mut self.asks, &mut self.dirty_asks),
        };
        if qty == 0 {
            levels.remove(&price);
        } else {
            levels.insert(price, qty);
        }
        dirty.insert(price);
    }

    /// Returns the levels changed since the last call, bids then asks, and
    /// clears the change set. A level that is gone has quantity zero.
    pub fn take_changes(&mut self) -> (Vec<Level>, Vec<Level>) {
        let bids = collect_changed(&self.bids, std::mem::take(&mut self.dirty_bids));
        let asks = collect_changed(&self.asks, std::mem::take(&mut self.dirty_asks));
        (bids, asks)
    }

    /// Whether any level changed since the last call to `take_changes`.
    pub fn has_changes(&self) -> bool {
        !self.dirty_bids.is_empty() || !self.dirty_asks.is_empty()
    }

    /// All levels, bids and asks each best-first (bids descending, asks ascending).
    pub fn levels(&self) -> (Vec<Level>, Vec<Level>) {
        let bids = self
            .bids
            .iter()
            .rev()
            .map(|(&price, &qty)| Level { price, qty })
            .collect();
        let asks = self
            .asks
            .iter()
            .map(|(&price, &qty)| Level { price, qty })
            .collect();
        (bids, asks)
    }
}

fn collect_changed(levels: &BTreeMap<Price, Qty>, dirty: BTreeSet<Price>) -> Vec<Level> {
    dirty
        .into_iter()
        .map(|price| Level {
            price,
            qty: levels.get(&price).copied().unwrap_or(0),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(price: Price, qty: Qty) -> Level {
        Level { price, qty }
    }

    #[test]
    fn changes_cover_added_changed_and_removed_levels() {
        let mut book = AggregatedBook::default();
        book.set(Side::Buy, 100, 5);
        book.set(Side::Buy, 99, 2);
        book.set(Side::Sell, 101, 1);
        assert_eq!(
            book.take_changes(),
            (vec![level(99, 2), level(100, 5)], vec![level(101, 1)])
        );
        assert!(!book.has_changes());

        book.set(Side::Buy, 100, 8);
        book.set(Side::Buy, 99, 0);
        assert_eq!(
            book.take_changes(),
            (vec![level(99, 0), level(100, 8)], vec![])
        );
    }
}
