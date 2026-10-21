//! The order book: resting orders grouped into price levels per side.

use std::collections::{BTreeMap, HashMap, VecDeque};

use protocol::{OrderId, Price, Qty, Side, TimeInForce};

/// An order resting in the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestingOrder {
    /// Identifier of the order.
    pub order_id: OrderId,
    /// Side of the book it rests on.
    pub side: Side,
    /// Limit price in ticks.
    pub price: Price,
    /// Remaining quantity in lots.
    pub qty: Qty,
    /// How the order was placed. A resting order is good-till-cancel or
    /// post-only; a replace needs to know which.
    pub time_in_force: TimeInForce,
}

/// All resting orders at one price: a FIFO queue and its total quantity.
#[derive(Debug, Default)]
pub struct PriceLevel {
    queue: VecDeque<OrderId>,
    total: Qty,
}

impl PriceLevel {
    /// Order ids in arrival order, oldest first.
    pub fn orders(&self) -> impl Iterator<Item = OrderId> + '_ {
        self.queue.iter().copied()
    }

    /// Sum of the remaining quantity of every order in the level.
    pub fn total(&self) -> Qty {
        self.total
    }
}

/// A fill of the front order at the best price, as reported by
/// [`OrderBook::fill_best`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    /// The resting (maker) order that was filled.
    pub maker_order_id: OrderId,
    /// The maker's price, which is the trade price.
    pub price: Price,
    /// Quantity filled.
    pub qty: Qty,
}

/// Why the book refused an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BookError {
    /// An order with this id is already resting in the book.
    #[error("order {0} is already resting in the book")]
    DuplicateOrderId(OrderId),
}

/// Both sides of the book.
///
/// Levels are kept in sorted maps, so iteration order is defined by price.
/// The order index is a `HashMap` that is only ever looked up by id, never
/// iterated, so its unspecified order cannot affect determinism.
#[derive(Debug, Default)]
pub struct OrderBook {
    bids: BTreeMap<Price, PriceLevel>,
    asks: BTreeMap<Price, PriceLevel>,
    orders: HashMap<OrderId, RestingOrder>,
}

impl OrderBook {
    /// An empty book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rests `order` at the back of its price level.
    ///
    /// Fails, leaving the book unchanged, if the id is already resting.
    ///
    /// # Panics
    ///
    /// If `order.qty` is zero; the gateway's size check rules that out.
    pub fn add(&mut self, order: RestingOrder) -> Result<(), BookError> {
        assert!(order.qty > 0, "resting orders have a positive quantity");
        if self.orders.contains_key(&order.order_id) {
            return Err(BookError::DuplicateOrderId(order.order_id));
        }
        let level = self.side_mut(order.side).entry(order.price).or_default();
        level.queue.push_back(order.order_id);
        level.total = level
            .total
            .checked_add(order.qty)
            .expect("the gateway caps orders at 10^8 lots, so overflowing a u64 total takes about 1.8 × 10^11 resting orders, far more than fit in memory");
        self.orders.insert(order.order_id, order);
        Ok(())
    }

    /// Whether an order with this id is resting in the book.
    pub fn contains(&self, order_id: OrderId) -> bool {
        self.orders.contains_key(&order_id)
    }

    /// The resting order with this id, or `None` if it is not resting.
    pub fn order(&self, order_id: OrderId) -> Option<RestingOrder> {
        self.orders.get(&order_id).copied()
    }

    /// Lowers a resting order's quantity to `qty`, keeping its place in the
    /// queue.
    ///
    /// # Panics
    ///
    /// If `qty` is zero or above the order's open quantity, or the id is not
    /// resting. Callers check the order first and cancel to remove it.
    pub fn reduce(&mut self, order_id: OrderId, qty: Qty) {
        assert!(qty > 0, "resting orders have a positive quantity");
        let order = self
            .orders
            .get_mut(&order_id)
            .expect("only resting orders can be reduced");
        let cut = order
            .qty
            .checked_sub(qty)
            .expect("a reduction does not raise the quantity");
        order.qty = qty;
        let level = match order.side {
            Side::Buy => self.bids.get_mut(&order.price),
            Side::Sell => self.asks.get_mut(&order.price),
        }
        .expect("every indexed order rests in its side's level at its price");
        level.total = level
            .total
            .checked_sub(cut)
            .expect("a level's total includes every order queued in it");
    }

    /// Removes a resting order, or returns `None` if the id is unknown.
    ///
    /// Scans the order's level for its place in the queue, so the cost is
    /// linear in that level's length. A level left empty is removed.
    pub fn cancel(&mut self, order_id: OrderId) -> Option<RestingOrder> {
        let order = self.orders.remove(&order_id)?;
        let levels = self.side_mut(order.side);
        let level = levels
            .get_mut(&order.price)
            .expect("every indexed order rests in its side's level at its price");
        let pos = level
            .queue
            .iter()
            .position(|&id| id == order_id)
            .expect("every indexed order is queued in its level");
        level.queue.remove(pos);
        level.total = level
            .total
            .checked_sub(order.qty)
            .expect("a level's total includes every order queued in it");
        if level.queue.is_empty() {
            levels.remove(&order.price);
        }
        Some(order)
    }

    /// Highest price with a resting bid.
    pub fn best_bid(&self) -> Option<Price> {
        self.bids.keys().next_back().copied()
    }

    /// Lowest price with a resting ask.
    pub fn best_ask(&self) -> Option<Price> {
        self.asks.keys().next().copied()
    }

    /// Number of orders resting in the book, both sides combined.
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    /// Whether no order rests on either side.
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    /// The levels on `side` with their prices, in ascending price order.
    pub fn levels(&self, side: Side) -> impl Iterator<Item = (Price, &PriceLevel)> {
        let levels = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        levels.iter().map(|(&price, level)| (price, level))
    }

    /// Fills up to `max_qty` of the front order at the best price on `side`,
    /// or returns `None` if the side is empty.
    ///
    /// The fill is `min(max_qty, front order's remaining quantity)`. A partly
    /// filled order keeps its place at the front. A fully filled one leaves
    /// the queue and the index, and an emptied level is removed. Trades only
    /// ever take the front of the best level, so they follow price-time
    /// priority by construction; [`OrderBook::reduce`] lowers a resting order
    /// without trading.
    ///
    /// # Panics
    ///
    /// If `max_qty` is zero.
    pub fn fill_best(&mut self, side: Side, max_qty: Qty) -> Option<Fill> {
        assert!(max_qty > 0, "a fill has a positive quantity");
        let price = self.best_price(side)?;
        let id = *self.level(side, price)?.queue.front()?;
        let maker = *self.orders.get(&id).expect("every queued order is indexed");
        let qty = max_qty.min(maker.qty);
        let remaining = maker.qty - qty;
        let levels = self.side_mut(side);
        let level = levels
            .get_mut(&maker.price)
            .expect("the best order rests in its side's level at its price");
        level.total = level
            .total
            .checked_sub(qty)
            .expect("a level's total includes its front order");
        if remaining == 0 {
            level.queue.pop_front();
            if level.queue.is_empty() {
                levels.remove(&maker.price);
            }
            self.orders.remove(&maker.order_id);
        } else {
            self.orders
                .get_mut(&maker.order_id)
                .expect("the best order is indexed")
                .qty = remaining;
        }
        Some(Fill {
            maker_order_id: maker.order_id,
            price: maker.price,
            qty,
        })
    }

    /// The level at `price` on `side`, if any order rests there.
    pub fn level(&self, side: Side, price: Price) -> Option<&PriceLevel> {
        match side {
            Side::Buy => self.bids.get(&price),
            Side::Sell => self.asks.get(&price),
        }
    }

    fn best_price(&self, side: Side) -> Option<Price> {
        match side {
            Side::Buy => self.best_bid(),
            Side::Sell => self.best_ask(),
        }
    }

    fn side_mut(&mut self, side: Side) -> &mut BTreeMap<Price, PriceLevel> {
        match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(order_id: OrderId, side: Side, price: Price, qty: Qty) -> RestingOrder {
        RestingOrder {
            order_id,
            side,
            price,
            qty,
            time_in_force: TimeInForce::Gtc,
        }
    }

    fn queue_of(book: &OrderBook, side: Side, price: Price) -> Vec<OrderId> {
        book.level(side, price)
            .map_or(vec![], |l| l.orders().collect())
    }

    /// Three bids at 100 (ids 1, 2, 3; qty 10, 20, 30).
    fn book_with_queue() -> OrderBook {
        let mut book = OrderBook::new();
        for (id, qty) in [(1, 10), (2, 20), (3, 30)] {
            book.add(order(id, Side::Buy, 100, qty)).unwrap();
        }
        book
    }

    #[test]
    fn levels_list_each_side_by_ascending_price() {
        let mut book = OrderBook::new();
        for (id, side, price) in [
            (1, Side::Buy, 98),
            (2, Side::Buy, 99),
            (3, Side::Sell, 102),
            (4, Side::Sell, 101),
            (5, Side::Buy, 99),
        ] {
            book.add(order(id, side, price, 5)).unwrap();
        }
        let levels = |side| {
            book.levels(side)
                .map(|(price, level)| (price, level.total()))
                .collect::<Vec<_>>()
        };
        assert_eq!(levels(Side::Buy), [(98, 5), (99, 10)]);
        assert_eq!(levels(Side::Sell), [(101, 5), (102, 5)]);
    }

    #[test]
    fn len_counts_the_resting_orders() {
        let mut book = book_with_queue();
        assert_eq!(book.len(), 3);
        assert!(!book.is_empty());
        book.fill_best(Side::Buy, 10);
        assert_eq!(book.len(), 2, "a filled order leaves");
        book.fill_best(Side::Buy, 5);
        assert_eq!(book.len(), 2, "a partly filled order stays");
        book.cancel(3);
        book.cancel(2);
        assert!(book.is_empty());
    }

    #[test]
    fn level_is_fifo_in_arrival_order() {
        let book = book_with_queue();
        assert_eq!(queue_of(&book, Side::Buy, 100), [1, 2, 3]);
    }

    #[test]
    fn best_prices_on_both_sides() {
        let mut book = OrderBook::new();
        assert_eq!((book.best_bid(), book.best_ask()), (None, None));
        for (id, side, price) in [
            (1, Side::Buy, 98),
            (2, Side::Buy, 99),
            (3, Side::Sell, 102),
            (4, Side::Sell, 101),
        ] {
            book.add(order(id, side, price, 1)).unwrap();
        }
        assert_eq!((book.best_bid(), book.best_ask()), (Some(99), Some(101)));
    }

    #[test]
    fn cancel_front_middle_and_back() {
        for (id, rest) in [(1, [2, 3]), (2, [1, 3]), (3, [1, 2])] {
            let mut book = book_with_queue();
            assert_eq!(book.cancel(id).map(|o| o.order_id), Some(id));
            assert_eq!(queue_of(&book, Side::Buy, 100), rest);
            assert_eq!(book.cancel(id), None);
        }
    }

    #[test]
    fn emptied_level_disappears() {
        let mut book = OrderBook::new();
        book.add(order(1, Side::Sell, 101, 5)).unwrap();
        book.add(order(2, Side::Sell, 102, 5)).unwrap();
        book.cancel(1);
        assert!(book.level(Side::Sell, 101).is_none());
        assert_eq!(book.best_ask(), Some(102));
    }

    #[test]
    fn level_total_tracks_adds_and_cancels() {
        let mut book = book_with_queue();
        let total = |b: &OrderBook| b.level(Side::Buy, 100).map(PriceLevel::total);
        assert_eq!(total(&book), Some(60));
        book.cancel(2);
        assert_eq!(total(&book), Some(40));
        book.add(order(4, Side::Buy, 100, 5)).unwrap();
        assert_eq!(total(&book), Some(45));
    }

    #[test]
    fn reduce_keeps_the_queue_position_and_updates_the_total() {
        let mut book = book_with_queue();
        book.reduce(2, 5);
        assert_eq!(book.order(2), Some(order(2, Side::Buy, 100, 5)));
        assert_eq!(queue_of(&book, Side::Buy, 100), [1, 2, 3]);
        assert_eq!(book.level(Side::Buy, 100).map(PriceLevel::total), Some(45));
        book.reduce(2, 5);
        assert_eq!(book.level(Side::Buy, 100).map(PriceLevel::total), Some(45));
    }

    #[test]
    fn order_looks_up_resting_orders_only() {
        let mut book = book_with_queue();
        assert_eq!(book.order(3), Some(order(3, Side::Buy, 100, 30)));
        book.cancel(3);
        assert_eq!(book.order(3), None);
        assert_eq!(book.order(99), None);
    }

    #[test]
    #[should_panic(expected = "does not raise the quantity")]
    fn reduce_cannot_raise_the_quantity() {
        book_with_queue().reduce(1, 11);
    }

    #[test]
    #[should_panic(expected = "resting orders have a positive quantity")]
    fn reduce_to_zero_panics() {
        book_with_queue().reduce(1, 0);
    }

    #[test]
    #[should_panic(expected = "only resting orders can be reduced")]
    fn reduce_of_an_unknown_id_panics() {
        book_with_queue().reduce(99, 1);
    }

    #[test]
    fn cancel_unknown_id_returns_none() {
        let mut book = book_with_queue();
        assert_eq!(book.cancel(99), None);
        assert_eq!(queue_of(&book, Side::Buy, 100), [1, 2, 3]);
    }

    #[test]
    fn duplicate_id_is_rejected_and_book_unchanged() {
        let mut book = book_with_queue();
        let result = book.add(order(2, Side::Sell, 105, 7));
        assert_eq!(result, Err(BookError::DuplicateOrderId(2)));
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.level(Side::Buy, 100).map(PriceLevel::total), Some(60));
        assert_eq!(queue_of(&book, Side::Buy, 100), [1, 2, 3]);
        assert_eq!(book.cancel(2), Some(order(2, Side::Buy, 100, 20)));
    }

    fn fill(maker_order_id: OrderId, price: Price, qty: Qty) -> Option<Fill> {
        Some(Fill {
            maker_order_id,
            price,
            qty,
        })
    }

    #[test]
    fn partial_fill_keeps_the_front_position() {
        let mut book = book_with_queue();
        assert_eq!(book.fill_best(Side::Buy, 4), fill(1, 100, 4));
        assert_eq!(book.level(Side::Buy, 100).map(PriceLevel::total), Some(56));
        assert_eq!(queue_of(&book, Side::Buy, 100), [1, 2, 3]);
        assert_eq!(book.fill_best(Side::Buy, 100), fill(1, 100, 6));
    }

    #[test]
    fn sell_side_partial_fill_reduces_the_best_ask() {
        let mut book = OrderBook::new();
        book.add(order(1, Side::Sell, 102, 5)).unwrap();
        book.add(order(2, Side::Sell, 101, 8)).unwrap();
        assert_eq!(book.fill_best(Side::Sell, 3), fill(2, 101, 3));
        assert_eq!(book.level(Side::Sell, 101).map(PriceLevel::total), Some(5));
        assert_eq!(book.level(Side::Sell, 102).map(PriceLevel::total), Some(5));
    }

    #[test]
    fn fill_is_capped_at_the_front_orders_remaining_quantity() {
        let mut book = book_with_queue();
        assert_eq!(book.fill_best(Side::Buy, 25), fill(1, 100, 10));
        assert_eq!(queue_of(&book, Side::Buy, 100), [2, 3]);
        assert_eq!(book.level(Side::Buy, 100).map(PriceLevel::total), Some(50));
        assert_eq!(book.cancel(1), None);
    }

    #[test]
    fn filling_the_last_order_removes_the_level() {
        let mut book = OrderBook::new();
        book.add(order(1, Side::Buy, 100, 5)).unwrap();
        book.add(order(2, Side::Buy, 99, 5)).unwrap();
        book.fill_best(Side::Buy, 5);
        assert!(book.level(Side::Buy, 100).is_none());
        assert_eq!(book.best_bid(), Some(99));
        assert_eq!(book.cancel(1), None);
    }

    #[test]
    fn fills_use_the_best_price_of_each_side() {
        let mut book = OrderBook::new();
        for (id, side, price) in [
            (1, Side::Buy, 98),
            (2, Side::Buy, 99),
            (3, Side::Sell, 102),
            (4, Side::Sell, 101),
        ] {
            book.add(order(id, side, price, 1)).unwrap();
        }
        assert_eq!(book.fill_best(Side::Buy, 1), fill(2, 99, 1));
        assert_eq!(book.fill_best(Side::Sell, 1), fill(4, 101, 1));
    }

    #[test]
    fn filling_an_empty_side_returns_none() {
        let mut book = book_with_queue();
        assert_eq!(book.fill_best(Side::Sell, 1), None);
    }

    #[test]
    #[should_panic(expected = "a fill has a positive quantity")]
    fn zero_max_qty_fill_panics() {
        book_with_queue().fill_best(Side::Buy, 0);
    }

    #[test]
    #[should_panic(expected = "resting orders have a positive quantity")]
    fn adding_a_zero_qty_order_panics() {
        let _ = OrderBook::new().add(order(1, Side::Buy, 100, 0));
    }
}
