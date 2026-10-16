// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { fileURLToPath } from "node:url";

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

import { mockApi } from "./mock/api.ts";

// `npm run dev` sends /api to `wrangler dev` on port 8787. `npm run
// dev:mock` answers /api from mock/api.ts instead, to work on screens
// without GitHub.
export default defineConfig(({ mode }) => ({
  plugins: [react(), tailwindcss(), ...(mode === "mock" ? [mockApi()] : [])],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  server: {
    proxy: mode === "mock" ? undefined : { "/api": "http://127.0.0.1:8787" },
  },
  build: {
    sourcemap: true,
  },
}));
