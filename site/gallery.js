/* Screenshot viewer: click or tap a screenshot to see it large, move between the screenshots of
 * its group (arrows, keys or swipe), and zoom in to read the text. No dependencies.
 *
 * Which images: every image in a `.shots` row is one group; every `.step img` on a page is one
 * group. An image can name a sharper copy for the viewer with `data-full="/img/….jpg"`.
 */
(function () {
  'use strict';

  var groups = [];
  function collect() {
    [].forEach.call(document.querySelectorAll('.shots'), function (row) {
      var imgs = [].slice.call(row.querySelectorAll('img'));
      if (imgs.length) groups.push(imgs);
    });
    var steps = [].slice.call(document.querySelectorAll('.step img'));
    if (steps.length) groups.push(steps);
  }

  var box, stage, big, counter, caption, zoomBtn;
  var cur = { group: null, i: 0 }, opener = null, zoomed = false;

  function el(tag, cls, attrs) {
    var e = document.createElement(tag);
    if (cls) e.className = cls;
    if (attrs) Object.keys(attrs).forEach(function (k) { e.setAttribute(k, attrs[k]); });
    return e;
  }

  function build() {
    box = el('div', 'lb', { role: 'dialog', 'aria-modal': 'true', 'aria-label': 'Screenshot viewer', hidden: '' });
    var top = el('div', 'lb-bar');
    counter = el('span', 'lb-count');
    zoomBtn = el('button', 'lb-btn', { type: 'button', 'aria-label': 'Zoom in' });
    zoomBtn.textContent = 'Zoom';
    var close = el('button', 'lb-btn', { type: 'button', 'aria-label': 'Close' });
    close.textContent = 'Close';
    var right = el('span', 'lb-right');
    right.appendChild(zoomBtn);
    right.appendChild(close);
    top.appendChild(counter);
    top.appendChild(right);

    stage = el('div', 'lb-stage');
    big = el('img', 'lb-img', { alt: '' });
    stage.appendChild(big);

    var bottom = el('div', 'lb-bar lb-bottom');
    var prev = el('button', 'lb-btn lb-prev', { type: 'button', 'aria-label': 'Previous screenshot' });
    prev.textContent = '← Previous';
    var next = el('button', 'lb-btn lb-next', { type: 'button', 'aria-label': 'Next screenshot' });
    next.textContent = 'Next →';
    caption = el('p', 'lb-cap');
    bottom.appendChild(prev);
    bottom.appendChild(caption);
    bottom.appendChild(next);

    box.appendChild(top);
    box.appendChild(stage);
    box.appendChild(bottom);
    document.body.appendChild(box);

    close.addEventListener('click', hide);
    prev.addEventListener('click', function () { go(-1); });
    next.addEventListener('click', function () { go(1); });
    zoomBtn.addEventListener('click', function () { setZoom(!zoomed); });
    big.addEventListener('click', function () { setZoom(!zoomed); });
    // A click on the dark area around the picture closes it (not while zoomed and panning).
    stage.addEventListener('click', function (e) { if (e.target === stage && !zoomed) hide(); });

    var x0 = null, y0 = null;
    stage.addEventListener('touchstart', function (e) {
      if (zoomed || e.touches.length !== 1) { x0 = null; return; }
      x0 = e.touches[0].clientX; y0 = e.touches[0].clientY;
    }, { passive: true });
    stage.addEventListener('touchend', function (e) {
      if (x0 === null || zoomed) return;
      var dx = e.changedTouches[0].clientX - x0, dy = e.changedTouches[0].clientY - y0;
      x0 = null;
      if (Math.abs(dx) > 50 && Math.abs(dx) > Math.abs(dy) * 1.5) go(dx < 0 ? 1 : -1);
    }, { passive: true });

    document.addEventListener('keydown', function (e) {
      if (box.hidden) return;
      if (e.key === 'Escape') { hide(); e.preventDefault(); }
      else if (e.key === 'ArrowLeft') { go(-1); e.preventDefault(); }
      else if (e.key === 'ArrowRight') { go(1); e.preventDefault(); }
      else if (e.key === '+' || e.key === '=') { setZoom(true); e.preventDefault(); }
      else if (e.key === '-') { setZoom(false); e.preventDefault(); }
      else if (e.key === 'Tab') {
        // keep the focus inside the viewer
        var f = [].slice.call(box.querySelectorAll('button'));
        var first = f[0], last = f[f.length - 1];
        if (e.shiftKey && document.activeElement === first) { last.focus(); e.preventDefault(); }
        else if (!e.shiftKey && document.activeElement === last) { first.focus(); e.preventDefault(); }
      }
    });
  }

  function src(img) { return img.getAttribute('data-full') || img.currentSrc || img.src; }

  function setZoom(on) {
    zoomed = on;
    box.classList.toggle('zoomed', on);
    zoomBtn.textContent = on ? 'Fit' : 'Zoom';
    zoomBtn.setAttribute('aria-label', on ? 'Fit to screen' : 'Zoom in');
    if (on) {
      // At least 760 px wide (more if the picture is sharper), so the text is readable.
      var w = Math.min(1400, Math.max(760, big.naturalWidth || 0));
      big.style.width = w + 'px';
      stage.scrollTop = 0;
      stage.scrollLeft = Math.max(0, (w - stage.clientWidth) / 2);
    } else {
      big.style.width = '';
    }
  }

  function show(group, i) {
    cur.group = group; cur.i = (i + group.length) % group.length;
    var img = group[cur.i];
    setZoom(false);
    big.src = src(img);
    big.alt = img.alt || '';
    counter.textContent = (cur.i + 1) + ' / ' + group.length;
    caption.textContent = img.alt || '';
    box.classList.toggle('single', group.length < 2);
    // load the neighbours so moving on is instant
    [1, -1].forEach(function (d) {
      var n = group[(cur.i + d + group.length) % group.length];
      if (n && n !== img) new Image().src = src(n);
    });
  }

  function open(group, i, from) {
    if (!box) build();
    opener = from || null;
    show(group, i);
    box.hidden = false;
    document.documentElement.classList.add('lb-open');
    box.querySelector('.lb-bar button:last-child').focus();
  }

  function hide() {
    if (!box || box.hidden) return;
    box.hidden = true;
    document.documentElement.classList.remove('lb-open');
    big.removeAttribute('src');
    if (opener && opener.focus) opener.focus();
  }

  function go(d) { if (cur.group) show(cur.group, cur.i + d); }

  function init() {
    collect();
    groups.forEach(function (group) {
      group.forEach(function (img, i) {
        img.setAttribute('role', 'button');
        img.setAttribute('tabindex', '0');
        img.setAttribute('aria-label', 'View larger: ' + (img.alt || 'screenshot'));
        img.addEventListener('click', function () { open(group, i, img); });
        img.addEventListener('keydown', function (e) {
          if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); open(group, i, img); }
        });
      });
    });
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
  else init();
})();
