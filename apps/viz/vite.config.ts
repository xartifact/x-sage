import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";

/** Where the Hono API listens during development. */
const API_TARGET = process.env.X_SAGE_API ?? "http://127.0.0.1:3001";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@x-sage/core": resolve(__dirname, "../../packages/core/src/index.ts"),
    },
  },
  server: {
    port: 5173,
    // The browser only ever talks to one origin: Vite forwards /api to the
    // server, so no CORS preflight and no hard-coded port in client code.
    proxy: {
      "/api": {
        target: API_TARGET,
        changeOrigin: true,
      },
    },
  },
});
