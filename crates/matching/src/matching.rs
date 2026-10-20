//! Matching of incoming orders against the book, and cancels and replaces of
//! resting ones.

use protocol::{
    CancelRejectReason, CancelRequest, Command, Event, OrderId, OrderType, Price, Qty,
    RejectReason, Side, TimeInForce,
};

use crate::{OrderBook, RestingOrder};

/// An order book plus the matching of incoming orders and the cancel and
/// replace of resting ones.
#[derive(Debug, Default)]
pub struct Matcher {
    book: OrderBook,
}

impl Matcher {
    /// A matcher over an empty book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs one command and returns the events it produced.
    ///
    /// The engine's input: `Cancel` and `Replace` go to the method of the same
    /// name, and `Place` goes to `place_limit` or `place_market` by order
    /// type. The events and the panics on values the gateway rules out are
    /// those of that method.
    pub fn apply(&mut self, command: Command) -> Vec<Event> {
        match command {
            Command::Place {
                order_id,
                side,
                order_type:
                    OrderType::Limit {
                        price,
                        time_in_force,
                    },
                qty,
            } => self.place_limit(order_id, side, price, time_in_force, qty),
            Command::Place {
                order_id,
                side,
                order_type: OrderType::Market {},
                qty,
            } => self.place_market(order_id, side, qty),
            Command::Cancel { order_id } => self.cancel(order_id),
            Command::Replace {
                order_id,
                price,
                qty,
            } => self.replace(order_id, price, qty),
        }
    }

    /// Matches a limit order against the opposite side.
    ///
    /// Trades execute at the maker's price, best price first and oldest order
    /// first within a price. What happens to the remainder depends on
    /// `time_in_force`: good-till-cancel and post-only orders rest it at the
    /// limit price, an immediate-or-cancel order cancels it. A post-only order
    /// that would trade on arrival is rejected instead, and nothing else
    /// happens.
    ///
    /// The events are `Accepted`, one `Trade` per fill, `Canceled` for an
    /// unrested remainder, then one `L2Update` per price level the order
    /// touched, carrying the level's final total: the consumed levels in
    /// consumption order, then the level it rests in.
    ///
    /// # Panics
    ///
    /// If `qty` is zero, or `order_id` is already resting; the gateway's checks
    /// rule both out.
    pub fn place_limit(
        &mut self,
        order_id: OrderId,
        side: Side,
        price: Price,
        time_in_force: TimeInForce,
        qty: Qty,
    ) -> Vec<Event> {
        assert!(qty > 0, "orders have a positive quantity");
        assert!(
            !self.book.contains(order_id),
            "the gateway rejects order ids that are already resting"
        );
        let rests = match time_in_force {
            TimeInForce::Gtc => true,
            TimeInForce::Ioc => false,
            TimeInForce::PostOnly => {
                if self.crosses(side, Some(price)) {
                    return vec![Event::Rejected {
                        order_id,
                        reason: RejectReason::PostOnlyWouldCross,
                    }];
                }
                true
            }
        };
        let mut events = vec![Event::Accepted {
            order_id,
            side,
            order_type: OrderType::Limit {
                price,
                time_in_force,
            },
            qty,
        }];
        let (remaining, touched) = self.take(order_id, side, Some(price), qty, &mut events);
        if remaining > 0 && !rests {
            events.push(Event::Canceled {
                order_id,
                leaves_qty: remaining,
            });
        }
        for level_price in touched {
            events.push(self.level_update(side.opposite(), level_price));
        }
        if remaining > 0 && rests {
            let resting = RestingOrder {
                order_id,
                side,
                price,
                qty: remaining,
                time_in_force,
            };
            self.book
                .add(resting)
                .expect("the id was checked absent and trades never rest it");
            events.push(self.level_update(side, price));
        }
        events
    }

    /// Matches a market order against the opposite side at any price.
    ///
    /// If the opposite side is empty the order is rejected with `NoLiquidity`
    /// and nothing else happens. Otherwise the events are `Accepted`, one
    /// `Trade` per fill, `Canceled` for whatever the book could not fill, then
    /// one `L2Update` per consumed level. A market order never rests, and it
    /// has no price protection: it takes every level it needs.
    ///
    /// # Panics
    ///
    /// If `qty` is zero, or `order_id` is already resting; the gateway's checks
    /// rule both out.
    pub fn place_market(&mut self, order_id: OrderId, side: Side, qty: Qty) -> Vec<Event> {
        assert!(qty > 0, "orders have a positive quantity");
        assert!(
            !self.book.contains(order_id),
            "the gateway rejects order ids that are already resting"
        );
        if !self.crosses(side, None) {
            return vec![Event::Rejected {
                order_id,
                reason: RejectReason::NoLiquidity,
            }];
        }
        let mut events = vec![Event::Accepted {
            order_id,
            side,
            order_type: OrderType::Market {},
            qty,
        }];
        let (remaining, touched) = self.take(order_id, side, None, qty, &mut events);
        if remaining > 0 {
            events.push(Event::Canceled {
                order_id,
                leaves_qty: remaining,
            });
        }
        for level_price in touched {
            events.push(self.level_update(side.opposite(), level_price));
        }
        events
    }

    /// Removes a resting order.
    ///
    /// The events are `Canceled`, carrying the order's open quantity, then an
    /// `L2Update` with the new total of its level (zero if the level is gone).
    /// An id that is not resting, whether never placed, filled or already
    /// canceled, yields a lone `CancelRejected` and leaves the book unchanged.
    pub fn cancel(&mut self, order_id: OrderId) -> Vec<Event> {
        match self.book.cancel(order_id) {
            Some(order) => vec![
                Event::Canceled {
                    order_id,
                    leaves_qty: order.qty,
                },
                self.level_update(order.side, order.price),
            ],
            None => vec![Event::CancelRejected {
                order_id,
                response_to: CancelRequest::Cancel,
                reason: CancelRejectReason::UnknownOrder,
            }],
        }
    }

    /// Changes a resting order to price `price` and open quantity `qty`.
    ///
    /// An order whose price is unchanged and whose quantity does not grow
    /// keeps its place in the queue. Any other change behaves like a cancel
    /// followed by a new arrival with the same side and time in force: the
    /// order first trades against the opposite side if the new price crosses,
    /// and rests any remainder at the back of the queue at the new price. A
    /// replace that changes nothing still counts as the first kind: it is
    /// reported and the order keeps its priority.
    ///
    /// The events are `Replaced`, one `Trade` per fill (this order is the
    /// taker), then one `L2Update` per touched level carrying its final
    /// total: the order's old level, the consumed opposite levels in
    /// consumption order, and the new level if a remainder rests there.
    ///
    /// Two requests are refused with a lone `CancelRejected` that leaves the
    /// book unchanged: an id that is not resting (`UnknownOrder`), and a
    /// post-only order whose new price would trade (`PostOnlyWouldCross`).
    ///
    /// # Panics
    ///
    /// If `qty` is zero; the gateway's checks rule that out.
    pub fn replace(&mut self, order_id: OrderId, price: Price, qty: Qty) -> Vec<Event> {
        assert!(qty > 0, "orders have a positive quantity");
        let refuse = |reason| {
            vec![Event::CancelRejected {
                order_id,
                response_to: CancelRequest::Replace,
                reason,
            }]
        };
        let Some(old) = self.book.order(order_id) else {
            return refuse(CancelRejectReason::UnknownOrder);
        };
        if old.time_in_force == TimeInForce::PostOnly && self.crosses(old.side, Some(price)) {
            return refuse(CancelRejectReason::PostOnlyWouldCross);
        }
        let mut events = vec![Event::Replaced {
            order_id,
            price,
            qty,
        }];
        if price == old.price && qty <= old.qty {
            self.book.reduce(order_id, qty);
            events.push(self.level_update(old.side, old.price));
            return events;
        }
        self.book
            .cancel(order_id)
            .expect("the order was just looked up in the book");
        let (remaining, touched) = self.take(order_id, old.side, Some(price), qty, &mut events);
        if remaining > 0 {
            self.book
                .add(RestingOrder {
                    price,
                    qty: remaining,
                    ..old
                })
                .expect("the id was just removed from the book");
        }
        events.push(self.level_update(old.side, old.price));
        for level_price in touched {
            events.push(self.level_update(old.side.opposite(), level_price));
        }
        if remaining > 0 && price != old.price {
            events.push(self.level_update(old.side, price));
        }
        events
    }

    /// Fills up to `qty` against the opposite side while its best price
    /// trades with `limit` (any price if `None`), pushing one `Trade` per
    /// fill. Returns the unfilled quantity and the consumed prices in order.
    fn take(
        &mut self,
        order_id: OrderId,
        side: Side,
        limit: Option<Price>,
        qty: Qty,
        events: &mut Vec<Event>,
    ) -> (Qty, Vec<Price>) {
        let mut touched = Vec::new();
        let mut remaining = qty;
        while remaining > 0 && self.crosses(side, limit) {
            let fill = self
                .book
                .fill_best(side.opposite(), remaining)
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
        (remaining, touched)
    }

    /// Whether the best opposite price trades with an order on `side` limited
    /// to `limit`; with no limit, whether the opposite side has any order.
    fn crosses(&self, side: Side, limit: Option<Price>) -> bool {
        match (side, limit) {
            (Side::Buy, None) => self.book.best_ask().is_some(),
            (Side::Sell, None) => self.book.best_bid().is_some(),
            (Side::Buy, Some(p)) => self.book.best_ask().is_some_and(|ask| ask <= p),
            (Side::Sell, Some(p)) => self.book.best_bid().is_some_and(|bid| bid >= p),
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
    use TimeInForce::{Gtc, Ioc, PostOnly};

    fn queue(m: &Matcher, side: Side, price: Price) -> Vec<OrderId> {
        m.book
            .level(side, price)
            .map_or(vec![], |level| level.orders().collect())
    }

    fn canceled(order_id: OrderId, leaves_qty: Qty) -> Event {
        Event::Canceled {
            order_id,
            leaves_qty,
        }
    }

    fn unknown_cancel(order_id: OrderId) -> Event {
        Event::CancelRejected {
            order_id,
            response_to: CancelRequest::Cancel,
            reason: CancelRejectReason::UnknownOrder,
        }
    }

    fn accepted_as(
        order_id: OrderId,
        side: Side,
        price: Price,
        time_in_force: TimeInForce,
        qty: Qty,
    ) -> Event {
        Event::Accepted {
            order_id,
            side,
            order_type: OrderType::Limit {
                price,
                time_in_force,
            },
            qty,
        }
    }

    fn accepted(order_id: OrderId, side: Side, price: Price, qty: Qty) -> Event {
        accepted_as(order_id, side, price, Gtc, qty)
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
        m.place_limit(1, Side::Sell, 101, Gtc, 5);
        assert_eq!(
            m.place_limit(2, Side::Buy, 100, Gtc, 3),
            [accepted(2, Side::Buy, 100, 3), l2(Side::Buy, 100, 3)]
        );
    }

    #[test]
    fn full_fill_at_the_makers_better_price_empties_the_level() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, Gtc, 5);
        assert_eq!(
            m.place_limit(2, Side::Buy, 105, Gtc, 5),
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
        m.place_limit(1, Side::Sell, 100, Gtc, 5);
        m.place_limit(2, Side::Sell, 100, Gtc, 5);
        m.place_limit(3, Side::Buy, 100, Gtc, 2);
        assert_eq!(
            m.place_limit(4, Side::Buy, 100, Gtc, 4),
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
        m.place_limit(1, Side::Sell, 101, Gtc, 2);
        m.place_limit(2, Side::Sell, 100, Gtc, 2);
        m.place_limit(3, Side::Sell, 100, Gtc, 3);
        m.place_limit(4, Side::Sell, 103, Gtc, 9);
        assert_eq!(
            m.place_limit(5, Side::Buy, 102, Gtc, 10),
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
        m.place_limit(1, Side::Buy, 99, Gtc, 4);
        m.place_limit(2, Side::Buy, 100, Gtc, 4);
        assert_eq!(
            m.place_limit(3, Side::Sell, 99, Gtc, 6),
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
        m.place_limit(1, Side::Buy, 102, Gtc, 5);
        assert_eq!(
            m.place_limit(2, Side::Buy, 102, Gtc, 3),
            [accepted(2, Side::Buy, 102, 3), l2(Side::Buy, 102, 8)]
        );
    }

    #[test]
    fn sell_remainder_rests_after_trading() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Buy, 100, Gtc, 2);
        assert_eq!(
            m.place_limit(2, Side::Sell, 100, Gtc, 5),
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
        m.place_limit(1, Side::Sell, 100, Gtc, 5);
        m.place_limit(1, Side::Buy, 100, Gtc, 8);
    }

    #[test]
    #[should_panic(expected = "positive quantity")]
    fn zero_quantity_panics() {
        Matcher::new().place_limit(1, Side::Buy, 100, Gtc, 0);
    }

    #[test]
    fn ioc_partial_fill_cancels_the_remainder() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, Gtc, 2);
        assert_eq!(
            m.place_limit(2, Side::Buy, 100, Ioc, 5),
            [
                accepted_as(2, Side::Buy, 100, Ioc, 5),
                trade(1, 2, Side::Buy, 100, 2),
                Event::Canceled {
                    order_id: 2,
                    leaves_qty: 3
                },
                l2(Side::Sell, 100, 0),
            ]
        );
        assert_eq!(m.book.best_bid(), None);
    }

    #[test]
    fn ioc_without_a_fill_is_accepted_then_canceled() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 101, Gtc, 2);
        assert_eq!(
            m.place_limit(2, Side::Buy, 100, Ioc, 5),
            [
                accepted_as(2, Side::Buy, 100, Ioc, 5),
                Event::Canceled {
                    order_id: 2,
                    leaves_qty: 5
                },
            ]
        );
        assert_eq!(m.book.best_bid(), None);
    }

    #[test]
    fn ioc_full_fill_emits_no_cancel() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, Gtc, 5);
        assert_eq!(
            m.place_limit(2, Side::Buy, 100, Ioc, 5),
            [
                accepted_as(2, Side::Buy, 100, Ioc, 5),
                trade(1, 2, Side::Buy, 100, 5),
                l2(Side::Sell, 100, 0),
            ]
        );
    }

    #[test]
    fn ioc_against_an_empty_book_is_accepted_then_canceled() {
        assert_eq!(
            Matcher::new().place_limit(1, Side::Sell, 100, Ioc, 4),
            [
                accepted_as(1, Side::Sell, 100, Ioc, 4),
                Event::Canceled {
                    order_id: 1,
                    leaves_qty: 4
                },
            ]
        );
    }

    #[test]
    fn crossing_sell_post_only_is_rejected_and_leaves_the_book_alone() {
        for price in [100, 99] {
            let mut m = Matcher::new();
            m.place_limit(1, Side::Buy, 100, Gtc, 5);
            let events = m.place_limit(2, Side::Sell, price, PostOnly, 3);
            let reason = RejectReason::PostOnlyWouldCross;
            assert_eq!(
                events,
                [Event::Rejected {
                    order_id: 2,
                    reason
                }]
            );
            assert_eq!((m.book.best_bid(), m.book.best_ask()), (Some(100), None));
            assert_eq!(m.book.level(Side::Buy, 100).map(|l| l.total()), Some(5));
        }
    }

    #[test]
    fn post_only_against_an_empty_side_rests() {
        assert_eq!(
            Matcher::new().place_limit(1, Side::Buy, 100, PostOnly, 3),
            [
                accepted_as(1, Side::Buy, 100, PostOnly, 3),
                l2(Side::Buy, 100, 3)
            ]
        );
    }

    #[test]
    #[should_panic(expected = "already resting")]
    fn duplicate_id_panics_even_on_a_crossing_post_only() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, Gtc, 5);
        m.place_limit(1, Side::Buy, 100, PostOnly, 3);
    }

    #[test]
    #[should_panic(expected = "positive quantity")]
    fn zero_quantity_panics_even_on_a_crossing_post_only() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, Gtc, 5);
        m.place_limit(2, Side::Buy, 100, PostOnly, 0);
    }

    #[test]
    fn crossing_post_only_is_rejected_and_leaves_the_book_alone() {
        for post_only_price in [100, 101] {
            let mut m = Matcher::new();
            m.place_limit(1, Side::Sell, 100, Gtc, 5);
            m.place_limit(2, Side::Buy, 99, Gtc, 4);
            let events = m.place_limit(3, Side::Buy, post_only_price, PostOnly, 3);
            let rejected = Event::Rejected {
                order_id: 3,
                reason: RejectReason::PostOnlyWouldCross,
            };
            assert_eq!(events, [rejected]);
            assert_eq!(
                (m.book.best_bid(), m.book.best_ask()),
                (Some(99), Some(100))
            );
            let total = |side, price| m.book.level(side, price).map(|l| l.total());
            assert_eq!(
                (total(Side::Buy, 99), total(Side::Sell, 100)),
                (Some(4), Some(5))
            );
        }
    }

    #[test]
    fn non_crossing_post_only_rests_like_gtc() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 101, Gtc, 5);
        assert_eq!(
            m.place_limit(2, Side::Buy, 100, PostOnly, 3),
            [
                accepted_as(2, Side::Buy, 100, PostOnly, 3),
                l2(Side::Buy, 100, 3)
            ]
        );
    }

    fn accepted_market(order_id: OrderId, side: Side, qty: Qty) -> Event {
        let order_type = OrderType::Market {};
        Event::Accepted {
            order_id,
            side,
            order_type,
            qty,
        }
    }

    #[test]
    fn market_buy_fills_within_one_level() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, Gtc, 5);
        assert_eq!(
            m.place_market(2, Side::Buy, 3),
            [
                accepted_market(2, Side::Buy, 3),
                trade(1, 2, Side::Buy, 100, 3),
                l2(Side::Sell, 100, 2),
            ]
        );
    }

    #[test]
    fn market_sell_sweeps_bids_in_price_time_order() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Buy, 99, Gtc, 4);
        m.place_limit(2, Side::Buy, 100, Gtc, 2);
        m.place_limit(3, Side::Buy, 100, Gtc, 2);
        assert_eq!(
            m.place_market(4, Side::Sell, 7),
            [
                accepted_market(4, Side::Sell, 7),
                trade(2, 4, Side::Sell, 100, 2),
                trade(3, 4, Side::Sell, 100, 2),
                trade(1, 4, Side::Sell, 99, 3),
                l2(Side::Buy, 100, 0),
                l2(Side::Buy, 99, 1),
            ]
        );
    }

    #[test]
    fn market_larger_than_the_book_cancels_the_remainder() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, Gtc, 2);
        m.place_limit(2, Side::Sell, 101, Gtc, 3);
        assert_eq!(
            m.place_market(3, Side::Buy, 9),
            [
                accepted_market(3, Side::Buy, 9),
                trade(1, 3, Side::Buy, 100, 2),
                trade(2, 3, Side::Buy, 101, 3),
                Event::Canceled {
                    order_id: 3,
                    leaves_qty: 4
                },
                l2(Side::Sell, 100, 0),
                l2(Side::Sell, 101, 0),
            ]
        );
        assert_eq!((m.book.best_bid(), m.book.best_ask()), (None, None));
    }

    #[test]
    fn market_against_an_empty_side_is_rejected() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Buy, 100, Gtc, 5);
        let reason = RejectReason::NoLiquidity;
        assert_eq!(
            m.place_market(2, Side::Buy, 3),
            [Event::Rejected {
                order_id: 2,
                reason
            }]
        );
        assert_eq!((m.book.best_bid(), m.book.best_ask()), (Some(100), None));
        assert_eq!(m.book.level(Side::Buy, 100).map(|l| l.total()), Some(5));
    }

    #[test]
    fn market_equal_to_the_depth_empties_the_side_without_cancel() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, Gtc, 2);
        m.place_limit(2, Side::Sell, 101, Gtc, 3);
        assert_eq!(
            m.place_market(3, Side::Buy, 5),
            [
                accepted_market(3, Side::Buy, 5),
                trade(1, 3, Side::Buy, 100, 2),
                trade(2, 3, Side::Buy, 101, 3),
                l2(Side::Sell, 100, 0),
                l2(Side::Sell, 101, 0),
            ]
        );
        assert_eq!(m.book.best_ask(), None);
    }

    #[test]
    #[should_panic(expected = "already resting")]
    fn market_duplicate_id_panics_even_on_an_empty_side() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Buy, 100, Gtc, 5);
        m.place_market(1, Side::Buy, 3);
    }

    #[test]
    #[should_panic(expected = "positive quantity")]
    fn market_zero_quantity_panics_even_on_an_empty_side() {
        Matcher::new().place_market(1, Side::Buy, 0);
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
            let mut events = orders
                .map(|(id, side, price, qty)| m.place_limit(id, side, price, Gtc, qty))
                .to_vec();
            events.push(m.place_market(6, Side::Buy, 10));
            events
        };
        assert_eq!(run(), run());
    }

    /// Three orders at 100 on `side` (ids 1, 2, 3; qty 10, 20, 30).
    fn matcher_with_queue(side: Side) -> Matcher {
        let mut m = Matcher::new();
        for (id, qty) in [(1, 10), (2, 20), (3, 30)] {
            m.place_limit(id, side, 100, Gtc, qty);
        }
        m
    }

    #[test]
    fn cancel_front_middle_and_back_keeps_fifo_of_the_rest() {
        for side in [Side::Buy, Side::Sell] {
            for (id, qty, rest, total) in [
                (1, 10, [2, 3], 50),
                (2, 20, [1, 3], 40),
                (3, 30, [1, 2], 30),
            ] {
                let mut m = matcher_with_queue(side);
                assert_eq!(m.cancel(id), [canceled(id, qty), l2(side, 100, total)]);
                assert_eq!(queue(&m, side, 100), rest);
            }
        }
    }

    #[test]
    fn canceling_the_last_order_removes_the_level() {
        for side in [Side::Buy, Side::Sell] {
            let mut m = Matcher::new();
            m.place_limit(1, side, 100, Gtc, 5);
            m.place_limit(2, side, 99, Gtc, 5);
            assert_eq!(m.cancel(1), [canceled(1, 5), l2(side, 100, 0)]);
            assert!(m.book.level(side, 100).is_none());
            assert_eq!(queue(&m, side, 99), [2]);
        }
    }

    #[test]
    fn cancel_reports_the_open_quantity_of_a_partly_filled_order() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, Gtc, 5);
        m.place_limit(2, Side::Buy, 100, Gtc, 2);
        assert_eq!(m.cancel(1), [canceled(1, 3), l2(Side::Sell, 100, 0)]);
    }

    #[test]
    fn canceling_an_unknown_id_is_rejected_and_leaves_the_book_alone() {
        let mut m = matcher_with_queue(Side::Buy);
        assert_eq!(m.cancel(99), [unknown_cancel(99)]);
        assert_eq!(queue(&m, Side::Buy, 100), [1, 2, 3]);
        assert_eq!(m.book.level(Side::Buy, 100).map(|l| l.total()), Some(60));
    }

    #[test]
    fn canceling_a_filled_id_is_rejected() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 100, Gtc, 5);
        m.place_limit(2, Side::Buy, 100, Gtc, 5);
        assert_eq!(m.cancel(1), [unknown_cancel(1)]);
        assert_eq!(m.cancel(2), [unknown_cancel(2)]);
    }

    #[test]
    fn canceling_twice_is_rejected_the_second_time() {
        let mut m = matcher_with_queue(Side::Sell);
        m.cancel(2);
        assert_eq!(m.cancel(2), [unknown_cancel(2)]);
        assert_eq!(queue(&m, Side::Sell, 100), [1, 3]);
    }

    #[test]
    fn a_canceled_id_can_be_placed_again() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Buy, 100, Gtc, 5);
        m.cancel(1);
        assert_eq!(
            m.place_limit(1, Side::Buy, 99, Gtc, 2),
            [accepted(1, Side::Buy, 99, 2), l2(Side::Buy, 99, 2)]
        );
    }

    fn replaced(order_id: OrderId, price: Price, qty: Qty) -> Event {
        Event::Replaced {
            order_id,
            price,
            qty,
        }
    }

    fn unknown_replace(order_id: OrderId) -> Event {
        Event::CancelRejected {
            order_id,
            response_to: CancelRequest::Replace,
            reason: CancelRejectReason::UnknownOrder,
        }
    }

    #[test]
    fn reducing_keeps_priority_so_a_later_taker_fills_it_first() {
        for (side, taker_side) in [(Side::Buy, Side::Sell), (Side::Sell, Side::Buy)] {
            let mut m = matcher_with_queue(side);
            assert_eq!(
                m.replace(1, 100, 4),
                [replaced(1, 100, 4), l2(side, 100, 54)]
            );
            assert_eq!(queue(&m, side, 100), [1, 2, 3]);
            assert_eq!(
                m.place_limit(9, taker_side, 100, Ioc, 4),
                [
                    accepted_as(9, taker_side, 100, Ioc, 4),
                    trade(1, 9, taker_side, 100, 4),
                    l2(side, 100, 50),
                ]
            );
            assert_eq!(queue(&m, side, 100), [2, 3]);
        }
    }

    #[test]
    fn replace_without_changes_is_reported_and_keeps_priority() {
        let mut m = matcher_with_queue(Side::Buy);
        assert_eq!(
            m.replace(1, 100, 10),
            [replaced(1, 100, 10), l2(Side::Buy, 100, 60)]
        );
        assert_eq!(queue(&m, Side::Buy, 100), [1, 2, 3]);
    }

    #[test]
    fn a_price_change_moves_the_order_behind_those_at_the_new_price() {
        for side in [Side::Buy, Side::Sell] {
            let mut m = matcher_with_queue(side);
            m.place_limit(4, side, 99, Gtc, 7);
            assert_eq!(
                m.replace(4, 100, 7),
                [replaced(4, 100, 7), l2(side, 99, 0), l2(side, 100, 67)]
            );
            assert_eq!(queue(&m, side, 100), [1, 2, 3, 4]);
            assert!(m.book.level(side, 99).is_none());
        }
    }

    #[test]
    fn a_price_change_with_a_smaller_quantity_still_loses_priority() {
        let mut m = matcher_with_queue(Side::Sell);
        m.place_limit(4, Side::Sell, 101, Gtc, 8);
        assert_eq!(
            m.replace(1, 101, 3),
            [
                replaced(1, 101, 3),
                l2(Side::Sell, 100, 50),
                l2(Side::Sell, 101, 11)
            ]
        );
        assert_eq!(queue(&m, Side::Sell, 101), [4, 1]);
        assert_eq!(queue(&m, Side::Sell, 100), [2, 3]);
    }

    #[test]
    fn a_quantity_increase_moves_the_order_to_the_back_of_its_level() {
        for side in [Side::Buy, Side::Sell] {
            let mut m = matcher_with_queue(side);
            assert_eq!(
                m.replace(1, 100, 15),
                [replaced(1, 100, 15), l2(side, 100, 65)]
            );
            assert_eq!(queue(&m, side, 100), [2, 3, 1]);
        }
    }

    #[test]
    fn a_requeued_order_fills_after_the_others() {
        let mut m = matcher_with_queue(Side::Sell);
        m.replace(1, 100, 11);
        assert_eq!(
            m.place_limit(9, Side::Buy, 100, Ioc, 25),
            [
                accepted_as(9, Side::Buy, 100, Ioc, 25),
                trade(2, 9, Side::Buy, 100, 20),
                trade(3, 9, Side::Buy, 100, 5),
                l2(Side::Sell, 100, 36),
            ]
        );
    }

    #[test]
    fn replacing_an_unknown_id_is_rejected_and_leaves_the_book_alone() {
        let mut m = matcher_with_queue(Side::Buy);
        assert_eq!(m.replace(99, 101, 5), [unknown_replace(99)]);
        assert_eq!(queue(&m, Side::Buy, 100), [1, 2, 3]);
        m.cancel(1);
        assert_eq!(m.replace(1, 100, 5), [unknown_replace(1)]);
    }

    #[test]
    #[should_panic(expected = "positive quantity")]
    fn replace_with_zero_quantity_panics() {
        matcher_with_queue(Side::Buy).replace(1, 100, 0);
    }

    fn refused_post_only_replace(order_id: OrderId) -> Event {
        Event::CancelRejected {
            order_id,
            response_to: CancelRequest::Replace,
            reason: CancelRejectReason::PostOnlyWouldCross,
        }
    }

    #[test]
    fn a_crossing_post_only_replace_is_refused_and_changes_nothing() {
        // The order rests at 100; the opposite best is 105 for a bid, 95 for an ask.
        // The targets are an equal and a better (more aggressive) price.
        for (side, taker_side, best, better) in [
            (Side::Buy, Side::Sell, 105, 106),
            (Side::Sell, Side::Buy, 95, 94),
        ] {
            for target in [best, better] {
                let mut m = Matcher::new();
                m.place_limit(1, side, 100, Gtc, 4);
                m.place_limit(2, side, 100, PostOnly, 6);
                m.place_limit(3, taker_side, best, Gtc, 9);
                assert_eq!(m.replace(2, target, 3), [refused_post_only_replace(2)]);
                assert_eq!(queue(&m, side, 100), [1, 2]);
                assert_eq!(m.book.order(2).map(|o| (o.price, o.qty)), Some((100, 6)));
                assert_eq!(queue(&m, taker_side, best), [3]);
            }
        }
    }

    #[test]
    fn a_post_only_replace_that_does_not_cross_is_an_ordinary_replace() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 105, Gtc, 9);
        m.place_limit(2, Side::Buy, 100, PostOnly, 6);
        assert_eq!(
            m.replace(2, 104, 6),
            [
                replaced(2, 104, 6),
                l2(Side::Buy, 100, 0),
                l2(Side::Buy, 104, 6)
            ]
        );
    }

    #[test]
    fn a_gtc_replace_that_fully_fills_rests_nothing() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 101, Gtc, 5);
        m.place_limit(2, Side::Buy, 99, Gtc, 5);
        assert_eq!(
            m.replace(2, 102, 5),
            [
                replaced(2, 102, 5),
                trade(1, 2, Side::Buy, 101, 5),
                l2(Side::Buy, 99, 0),
                l2(Side::Sell, 101, 0),
            ]
        );
        assert!(!m.book.contains(2));
    }

    #[test]
    fn a_partly_filled_replace_rests_the_remainder_behind_existing_orders() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Buy, 100, Gtc, 4);
        m.place_limit(2, Side::Buy, 98, Gtc, 6);
        m.place_limit(3, Side::Sell, 101, Gtc, 2);
        assert_eq!(
            m.replace(2, 101, 6),
            [
                replaced(2, 101, 6),
                trade(3, 2, Side::Buy, 101, 2),
                l2(Side::Buy, 98, 0),
                l2(Side::Sell, 101, 0),
                l2(Side::Buy, 101, 4),
            ]
        );
        m.place_limit(4, Side::Buy, 101, Gtc, 1);
        assert_eq!(queue(&m, Side::Buy, 101), [2, 4]);
    }

    #[test]
    fn a_marketable_replace_sweeps_levels_at_the_makers_prices() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Buy, 100, Gtc, 10);
        m.place_limit(2, Side::Sell, 101, Gtc, 3);
        m.place_limit(3, Side::Sell, 101, Gtc, 2);
        m.place_limit(4, Side::Sell, 102, Gtc, 4);
        m.place_limit(5, Side::Sell, 103, Gtc, 9);
        assert_eq!(
            m.replace(1, 102, 12),
            [
                replaced(1, 102, 12),
                trade(2, 1, Side::Buy, 101, 3),
                trade(3, 1, Side::Buy, 101, 2),
                trade(4, 1, Side::Buy, 102, 4),
                l2(Side::Buy, 100, 0),
                l2(Side::Sell, 101, 0),
                l2(Side::Sell, 102, 0),
                l2(Side::Buy, 102, 3),
            ]
        );
        assert_eq!(m.book.best_ask(), Some(103));
    }

    #[test]
    fn a_marketable_sell_replace_trades_against_bids() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 105, Gtc, 4);
        m.place_limit(2, Side::Buy, 100, Gtc, 3);
        assert_eq!(
            m.replace(1, 99, 4),
            [
                replaced(1, 99, 4),
                trade(2, 1, Side::Sell, 100, 3),
                l2(Side::Sell, 105, 0),
                l2(Side::Buy, 100, 0),
                l2(Side::Sell, 99, 1),
            ]
        );
    }

    #[test]
    fn a_quantity_increase_that_stays_passive_is_unaffected_by_the_opposite_side() {
        let mut m = Matcher::new();
        m.place_limit(1, Side::Sell, 105, Gtc, 1);
        m.place_limit(2, Side::Buy, 100, PostOnly, 2);
        assert_eq!(
            m.replace(2, 100, 5),
            [replaced(2, 100, 5), l2(Side::Buy, 100, 5)]
        );
    }
}
