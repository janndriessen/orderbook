//! Matching of incoming limit orders against the book.

use protocol::{Event, OrderId, OrderType, Price, Qty, Side, TimeInForce};

use crate::{OrderBook, RestingOrder};

/// An order book plus the matching of incoming good-till-cancel limit orders.
#[derive(Debug, Default)]
pub struct Matcher {
    book: OrderBook,
}

impl Matcher {
    /// A matcher over an empty book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Matches a good-till-canceled limit order against the opposite side,
    /// then rests any remainder at its limit price.
    ///
    /// Trades execute at the maker's price, best price first and oldest order
    /// first within a price. The events are `Accepted`, one `Trade` per fill,
    /// then one `L2Update` per price level the order touched, carrying the
    /// level's final total: the consumed levels in consumption order, then the
    /// level it rests in.
    ///
    /// # Panics
    ///
    /// If `qty` is zero, or `order_id` is already resting; the gateway's
    /// stateless checks rule both out.
    pub fn place_limit(
        &mut self,
        order_id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
    ) -> Vec<Event> {
        assert!(qty > 0, "orders have a positive quantity");
        assert!(
            !self.book.contains(order_id),
            "the gateway rejects order ids that are already resting"
        );
        let mut events = vec![Event::Accepted {
            order_id,
            side,
            order_type: OrderType::Limit {
                price,
                time_in_force: TimeInForce::Gtc,
            },
            qty,
        }];
        let opposite = match side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        let mut touched = Vec::new();
        let mut remaining = qty;
        while remaining > 0 && self.crosses(side, price) {
            let fill = self
                .book
                .fill_best(opposite, remaining)
                .expect("crosses() saw a best price on the opposite side");
            remaining -= fill.qty;
            events.push(Event::Trade {
                maker_order_id: fill.maker_order_id,
                taker_order_id: order_id,
                aggressor_side: side,
                price: fill.price,
                qty: fill.qty,
            });
            // Consumption is monotone in price, so a repeat is always the last entry.
            if touched.last() != Some(&fill.price) {
                touched.push(fill.price);
            }
        }
        for level_price in touched {
            events.push(self.level_update(opposite, level_price));
        }
        if remaining > 0 {
            let resting = RestingOrder {
                order_id,
                side,
                price,
                qty: remaining,
            };
            self.book
                .add(resting)
                .expect("the id was checked absent and trades never rest it");
            events.push(self.level_update(side, price));
        }
        events
    }

    /// Whether the best opposite price trades with a limit order at `price`.
    fn crosses(&self, side: Side, price: Price) -> bool {
        match side {
            Side::Buy => self.book.best_ask().is_some_and(|ask| ask <= price),
            Side::Sell => self.book.best_bid().is_some_and(|bid| bid >= price),
        }
    }

    fn level_update(&self, side: Side, price: Price) -> Event {
        let qty = self
            .book
            .level(side, price)
            .map_or(0, |level| level.total());
        Event::L2Update { side, price, qty }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted(order_id: OrderId, side: Side, price: Price, qty: Qty) -> Event {
        Event::Accepted {
            order_id,
            side,
            order_type: OrderType::Limit {
                price,
                time_in_force: TimeInForce::Gtc,
            },
            qty,
        }
    }

    fn trade(maker: OrderId, taker: OrderId, side: Side, price: Price, qty: Qty) -> Event {
        Event::Trade {
            maker_order_id: maker,
            taker_order_id: taker,
            aggressor_side: side,
            price,
            qty,
        }
    }

    fn l2(side: Side, price: Price, qty: Qty) -> Event {
        Event::L2Update { side, price, qty }
    }

    #[test]
    fn non_crossing_order_rests() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 101, 5);
        assert_eq!(
            m.place_limit(2, Side::Buy, 100, 3),
            [accepted(2, Side::Buy, 100, 3), l2(Side::Buy, 100, 3)]
        );
    }

    #[test]
    fn full_fill_at_the_makers_better_price_empties_the_level() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, 5);
        assert_eq!(
            m.place_limit(2, Side::Buy, 105, 5),
            [
                accepted(2, Side::Buy, 105, 5),
                trade(1, 2, Side::Buy, 100, 5),
                l2(Side::Sell, 100, 0),
            ]
        );
    }

    #[test]
    fn partly_filled_maker_keeps_priority() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, 5);
        m.place_limit(2, Side::Sell, 100, 5);
        m.place_limit(3, Side::Buy, 100, 2);
        assert_eq!(
            m.place_limit(4, Side::Buy, 100, 4),
            [
                accepted(4, Side::Buy, 100, 4),
                trade(1, 4, Side::Buy, 100, 3),
                trade(2, 4, Side::Buy, 100, 1),
                l2(Side::Sell, 100, 4),
            ]
        );
    }

    #[test]
    fn sweep_takes_levels_and_makers_in_priority_order() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 101, 2);
        m.place_limit(2, Side::Sell, 100, 2);
        m.place_limit(3, Side::Sell, 100, 3);
        m.place_limit(4, Side::Sell, 103, 9);
        assert_eq!(
            m.place_limit(5, Side::Buy, 102, 10),
            [
                accepted(5, Side::Buy, 102, 10),
                trade(2, 5, Side::Buy, 100, 2),
                trade(3, 5, Side::Buy, 100, 3),
                trade(1, 5, Side::Buy, 101, 2),
                l2(Side::Sell, 100, 0),
                l2(Side::Sell, 101, 0),
                l2(Side::Buy, 102, 3),
            ]
        );
    }

    #[test]
    fn sell_taker_trades_against_bids() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Buy, 99, 4);
        m.place_limit(2, Side::Buy, 100, 4);
        assert_eq!(
            m.place_limit(3, Side::Sell, 99, 6),
            [
                accepted(3, Side::Sell, 99, 6),
                trade(2, 3, Side::Sell, 100, 4),
                trade(1, 3, Side::Sell, 99, 2),
                l2(Side::Buy, 100, 0),
                l2(Side::Buy, 99, 2),
            ]
        );
    }

    #[test]
    fn order_joining_an_existing_level_reports_the_level_total() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Buy, 102, 5);
        assert_eq!(
            m.place_limit(2, Side::Buy, 102, 3),
            [accepted(2, Side::Buy, 102, 3), l2(Side::Buy, 102, 8)]
        );
    }

    #[test]
    fn sell_remainder_rests_after_trading() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Buy, 100, 2);
        assert_eq!(
            m.place_limit(2, Side::Sell, 100, 5),
            [
                accepted(2, Side::Sell, 100, 5),
                trade(1, 2, Side::Sell, 100, 2),
                l2(Side::Buy, 100, 0),
                l2(Side::Sell, 100, 3),
            ]
        );
    }

    #[test]
    #[should_panic(expected = "already resting")]
    fn duplicate_id_on_the_opposite_side_panics_before_matching() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, 5);
        m.place_limit(1, Side::Buy, 100, 8);
    }

    #[test]
    #[should_panic(expected = "positive quantity")]
    fn zero_quantity_panics() {
        Matcher::new().place_limit(1, Side::Buy, 100, 0);
    }

    #[test]
    fn same_orders_give_identical_events() {
        let run = || {
            let mut m = Matcher::new();
            let orders = [
                (1, Side::Sell, 101, 3),
                (2, Side::Sell, 100, 2),
                (3, Side::Buy, 101, 4),
                (4, Side::Buy, 99, 5),
                (5, Side::Sell, 99, 7),
            ];
            orders.map(|(id, side, price, qty)| m.place_limit(id, side, price, qty))
        };
        assert_eq!(run(), run());
    }
}
