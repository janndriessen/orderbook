//! The source's own resting-order book.
//!
//! Orders queue first-in-first-out within a price level, so a taker fills the
//! oldest maker first, as price-time priority requires.

use std::collections::{BTreeMap, VecDeque};

use protocol::{OrderId, Price, Qty, Side};

/// A resting order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Order {
    /// Buy or sell.
    pub side: Side,
    /// Limit price.
    pub price: Price,
    /// Open quantity.
    pub qty: Qty,
}

/// Orders at one price in arrival order, plus their total.
#[derive(Debug, Default)]
struct Level {
    total: Qty,
    queue: VecDeque<OrderId>,
}

/// Resting orders indexed by id and by price level.
#[derive(Debug, Default)]
pub struct RestingBook {
    orders: BTreeMap<OrderId, Order>,
    bids: BTreeMap<Price, Level>,
    asks: BTreeMap<Price, Level>,
}

impl RestingBook {
    fn levels(&self, side: Side) -> &BTreeMap<Price, Level> {
        match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        }
    }

    fn levels_mut(&mut self, side: Side) -> &mut BTreeMap<Price, Level> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }

    /// Best price of `side`: the highest bid or the lowest ask.
    pub fn best(&self, side: Side) -> Option<Price> {
        match side {
            Side::Buy => self.bids.keys().next_back().copied(),
            Side::Sell => self.asks.keys().next().copied(),
        }
    }

    /// Number of price levels on `side`.
    pub fn level_count(&self, side: Side) -> usize {
        self.levels(side).len()
    }

    /// Number of resting orders on `side`.
    pub fn order_count(&self, side: Side) -> usize {
        self.levels(side).values().map(|l| l.queue.len()).sum()
    }

    /// Total quantity resting at a price; zero when the level is empty.
    pub fn total(&self, side: Side, price: Price) -> Qty {
        self.levels(side).get(&price).map_or(0, |l| l.total)
    }

    /// Looks up a resting order.
    pub fn get(&self, id: OrderId) -> Option<Order> {
        self.orders.get(&id).copied()
    }

    /// The `n`-th resting order by id, for uniform random selection.
    pub fn nth_id(&self, n: usize) -> Option<OrderId> {
        self.orders.keys().nth(n).copied()
    }

    /// Number of resting orders.
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    /// Resting order ids at a price, oldest first.
    pub fn queue(&self, side: Side, price: Price) -> Vec<OrderId> {
        self.levels(side)
            .get(&price)
            .map_or_else(Vec::new, |l| l.queue.iter().copied().collect())
    }

    /// Adds an order at the back of its level's queue.
    pub fn insert(&mut self, id: OrderId, order: Order) {
        let level = self.levels_mut(order.side).entry(order.price).or_default();
        level.total += order.qty;
        level.queue.push_back(id);
        self.orders.insert(id, order);
    }

    /// Removes an order, dropping its level when it was the last one.
    pub fn remove(&mut self, id: OrderId) -> Option<Order> {
        let order = self.orders.remove(&id)?;
        let levels = self.levels_mut(order.side);
        if let Some(level) = levels.get_mut(&order.price) {
            level.total -= order.qty;
            level.queue.retain(|&queued| queued != id);
            if level.queue.is_empty() {
                levels.remove(&order.price);
            }
        }
        Some(order)
    }

    /// Changes an order's quantity in place, keeping its queue position.
    pub fn resize(&mut self, id: OrderId, new_qty: Qty) {
        let Some(order) = self.orders.get_mut(&id) else {
            return;
        };
        let old_qty = std::mem::replace(&mut order.qty, new_qty);
        let (side, price) = (order.side, order.price);
        if let Some(level) = self.levels_mut(side).get_mut(&price) {
            level.total = level.total - old_qty + new_qty;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(side: Side, price: Price, qty: Qty) -> Order {
        Order { side, price, qty }
    }

    #[test]
    fn totals_follow_inserts_and_removals() {
        let mut book = RestingBook::default();
        book.insert(1, order(Side::Buy, 100, 5));
        book.insert(2, order(Side::Buy, 100, 7));
        book.insert(3, order(Side::Sell, 101, 2));
        assert_eq!(book.total(Side::Buy, 100), 12);
        assert_eq!(
            (book.best(Side::Buy), book.best(Side::Sell)),
            (Some(100), Some(101))
        );

        book.remove(1);
        assert_eq!(book.total(Side::Buy, 100), 7);
        book.remove(2);
        assert_eq!(book.total(Side::Buy, 100), 0);
        assert_eq!(book.best(Side::Buy), None);
        assert_eq!(book.remove(2), None);
    }

    #[test]
    fn queue_keeps_arrival_order() {
        let mut book = RestingBook::default();
        for id in [4, 2, 9] {
            book.insert(id, order(Side::Sell, 50, 1));
        }
        book.remove(2);
        book.insert(7, order(Side::Sell, 50, 1));
        assert_eq!(book.queue(Side::Sell, 50), vec![4, 9, 7]);
    }

    #[test]
    fn resize_keeps_position_and_total() {
        let mut book = RestingBook::default();
        book.insert(1, order(Side::Buy, 10, 10));
        book.insert(2, order(Side::Buy, 10, 10));
        book.resize(1, 4);
        book.resize(2, 30);
        assert_eq!(book.total(Side::Buy, 10), 34);
        assert_eq!(book.get(1).map(|o| o.qty), Some(4));
        assert_eq!(book.queue(Side::Buy, 10), vec![1, 2]);
    }
}
