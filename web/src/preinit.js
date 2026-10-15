// Set before the stylesheet loads. The app is dark only (see src/theme.js);
// this stays a real file rather than an inline <script> so a strict CSP
// (script-src 'self', no 'unsafe-inline') covers it with no special-casing.
document.documentElement.setAttribute('data-theme', 'dark');
