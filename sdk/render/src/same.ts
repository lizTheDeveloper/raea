// Structural equality for props, for React.memo's compare on leaf components (a material, a shape) and, with
// elements, on Mesh3d and Entity. A frame-pure composition makes every prop again each frame: an inline [x, y, z], a
// colour from Srgba.hex() or a glow { red, green, blue } with the same numbers. Shallow memo calls those changes, and
// React re-renders thousands of unchanged parts a frame (about 110-160 µs each on QuickJS).

/** Primitives by value (NaN equals NaN), arrays and plain or class objects by their own keys; React elements,
 * functions and anything past `depth` by identity. */
export function sameValue(a: unknown, b: unknown, depth = 6): boolean {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null || depth <= 0) return false;
  if ((a as { $$typeof?: unknown }).$$typeof !== undefined || (b as { $$typeof?: unknown }).$$typeof !== undefined) return false;
  if (Array.isArray(a)) {
    if (!Array.isArray(b) || a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (!sameValue(a[i], b[i], depth - 1)) return false;
    return true;
  }
  if (Array.isArray(b) || Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
  const ka = Object.keys(a as object), kb = Object.keys(b as object);
  if (ka.length !== kb.length) return false;
  for (const k of ka) if (!sameValue((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k], depth - 1)) return false;
  return true;
}

/** React.memo's compare for a leaf: every prop structurally equal. */
export function sameProps(a: Record<string, unknown>, b: Record<string, unknown>): boolean {
  const ka = Object.keys(a), kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  for (const k of ka) if (!sameValue(a[k], b[k])) return false;
  return true;
}

/** React.memo's compare for a part with children (Mesh3d, Entity): as sameProps, with React elements compared by
 * type, key and props, looking at no more than `budget` values in all (a big subtree that differs late costs no more
 * than rendering it). A subtree whose elements are made again each frame with the same props is no change. */
export function samePropsDeep(a: Record<string, unknown>, b: Record<string, unknown>, budget = 4000): boolean {
  const left = { n: budget };
  const ka = Object.keys(a), kb = Object.keys(b);
  if (ka.length !== kb.length) return false;
  for (const k of ka) if (!sameTree(a[k], b[k], 12, left)) return false;
  return true;
}

function sameTree(a: unknown, b: unknown, depth: number, left: { n: number }): boolean {
  if (Object.is(a, b)) return true;
  if (--left.n < 0 || typeof a !== "object" || typeof b !== "object" || a === null || b === null || depth <= 0) return false;
  const ea = a as { $$typeof?: unknown; type?: unknown; key?: unknown; ref?: unknown; props?: unknown }, eb = b as typeof ea;
  if (ea.$$typeof !== undefined || eb.$$typeof !== undefined) {
    return ea.$$typeof === eb.$$typeof && ea.type === eb.type && ea.key === eb.key && ea.ref === eb.ref && sameTree(ea.props, eb.props, depth - 1, left);
  }
  if (Array.isArray(a)) {
    if (!Array.isArray(b) || a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (!sameTree(a[i], b[i], depth - 1, left)) return false;
    return true;
  }
  if (Array.isArray(b) || Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
  const ka = Object.keys(a as object), kb = Object.keys(b as object);
  if (ka.length !== kb.length) return false;
  for (const k of ka) if (!sameTree((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k], depth - 1, left)) return false;
  return true;
}

/** Two places' numbers equal (a Vec3, a quaternion, or a scale that may be one number). */
export function sameNumbers(a: number | readonly number[] | undefined, b: number | readonly number[] | undefined): boolean {
  if (a === b) return true;
  if (typeof a === "number" || typeof b === "number" || !a || !b || a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}
