//! Invariant checks over the matcher: a [`Run`] checks the book's structure
//! and the consistency of the events after every command. It does not know
//! the right outcome of a command, so it catches a book or feed that is
//! inconsistent, not one that is consistent but wrong. Failures panic with a
//! message naming the broken invariant; the calling test adds what is needed
//! to reproduce it.

use std::collections::{HashMap, HashSet};

use matching::{Matcher, OrderBook};
use protocol::{Command, Event, OrderId, OrderType, Price, Qty, Side, TimeInForce};

/// Checks the structure of a book.
///
/// # Panics
///
/// If the book is crossed, a level is empty or its total differs from the sum
/// of its orders, a queued order is missing from the index, queued twice or
/// disagrees with its level, or the queues do not hold exactly the indexed
/// orders.
fn check_book(book: &OrderBook) {
    if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
        assert!(bid < ask, "crossed book: best bid {bid} >= best ask {ask}");
    }
    let mut queued = HashSet::new();
    for side in [Side::Buy, Side::Sell] {
        for (price, level) in book.levels(side) {
            let mut sum = 0;
            let mut count = 0;
            for id in level.orders() {
                assert!(queued.insert(id), "order {id} is queued twice");
                let order = book
                    .order(id)
                    .unwrap_or_else(|| panic!("order {id} is queued but not indexed"));
                assert!(
                    order.side == side && order.price == price,
                    "order {id} is queued at {side:?} {price} but indexed as {order:?}"
                );
                assert!(order.qty > 0, "resting order {id} has no quantity");
                assert!(
                    order.time_in_force != TimeInForce::Ioc,
                    "IOC order {id} rests in the book"
                );
                sum += order.qty;
                count += 1;
            }
            assert!(count > 0, "empty level {side:?} {price}");
            assert!(
                sum == level.total(),
                "level {side:?} {price} totals {} but its orders sum to {sum}",
                level.total()
            );
        }
    }
    assert!(
        queued.len() == book.len(),
        "{} orders are queued but {} are indexed",
        queued.len(),
        book.len()
    );
}

/// A matcher plus the bookkeeping needed to check the events of each command.
#[derive(Debug, Default)]
pub struct Run {
    matcher: Matcher,
    /// Side and current price of every limit order of the run, to judge
    /// trades against their maker, who may be gone from the book by then.
    makers: HashMap<OrderId, (Side, Price)>,
    /// Every level as the `L2Update`s describe it, to compare the feed with
    /// the book: a missing, stale or misplaced update makes them differ.
    feed: HashMap<(Side, Price), Qty>,
}

impl Run {
    /// A run over an empty book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies `command`, checks the events and the book, and returns the
    /// events.
    ///
    /// # Panics
    ///
    /// If a `Place` is not answered by exactly one leading `Accepted` or
    /// `Rejected`, an IOC or market order rests, a `Trade` or `Replaced` names
    /// an order never accepted, a `Trade` is empty, off the maker's price or
    /// on the maker's side, the levels the `L2Update`s describe differ from
    /// the book's, or the book's structure is broken.
    pub fn apply(&mut self, command: Command) -> Vec<Event> {
        let events = self.matcher.apply(command);
        let book = self.matcher.book();
        if let Command::Place {
            order_id,
            order_type,
            ..
        } = command
        {
            let answers = |event: &Event| {
                matches!(event, Event::Accepted { order_id: id, .. }
                    | Event::Rejected { order_id: id, .. } if *id == order_id)
            };
            assert!(
                events.first().is_some_and(answers)
                    && events.iter().filter(|e| answers(e)).count() == 1,
                "order {order_id} is not answered by exactly one leading Accepted or Rejected: {events:?}"
            );
            let never_rests = matches!(
                order_type,
                OrderType::Market {}
                    | OrderType::Limit {
                        time_in_force: TimeInForce::Ioc,
                        ..
                    }
            );
            assert!(
                !(never_rests && book.contains(order_id)),
                "IOC or market order {order_id} rests after its command"
            );
        }
        for event in &events {
            match *event {
                Event::Accepted {
                    order_id,
                    side,
                    order_type: OrderType::Limit { price, .. },
                    ..
                } => {
                    self.makers.insert(order_id, (side, price));
                }
                Event::Replaced {
                    order_id, price, ..
                } => {
                    let maker = self
                        .makers
                        .get_mut(&order_id)
                        .unwrap_or_else(|| panic!("replace of an order never accepted: {event:?}"));
                    maker.1 = price;
                }
                Event::Trade {
                    maker_order_id,
                    aggressor_side,
                    price,
                    qty,
                    ..
                } => {
                    let (side, maker_price) =
                        *self.makers.get(&maker_order_id).unwrap_or_else(|| {
                            panic!("trade against an order never accepted: {event:?}")
                        });
                    assert!(qty > 0, "trade with no quantity: {event:?}");
                    assert!(
                        price == maker_price,
                        "trade at {price}, not at the maker's price {maker_price}: {event:?}"
                    );
                    assert!(
                        aggressor_side == side.opposite(),
                        "trade aggressor {aggressor_side:?} is on the maker's side: {event:?}"
                    );
                }
                Event::L2Update { side, price, qty } => {
                    if qty == 0 {
                        self.feed.remove(&(side, price));
                    } else {
                        self.feed.insert((side, price), qty);
                    }
                }
                _ => {}
            }
        }
        let levels: HashMap<(Side, Price), Qty> = [Side::Buy, Side::Sell]
            .into_iter()
            .flat_map(|side| {
                book.levels(side)
                    .map(move |(price, level)| ((side, price), level.total()))
            })
            .collect();
        assert!(
            self.feed == levels,
            "the L2Updates describe {:?} but the book holds {levels:?}",
            self.feed
        );
        check_book(book);
        events
    }
}
