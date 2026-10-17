import { vi } from "vitest";

/**
 * jsdom does no layout and loads no stylesheets, so tests provide what the
 * order book reads from the browser: an element height and the size tokens.
 */
export function stubViewport(height: number) {
  const root = document.documentElement.style;
  root.setProperty("--row-height", "20px");
  root.setProperty("--header-height", "20px");
  root.setProperty("--spread-height", "20px");

  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockReturnValue(
    height,
  );
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
}
