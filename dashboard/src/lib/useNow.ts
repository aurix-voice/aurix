import { useEffect, useState } from "react";

/** Current time, refreshed every `intervalMs`; keeps relative timestamps ticking without impure reads during render. */
export function useNow(intervalMs = 10_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(id);
  }, [intervalMs]);
  return now;
}
