import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  accepted,
  level,
  snapshot,
  statusChanged,
  trade,
  update,
} from "../test/fixtures";
import {
  BACKOFF_BASE_MS,
  BACKOFF_CAP_MS,
  CONNECT_TIMEOUT_MS,
  FeedHandler,
  HEARTBEAT_INTERVAL_MS,
  SILENCE_TIMEOUT_MS,
  type Transport,
} from "./FeedHandler";
import { MAX_ORDER_ENTRIES } from "./log";

type FakeSocket = {
  closed: boolean;
  send(data: unknown): void;
  drop(): void;
  close(): void;
};

function setup() {
  const sockets: FakeSocket[] = [];
  const transport: Transport = (_url, { onMessage, onClose }) => {
    const socket: FakeSocket = {
      closed: false,
      send: onMessage,
      drop: onClose,
      close() {
        socket.closed = true;
      },
    };
    sockets.push(socket);
    return socket;
  };
  // Midpoint random: the delay is halfway between the floor and the ceiling.
  const handler = new FeedHandler({
    url: "ws://test/ws",
    transport,
    random: () => 0.5,
  });
  const last = () => {
    const socket = sockets.at(-1);
    if (!socket) throw new Error("no socket");
    return socket;
  };
  /** Advances time until the handler opens its next socket; returns the wait. */
  const waitForReconnect = () => {
    const before = sockets.length;
    let waited = 0;
    while (sockets.length === before) {
      vi.advanceTimersByTime(1);
      waited += 1;
    }
    return waited;
  };
  return { handler, sockets, last, waitForReconnect };
}

const wire = (message: unknown) => JSON.stringify(message);

const delayFor = (attempt: number) => {
  const ceiling = Math.min(BACKOFF_CAP_MS, BACKOFF_BASE_MS * 2 ** attempt);
  return Math.round(BACKOFF_BASE_MS + (ceiling - BACKOFF_BASE_MS) / 2);
};

describe("FeedHandler", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("is connecting until the snapshot arrives, then live", () => {
    const { handler, last } = setup();
    handler.start();
    expect(handler.getState()).toMatchObject({
      status: "connecting",
      book: null,
    });
    last().send(wire(snapshot({ events: [accepted(1)] })));
    const state = handler.getState();
    expect(state.status).toBe("live");
    expect(state.book?.seq).toBe(1);
    expect(state.market?.symbol).toBe("BTC-USD");
    expect(state.log).toHaveLength(1);
  });

  it("counts a snapshot with a lower sequence as a restart, not a resync", () => {
    const { handler, last, waitForReconnect } = setup();
    handler.start();
    last().send(wire(snapshot({ seq: 5 })));
    last().send(wire(update({ seq: 6 })));
    last().drop();
    waitForReconnect();
    last().send(wire(snapshot({ seq: 6 })));
    expect(handler.getState().restarts).toBe(0);
    last().send(wire(update({ seq: 9 })));
    waitForReconnect();
    last().send(wire(snapshot({ seq: 20 })));
    expect(handler.getState().restarts).toBe(0);
    last().drop();
    waitForReconnect();
    last().send(wire(snapshot({ seq: 1 })));
    expect(handler.getState().restarts).toBe(1);
  });

  it("starts clean when a stopped handler is started again", () => {
    const { handler, last } = setup();
    handler.start();
    last().send(wire(snapshot({ seq: 5 })));
    handler.stop();
    handler.start();
    last().send(wire(snapshot({ seq: 1 })));
    expect(handler.getState().restarts).toBe(0);
  });

  it("ignores a heartbeat: no state change, no notification", () => {
    const { handler, last } = setup();
    handler.start();
    last().send(wire(snapshot({ events: [accepted(1)] })));
    const before = handler.getState();
    const listener = vi.fn();
    handler.subscribe(listener);
    last().send(wire({ type: "heartbeat", seq: 1 }));
    expect(handler.getState()).toBe(before);
    expect(listener).not.toHaveBeenCalled();
  });

  it("recovers when a heartbeat's seq differs from the held book's", () => {
    const { handler, sockets, last, waitForReconnect } = setup();
    handler.start();
    last().send(wire(snapshot()));
    const first = last();
    first.send(wire({ type: "heartbeat", seq: 2 }));
    expect(first.closed).toBe(true);
    expect(handler.getState()).toMatchObject({
      status: "reconnecting",
      book: null,
    });
    waitForReconnect();
    expect(sockets).toHaveLength(2);
  });

  it("ignores a heartbeat before the first snapshot", () => {
    const { handler, sockets, last } = setup();
    const listener = vi.fn();
    handler.subscribe(listener);
    handler.start();
    listener.mockClear();
    last().send(wire({ type: "heartbeat", seq: 7 }));
    expect(handler.getState().status).toBe("connecting");
    expect(sockets).toHaveLength(1);
    expect(sockets[0]?.closed).toBe(false);
    expect(listener).not.toHaveBeenCalled();
  });

  describe("liveness watchdog", () => {
    const beat = wire({ type: "heartbeat", seq: 1 });

    it("gives a socket that never sent anything the connect window", () => {
      const { handler, sockets, last, waitForReconnect } = setup();
      handler.start();
      const first = last();
      vi.advanceTimersByTime(CONNECT_TIMEOUT_MS - 1);
      expect(first.closed).toBe(false);
      vi.advanceTimersByTime(1);
      expect(first.closed).toBe(true);
      expect(handler.getState().status).toBe("connecting");
      waitForReconnect();
      expect(sockets).toHaveLength(2);
    });

    it("drops a socket that sends only heartbeats before any snapshot", () => {
      const { handler, sockets, last, waitForReconnect } = setup();
      handler.start();
      const first = last();
      for (let i = 0; i < 9; i++) {
        vi.advanceTimersByTime(HEARTBEAT_INTERVAL_MS);
        first.send(beat);
      }
      expect(first.closed).toBe(false);
      vi.advanceTimersByTime(CONNECT_TIMEOUT_MS - 9 * HEARTBEAT_INTERVAL_MS);
      expect(first.closed).toBe(true);
      expect(handler.getState().status).toBe("connecting");
      waitForReconnect();
      expect(sockets).toHaveLength(2);
    });

    it("keeps a socket whose snapshot arrives within the connect window", () => {
      const { handler, sockets, last } = setup();
      handler.start();
      vi.advanceTimersByTime(CONNECT_TIMEOUT_MS - 1);
      last().send(wire(snapshot()));
      for (let i = 0; i < 3; i++) {
        vi.advanceTimersByTime(HEARTBEAT_INTERVAL_MS);
        last().send(beat);
      }
      expect(handler.getState().status).toBe("live");
      expect(sockets).toHaveLength(1);
    });

    it("reports reconnecting when a live feed goes silent", () => {
      const { handler, last } = setup();
      handler.start();
      last().send(wire(snapshot()));
      vi.advanceTimersByTime(SILENCE_TIMEOUT_MS - 1);
      expect(handler.getState().status).toBe("live");
      vi.advanceTimersByTime(1);
      expect(handler.getState()).toMatchObject({
        status: "reconnecting",
        book: null,
      });
    });

    it("stays alive while a message arrives every heartbeat interval", () => {
      const { handler, sockets, last } = setup();
      handler.start();
      last().send(wire(snapshot()));
      for (let i = 0; i < 10; i++) {
        vi.advanceTimersByTime(HEARTBEAT_INTERVAL_MS);
        last().send(beat);
      }
      expect(handler.getState().status).toBe("live");
      expect(sockets).toHaveLength(1);
    });

    it("tolerates a heartbeat arriving three intervals late", () => {
      const { handler, sockets, last } = setup();
      handler.start();
      last().send(wire(snapshot()));
      vi.advanceTimersByTime(3 * HEARTBEAT_INTERVAL_MS);
      last().send(beat);
      expect(handler.getState().status).toBe("live");
      expect(sockets).toHaveLength(1);
    });

    it("leaves no pending timer after stop", () => {
      const { handler, sockets, last } = setup();
      handler.start();
      last().send(wire(snapshot()));
      handler.stop();
      expect(vi.getTimerCount()).toBe(0);
      vi.advanceTimersByTime(SILENCE_TIMEOUT_MS * 10);
      expect(sockets).toHaveLength(1);
    });

    it("ignores a stale socket's late message for the new timer", () => {
      const { handler, sockets, last, waitForReconnect } = setup();
      handler.start();
      const stale = last();
      stale.drop();
      waitForReconnect();
      expect(sockets).toHaveLength(2);
      vi.advanceTimersByTime(CONNECT_TIMEOUT_MS - 1);
      stale.send(beat);
      vi.advanceTimersByTime(1);
      expect(last().closed).toBe(true);
    });

    it("arms the reopened socket again after a reconnect", () => {
      const { handler, sockets, last, waitForReconnect } = setup();
      handler.start();
      last().drop();
      waitForReconnect();
      expect(sockets).toHaveLength(2);
      vi.advanceTimersByTime(CONNECT_TIMEOUT_MS);
      expect(sockets[1]?.closed).toBe(true);
    });

    it("leaves only the backoff timer pending after a malformed message", () => {
      const { handler, last } = setup();
      handler.start();
      last().send(wire(snapshot()));
      last().send("not json");
      expect(vi.getTimerCount()).toBe(1);
    });
  });

  it("applies updates in sequence and grows the log newest first", () => {
    const { handler, last } = setup();
    handler.start();
    last().send(wire(snapshot({ events: [accepted(1)] })));
    last().send(
      wire(update({ seq: 2, bids: [level(1_000_000, 0)], events: [trade(2)] })),
    );
    const { book, log } = handler.getState();
    expect(book?.bids.has(1_000_000)).toBe(false);
    expect(log.map((e) => e.seq)).toEqual([2, 1]);
  });

  it("notifies subscribers and stops after unsubscribe", () => {
    const { handler, last } = setup();
    const listener = vi.fn();
    const unsubscribe = handler.subscribe(listener);
    handler.start();
    listener.mockClear();
    last().send(wire(snapshot()));
    expect(listener).toHaveBeenCalledTimes(1);
    unsubscribe();
    last().send(wire(update()));
    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("drops the book after a gap but keeps market and log until the next snapshot", () => {
    const { handler, sockets, last, waitForReconnect } = setup();
    handler.start();
    last().send(wire(snapshot({ events: [accepted(1)] })));
    const first = last();
    first.send(wire(update({ seq: 5 })));
    expect(first.closed).toBe(true);
    expect(handler.getState()).toMatchObject({
      status: "reconnecting",
      book: null,
    });
    expect(handler.getState().market?.symbol).toBe("BTC-USD");
    expect(handler.getState().log).toHaveLength(1);

    waitForReconnect();
    expect(sockets).toHaveLength(2);
    last().send(wire(snapshot({ seq: 77, events: [trade(9), trade(10)] })));
    const state = handler.getState();
    expect(state.status).toBe("live");
    expect(state.book?.seq).toBe(77);
    expect(state.log.map((e) => e.seq)).toEqual([10, 9]);
  });

  it("treats a malformed message as a reason to resync", () => {
    const { handler, last } = setup();
    handler.start();
    last().send(wire(snapshot()));
    last().send("not json");
    expect(handler.getState().book).toBeNull();
  });

  it("keeps saying connecting while the first attempt keeps failing", () => {
    const { handler, last } = setup();
    handler.start();
    last().drop();
    expect(handler.getState().status).toBe("connecting");
  });

  it("backs off from the floor up to the cap", () => {
    const { handler, last, waitForReconnect } = setup();
    handler.start();
    const delays: number[] = [];
    for (let i = 0; i < 9; i++) {
      last().drop();
      delays.push(waitForReconnect());
    }
    expect(delays).toEqual([0, 1, 2, 3, 4, 5, 6, 7, 8].map(delayFor));
    expect(delays[0]).toBe(BACKOFF_BASE_MS);
    expect(delays.at(-1)).toBe(
      Math.round((BACKOFF_BASE_MS + BACKOFF_CAP_MS) / 2),
    );
  });

  it("resets the backoff after an update, not after a snapshot alone", () => {
    const { handler, last, waitForReconnect } = setup();
    handler.start();
    last().drop();
    waitForReconnect();
    last().drop();
    waitForReconnect();

    // Snapshot, then a gap: the next delay keeps growing.
    last().send(wire(snapshot()));
    last().send(wire(update({ seq: 9 })));
    expect(waitForReconnect()).toBe(delayFor(2));

    // Snapshot, then a valid update: the next delay starts over.
    last().send(wire(snapshot()));
    last().send(wire(update()));
    last().drop();
    expect(waitForReconnect()).toBe(delayFor(0));
    expect(handler.getState().status).toBe("reconnecting");
  });

  it("ignores messages and closes from a socket it already dropped", () => {
    const { handler, sockets, last, waitForReconnect } = setup();
    handler.start();
    const stale = last();
    stale.send(wire(snapshot()));
    stale.send(wire(update({ seq: 9 })));
    waitForReconnect();
    last().send(wire(snapshot({ seq: 3 })));
    stale.send(wire(update({ seq: 4 })));
    stale.drop();
    expect(handler.getState().book?.seq).toBe(3);
    expect(sockets).toHaveLength(2);
  });

  it("closes the socket and cancels a pending reconnect on stop, and can start again", () => {
    const { handler, sockets, last } = setup();
    handler.start();
    const first = last();
    handler.stop();
    expect(first.closed).toBe(true);

    handler.start();
    last().drop();
    handler.stop();
    vi.advanceTimersByTime(BACKOFF_CAP_MS * 2);
    expect(sockets).toHaveLength(2);
    handler.start();
    expect(sockets).toHaveLength(3);
    expect(handler.getState()).toMatchObject({
      status: "connecting",
      market: null,
    });
  });

  describe("trading status", () => {
    it("is null before the snapshot, then the snapshot's", () => {
      const { handler, last } = setup();
      handler.start();
      expect(handler.getState().tradingStatus).toBeNull();
      last().send(wire(snapshot({ seq: 1, trading_status: "halted" })));
      expect(handler.getState().tradingStatus).toBe("halted");
    });

    it("follows status events, the newest in an update winning", () => {
      const { handler, last } = setup();
      handler.start();
      last().send(wire(snapshot({ seq: 1 })));
      last().send(
        wire(
          update({ seq: 2, events: [statusChanged(1, "halted"), accepted(2)] }),
        ),
      );
      expect(handler.getState().tradingStatus).toBe("halted");
      last().send(wire(update({ seq: 3, events: [accepted(3)] })));
      expect(handler.getState().tradingStatus).toBe("halted");
      last().send(
        wire(
          update({
            seq: 4,
            events: [statusChanged(4, "open"), statusChanged(4, "halted")],
          }),
        ),
      );
      expect(handler.getState().tradingStatus).toBe("halted");
      last().send(wire(update({ seq: 5, events: [statusChanged(5, "open")] })));
      expect(handler.getState().tradingStatus).toBe("open");
    });

    it("is replaced by a resync snapshot", () => {
      const { handler, last, waitForReconnect } = setup();
      handler.start();
      last().send(wire(snapshot({ seq: 1, trading_status: "halted" })));
      last().drop();
      waitForReconnect();
      last().send(wire(snapshot({ seq: 5, trading_status: "open" })));
      expect(handler.getState().tradingStatus).toBe("open");
    });
  });

  describe("last trade", () => {
    it("is null before any trade and follows the newest trade", () => {
      const { handler, last } = setup();
      handler.start();
      last().send(wire(snapshot({ seq: 1 })));
      expect(handler.getState().lastTrade).toBeNull();

      last().send(
        wire(update({ seq: 2, events: [trade(2, "buy", 1_000_010)] })),
      );
      expect(handler.getState().lastTrade).toEqual({
        price: 1_000_010,
        side: "buy",
      });
    });

    it("survives more later order events than the log keeps", () => {
      const { handler, last } = setup();
      handler.start();
      last().send(wire(snapshot({ seq: 1 })));
      last().send(
        wire(update({ seq: 2, events: [trade(2, "sell", 999_990)] })),
      );
      const flood = Array.from({ length: MAX_ORDER_ENTRIES + 1 }, (_, i) =>
        accepted(3 + i),
      );
      last().send(wire(update({ seq: 3, events: flood })));
      expect(handler.getState().log.some((e) => e.body.type === "trade")).toBe(
        true,
      );
      expect(handler.getState().lastTrade).toEqual({
        price: 999_990,
        side: "sell",
      });
    });

    it("stays through a reconnect until a newer trade arrives", () => {
      const { handler, last, waitForReconnect } = setup();
      handler.start();
      last().send(wire(snapshot({ seq: 1 })));
      last().send(
        wire(update({ seq: 2, events: [trade(2, "sell", 999_990)] })),
      );
      last().drop();
      waitForReconnect();
      last().send(wire(snapshot({ seq: 5 })));
      expect(handler.getState().lastTrade).toEqual({
        price: 999_990,
        side: "sell",
      });
    });
  });
});
