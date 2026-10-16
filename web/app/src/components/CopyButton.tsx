// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

import { useState } from "react";

export function CopyButton({
  text,
  label = "Copy",
  className = "button button-small",
}: {
  text: string;
  label?: string;
  className?: string;
}) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard refused, as in some embedded browsers; the text stays
      // selectable.
    }
  };
  return (
    <button type="button" className={className} onClick={copy} aria-live="polite">
      {copied ? (label === label.toLowerCase() ? "copied" : "Copied") : label}
    </button>
  );
}
