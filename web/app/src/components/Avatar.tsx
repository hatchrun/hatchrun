// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { useState } from "react";

/** The user's avatar image; the initial of `name` when there is none or it fails to load. */
export function Avatar({ src, name, className }: { src: string | null | undefined; name: string; className?: string }) {
  const [failed, setFailed] = useState(false);
  if (src && !failed) {
    return <img src={src} alt="" onError={() => setFailed(true)} className={className ?? "size-full rounded-[inherit]"} />;
  }
  return <>{(name.trim().charAt(0) || "?").toUpperCase()}</>;
}
