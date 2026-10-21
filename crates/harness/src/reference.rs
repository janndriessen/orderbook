//! An independent oracle for the matcher: a second, deliberately naive
//! implementation of the same documented behavior.
//!
//! Resting orders sit in one `Vec`, the best maker is found by scanning it and
//! level totals are summed on demand. It shares no code with the real
//! matcher and is simple enough to check by reading, so when the two disagree
//! on an event, one of them is mistaken. Speed is irrelevant here.
//!
//! Inputs must lie within the gateway's bounds: positive quantities for places
//! and replaces, and order ids of places that were never used before.

use protocol::{
    CancelRejectReason, CancelRequest, Command, Event, OrderId, OrderType, Price, Qty,
    RejectReason, Side, TimeInForce,
};

/// An order resting in the reference book.
#[derive(Debug)]
struct Resting {
    id: OrderId,
    side: Side,
    price: Price,
    qty: Qty,
    time_in_force: TimeInForce,
    /// Position in the order of arrival; lower is older.
    arrival: u64,
}

/// A naive matcher whose events the real one must reproduce exactly.
#[derive(Debug, Default)]
pub struct ReferenceMatcher {
    orders: Vec<Resting>,
    arrivals: u64,
}

impl ReferenceMatcher {
    /// A matcher over an empty book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs one command and returns the events it produced.
    pub fn apply(&mut self, command: Command) -> Vec<Event> {
        match command {
            Command::Place {
                order_id,
                side,
                order_type,
                qty,
            } => self.place(order_id, side, order_type, qty),
            Command::Cancel { order_id } => self.cancel(order_id),
            Command::Replace {
                order_id,
                price,
                qty,
            } => self.replace(order_id, price, qty),
        }
    }

    fn place(&mut self, id: OrderId, side: Side, order_type: OrderType, qty: Qty) -> Vec<Event> {
        let (limit, time_in_force) = match order_type {
            OrderType::Limit {
                price,
                time_in_force,
            } => (Some(price), Some(time_in_force)),
            OrderType::Market {} => (None, None),
        };
        let trades = self.best_maker(side, limit).is_some();
        if time_in_force == Some(TimeInForce::PostOnly) && trades {
            return vec![reject(id, RejectReason::PostOnlyWouldCross)];
        }
        if limit.is_none() && !trades {
            return vec![reject(id, RejectReason::NoLiquidity)];
        }
        let mut events = vec![Event::Accepted {
            order_id: id,
            side,
            order_type,
            qty,
        }];
        let mut touched = Vec::new();
        let left = self.take(id, side, limit, qty, &mut events, &mut touched);
        match (limit, time_in_force) {
            (Some(price), Some(tif)) if tif != TimeInForce::Ioc && left > 0 => {
                self.rest(id, side, price, left, tif);
                touch(&mut touched, (side, price));
            }
            _ if left > 0 => events.push(Event::Canceled {
                order_id: id,
                leaves_qty: left,
            }),
            _ => {}
        }
        self.report_levels(&touched, &mut events);
        events
    }

    fn cancel(&mut self, id: OrderId) -> Vec<Event> {
        let Some(index) = self.orders.iter().position(|o| o.id == id) else {
            return vec![refuse(
                id,
                CancelRequest::Cancel,
                CancelRejectReason::UnknownOrder,
            )];
        };
        let order = self.orders.remove(index);
        let mut events = vec![Event::Canceled {
            order_id: id,
            leaves_qty: order.qty,
        }];
        self.report_levels(&[(order.side, order.price)], &mut events);
        events
    }

    fn replace(&mut self, id: OrderId, price: Price, qty: Qty) -> Vec<Event> {
        let Some(index) = self.orders.iter().position(|o| o.id == id) else {
            return vec![refuse(
                id,
                CancelRequest::Replace,
                CancelRejectReason::UnknownOrder,
            )];
        };
        let (side, old_price, old_qty, tif) = {
            let o = &self.orders[index];
            (o.side, o.price, o.qty, o.time_in_force)
        };
        let mut events = vec![Event::Replaced {
            order_id: id,
            price,
            qty,
        }];
        let mut touched = vec![(side, old_price)];
        if price == old_price && qty <= old_qty {
            self.orders[index].qty = qty;
        } else {
            if tif == TimeInForce::PostOnly && self.best_maker(side, Some(price)).is_some() {
                return vec![refuse(
                    id,
                    CancelRequest::Replace,
                    CancelRejectReason::PostOnlyWouldCross,
                )];
            }
            self.orders.remove(index);
            let left = self.take(id, side, Some(price), qty, &mut events, &mut touched);
            if left > 0 {
                self.rest(id, side, price, left, tif);
                touch(&mut touched, (side, price));
            }
        }
        self.report_levels(&touched, &mut events);
        events
    }

    /// Index of the maker an order on `side` would trade with first: the best
    /// price for the taker, the oldest arrival within it. `limit` is the
    /// taker's worst acceptable price; `None` takes any.
    fn best_maker(&self, side: Side, limit: Option<Price>) -> Option<usize> {
        let beats = |a: &Resting, b: &Resting| {
            if a.price == b.price {
                a.arrival < b.arrival
            } else {
                (side == Side::Buy) == (a.price < b.price)
            }
        };
        let mut best: Option<usize> = None;
        for (i, order) in self.orders.iter().enumerate() {
            if order.side != side && best.is_none_or(|b| beats(order, &self.orders[b])) {
                best = Some(i);
            }
        }
        let maker = &self.orders[best?];
        let allowed = match (side, limit) {
            (_, None) => true,
            (Side::Buy, Some(limit)) => maker.price <= limit,
            (Side::Sell, Some(limit)) => maker.price >= limit,
        };
        allowed.then_some(best?)
    }

    /// Trades up to `qty` against the best makers, records the levels it
    /// consumed in `touched` and returns what is left.
    fn take(
        &mut self,
        taker: OrderId,
        side: Side,
        limit: Option<Price>,
        mut qty: Qty,
        events: &mut Vec<Event>,
        touched: &mut Vec<(Side, Price)>,
    ) -> Qty {
        while qty > 0 {
            let Some(index) = self.best_maker(side, limit) else {
                break;
            };
            let maker = &mut self.orders[index];
            let fill = qty.min(maker.qty);
            events.push(Event::Trade {
                maker_order_id: maker.id,
                taker_order_id: taker,
                aggressor_side: side,
                price: maker.price,
                qty: fill,
            });
            touch(touched, (maker.side, maker.price));
            maker.qty -= fill;
            qty -= fill;
            if maker.qty == 0 {
                self.orders.remove(index);
            }
        }
        qty
    }

    fn rest(
        &mut self,
        id: OrderId,
        side: Side,
        price: Price,
        qty: Qty,
        time_in_force: TimeInForce,
    ) {
        self.arrivals += 1;
        self.orders.push(Resting {
            id,
            side,
            price,
            qty,
            time_in_force,
            arrival: self.arrivals,
        });
    }

    /// Appends one `L2Update` per touched level with the level's total now.
    fn report_levels(&self, touched: &[(Side, Price)], events: &mut Vec<Event>) {
        for &(side, price) in touched {
            let qty = self
                .orders
                .iter()
                .filter(|o| o.side == side && o.price == price)
                .map(|o| o.qty)
                .sum();
            events.push(Event::L2Update { side, price, qty });
        }
    }
}

/// Notes a level as touched, once, in the order it was first touched.
fn touch(touched: &mut Vec<(Side, Price)>, level: (Side, Price)) {
    if !touched.contains(&level) {
        touched.push(level);
    }
}

fn reject(order_id: OrderId, reason: RejectReason) -> Event {
    Event::Rejected { order_id, reason }
}

fn refuse(order_id: OrderId, response_to: CancelRequest, reason: CancelRejectReason) -> Event {
    Event::CancelRejected {
        order_id,
        response_to,
        reason,
    }
}
