import { useEffect, useState } from "react";

/** Trailing-edge debounce: returns `value` only after it has been stable
 * for `delayMs`. Used by the search box so each keystroke doesn't fire a
 * ListObjectsV2 request. */
export function useDebounced<T>(value: T, delayMs = 300): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setDebounced(value), delayMs);
    return () => clearTimeout(timer);
  }, [value, delayMs]);
  return debounced;
}
