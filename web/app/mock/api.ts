// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

// A stand-in for the Worker's /api under `npm run dev:mock`, holding
// its state in memory, so screens can be worked on without GitHub,
// Polar or AWS. It answers the same shapes as crates/cloud/src/app.rs.

import type { IncomingMessage, ServerResponse } from "node:http";
import type { Plugin } from "vite";

interface State {
  signedIn: boolean;
}

const state: State = { signedIn: false };

const user = {
  id: 1,
  login: "octocat",
  name: "Mona Octocat",
  avatarUrl:
    "data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 2 2'><rect width='2' height='2' fill='%23f5a524'/></svg>",
};

type Handler = (req: IncomingMessage, url: URL, body: unknown) => [number, unknown] | string;

const routes: Record<string, Handler> = {
  "GET /api/auth/login": (_req, url) => {
    state.signedIn = true;
    return url.searchParams.get("return_to") ?? "/";
  },
  "POST /api/auth/logout": () => {
    state.signedIn = false;
    return [204, null];
  },
  "GET /api/session": () =>
    state.signedIn ? [200, { user, csrfToken: "mock" }] : [401, { error: "not signed in" }],
};

async function readBody(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(chunk as Buffer);
  }
  const text = Buffer.concat(chunks).toString();
  return text ? JSON.parse(text) : undefined;
}

function answer(res: ServerResponse, status: number, body: unknown) {
  res.statusCode = status;
  if (status === 204) {
    res.end();
    return;
  }
  res.setHeader("Content-Type", "application/json");
  res.end(JSON.stringify(body));
}

export function mockApi(): Plugin {
  return {
    name: "hatchrun-mock-api",
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        const url = new URL(req.url ?? "/", "http://localhost");
        if (!url.pathname.startsWith("/api/")) {
          next();
          return;
        }
        const key = `${req.method} ${url.pathname}`;
        const handler =
          routes[key] ??
          Object.entries(routes).find(([pattern]) => {
            const regex = new RegExp(`^${pattern.replace(/:[a-z]+/g, "[^/]+")}$`);
            return regex.test(key);
          })?.[1];
        if (!handler) {
          answer(res, 404, { error: "not found" });
          return;
        }
        const result = handler(req, url, await readBody(req));
        if (typeof result === "string") {
          res.statusCode = 302;
          res.setHeader("Location", result);
          res.end();
          return;
        }
        answer(res, result[0], result[1]);
      });
    },
  };
}
