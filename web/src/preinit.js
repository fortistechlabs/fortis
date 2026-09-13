// Set before the stylesheet loads so there's no flash of the wrong theme —
// style.css keys every color off this attribute. Defaults to dark until the
// user opts into "System default" or "Light" (Settings -> Theme), rather
// than following the OS preference from the start. Kept in sync afterward
// by src/theme.js (this is just the pre-paint half of the same logic).
//
// A real file rather than an inline <script> specifically so a strict CSP
// (script-src 'self', no 'unsafe-inline') covers it with no special-casing —
// an inline script would otherwise need a content hash kept in sync by hand
// on every edit, in a project with no build step to automate that.
(function () {
  try {
    var stored = localStorage.getItem('fortis_theme') || 'dark';
    var dark = matchMedia('(prefers-color-scheme: dark)').matches;
    document.documentElement.setAttribute('data-theme', stored === 'system' ? (dark ? 'dark' : 'light') : stored);
  } catch (e) { /* private browsing / storage disabled — CSS default (dark) applies */ }
})();
