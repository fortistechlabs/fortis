/* Keeps "Latest: vX" on the home page equal to version.json, so it cannot go stale. */
(function () {
  'use strict';
  var el = document.getElementById('latest-version');
  if (!el || !window.fetch) return;
  fetch('/version.json', { cache: 'no-cache' })
    .then(function (r) { return r.ok ? r.json() : null; })
    .then(function (v) { if (v && /^[0-9][0-9.]*$/.test(v.versionName)) el.textContent = 'v' + v.versionName; })
    .catch(function () {});
})();
