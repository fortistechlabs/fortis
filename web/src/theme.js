// Theme (light/dark/system) — persisted separately from wallet state, like
// locale, so it can be read synchronously before any async load. Defaults to
// dark until the user picks something else (Settings -> Theme), regardless
// of the OS preference — "System default" is an explicit opt-in, not the
// initial state. Applied via a `data-theme` attribute that the CSS keys off
// directly: every color in style.css is a custom property, so flipping the
// attribute re-themes the whole app through the cascade alone — no
// re-render needed. index.html also sets this attribute inline, before the
// stylesheet loads, so there's no flash of the wrong theme on boot; this
// module re-applies it (harmless, idempotent) and keeps it in sync if the OS
// theme changes while "system" is selected.

const LS_KEY = 'fortis_theme';

function systemPrefersDark() {
  return matchMedia('(prefers-color-scheme: dark)').matches;
}

export function storedTheme() {
  try {
    return localStorage.getItem(LS_KEY) || 'dark';
  } catch {
    return 'dark';
  }
}

function apply(theme) {
  const effective = theme === 'system' ? (systemPrefersDark() ? 'dark' : 'light') : theme;
  document.documentElement.setAttribute('data-theme', effective);
}

export function setTheme(theme) {
  try {
    localStorage.setItem(LS_KEY, theme);
  } catch {
    /* private browsing / storage disabled — theme just won't persist */
  }
  apply(theme);
}

export function initThemeWatcher() {
  apply(storedTheme());
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
    if (storedTheme() === 'system') apply('system');
  });
}
