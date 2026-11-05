// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

import { Check, Copy } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { cn } from "@/lib/utils";

/** Copies `text` to the clipboard and shows a check for a moment. An icon button; `label` is its accessible name. */
export function CopyButton({
  text,
  label = "Copy",
  className,
  size = 14,
}: {
  text: string;
  label?: string;
  className?: string;
  size?: number;
}) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard refused, as in some embedded browsers; the text stays selectable.
    }
  };
  return (
    <>
      <button
        type="button"
        onClick={copy}
        aria-label={copied ? "Copied" : label}
        title={copied ? "Copied" : label}
        className={cn("inline-flex shrink-0 cursor-pointer p-0.5 text-fg-3 hover:text-fg", className)}
      >
        {copied ? <Check size={size} aria-hidden="true" className="text-passed" /> : <Copy size={size} aria-hidden="true" />}
      </button>
      <span role="status" className="sr-only">
        {copied ? "Copied" : ""}
      </span>
    </>
  );
}
