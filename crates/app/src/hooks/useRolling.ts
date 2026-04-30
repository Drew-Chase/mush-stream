import { useEffect, useState } from "react";

export function useRolling(seedFn: () => number, ms = 700, len = 50): number[] {
  const [d, setD] = useState<number[]>(() =>
    Array.from({ length: len }, () => seedFn()),
  );
  useEffect(() => {
    const id = setInterval(
      () => setD((p) => [...p.slice(1), seedFn()]),
      ms,
    );
    return () => clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return d;
}
