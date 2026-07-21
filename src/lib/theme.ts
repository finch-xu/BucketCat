export type ThemeMode = "light" | "dark" | "system";

const STORAGE_KEY = "bucketcat.theme-mode";
const media = window.matchMedia("(prefers-color-scheme: dark)");

export function getThemeMode(): ThemeMode {
  const stored = localStorage.getItem(STORAGE_KEY);
  return stored === "light" || stored === "dark" ? stored : "system";
}

export function resolvesDark(mode: ThemeMode): boolean {
  return mode === "dark" || (mode === "system" && media.matches);
}

export function applyThemeMode(mode: ThemeMode) {
  localStorage.setItem(STORAGE_KEY, mode);
  document.documentElement.classList.toggle("dark", resolvesDark(mode));
}

export function onSystemThemeChange(listener: () => void): () => void {
  media.addEventListener("change", listener);
  return () => media.removeEventListener("change", listener);
}

export function initTheme() {
  document.documentElement.classList.toggle("dark", resolvesDark(getThemeMode()));
  onSystemThemeChange(() => {
    if (getThemeMode() === "system") {
      document.documentElement.classList.toggle("dark", media.matches);
    }
  });
}
