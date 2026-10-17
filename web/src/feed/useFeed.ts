import { useEffect, useState, useSyncExternalStore } from "react";
import { FeedHandler, type FeedState } from "./FeedHandler";

function feedUrl(): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws`;
}

/** Live feed state for this page; one connection per mounted caller. */
export function useFeed(): FeedState {
  const [handler] = useState(() => new FeedHandler({ url: feedUrl() }));
  useEffect(() => {
    handler.start();
    return () => handler.stop();
  }, [handler]);
  return useSyncExternalStore(handler.subscribe, handler.getState);
}
