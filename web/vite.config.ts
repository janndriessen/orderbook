import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/ws": { target: "http://127.0.0.1:8080", ws: true },
    },
  },
  test: {
    environment: "jsdom",
    unstubGlobals: true,
    restoreMocks: true,
    setupFiles: ["./src/test/setup.ts"],
  },
});
