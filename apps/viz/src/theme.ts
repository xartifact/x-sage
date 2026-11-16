import { useEffect, useState } from "react";
export const PALETTES = {
  iris: "鸢尾紫",
  ocean: "海洋蓝",
  jade: "松石绿",
  clay: "陶土橙",
} as const;
export type Palette = keyof typeof PALETTES;
export type Appearance = "system" | "light" | "dark";
export function validPalette(value: unknown): Palette {
  return typeof value === "string" && Object.hasOwn(PALETTES, value)
    ? (value as Palette)
    : "iris";
}
export function validAppearance(value: unknown): Appearance {
  return value === "light" || value === "dark" ? value : "system";
}
function saved(key: string) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
export function applyTheme(palette: Palette, appearance: Appearance) {
  document.documentElement.dataset.xartifactPalette = palette;
  const dark =
    appearance === "dark" ||
    (appearance === "system" &&
      matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
  document.documentElement.style.colorScheme = dark ? "dark" : "light";
}
export function initializeTheme() {
  applyTheme(
    validPalette(saved("xartifact.palette")),
    validAppearance(saved("xartifact.appearance")),
  );
}
export function useTheme() {
  const [palette, setPalette] = useState<Palette>(() =>
    validPalette(saved("xartifact.palette")),
  );
  const [appearance, setAppearance] = useState<Appearance>(() =>
    validAppearance(saved("xartifact.appearance")),
  );
  useEffect(() => {
    applyTheme(palette, appearance);
    try {
      localStorage.setItem("xartifact.palette", palette);
      localStorage.setItem("xartifact.appearance", appearance);
    } catch {}
    const media = matchMedia("(prefers-color-scheme: dark)");
    const change = () => applyTheme(palette, appearance);
    media.addEventListener("change", change);
    return () => media.removeEventListener("change", change);
  }, [palette, appearance]);
  useEffect(() => {
    const sync = (e: StorageEvent) => {
      if (e.key === null) {
        setPalette("iris");
        setAppearance("system");
        return;
      }
      if (e.key === "xartifact.palette") setPalette(validPalette(e.newValue));
      if (e.key === "xartifact.appearance")
        setAppearance(validAppearance(e.newValue));
    };
    window.addEventListener("storage", sync);
    return () => window.removeEventListener("storage", sync);
  }, []);
  return { palette, setPalette, appearance, setAppearance };
}
