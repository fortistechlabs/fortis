// The app is dark only, like the Android app (DESIGN.md: "Graphite signal").
// The Light / System theme setting is gone; a leftover choice from it is
// cleared so nothing reads it again.

const LS_KEY = 'fortis_theme';

export function initTheme() {
  document.documentElement.setAttribute('data-theme', 'dark');
  try {
    localStorage.removeItem(LS_KEY);
  } catch {
    /* private browsing / storage disabled — nothing stored to clear */
  }
}
