// Locale strings. Mirrors the Android app's res/values*/strings.xml — same keys,
// same %1$s/%1$d placeholder syntax — so translations can be converted straight
// across (see web/tools/i18n_extract.py). Fiat stays USD-only regardless of
// locale; only strings and number/date formatting localize.

const LS_KEY = 'fortis_locale';

/** BCP47 tags with a translation file under ./locales/. "en" is the base. */
export const SUPPORTED_LOCALES = [
  'en', 'af', 'am', 'ar', 'az', 'be', 'bg', 'bn', 'ca', 'cs', 'da', 'de', 'el', 'en-GB',
  'es', 'es-US', 'et', 'eu', 'fa', 'fi', 'fil', 'fr', 'fr-CA', 'gl', 'gu', 'he', 'hi',
  'hr', 'hu', 'hy', 'id', 'is', 'it', 'ja', 'ka', 'kk', 'km', 'kn', 'ko', 'ky', 'lo',
  'lt', 'lv', 'mk', 'ml', 'mn', 'mr', 'ms', 'my', 'nb', 'ne', 'nl', 'pa', 'pl', 'pt-BR',
  'pt-PT', 'ro', 'ru', 'si', 'sk', 'sl', 'sq', 'sr', 'sv', 'sw', 'ta', 'te', 'th', 'tr',
  'uk', 'ur', 'vi', 'zh-CN', 'zh-HK', 'zh-TW', 'zu',
];

const cache = new Map(); // tag -> dict (flat strings + nested plural objects)
let en = null;
let active = null;
let activeTag = 'en';

async function fetchLocale(tag) {
  if (cache.has(tag)) return cache.get(tag);
  const dict = await fetch(`./src/locales/${tag}.json`)
    .then((r) => (r.ok ? r.json() : null))
    .catch(() => null);
  if (dict) cache.set(tag, dict);
  return dict;
}

/** Best match among `navigator.languages` against SUPPORTED_LOCALES: exact tag
 *  first, then bare language (e.g. "pt" -> "pt-BR"). */
function pickSystemLocale() {
  const prefs = navigator.languages && navigator.languages.length ? navigator.languages : [navigator.language];
  for (const p of prefs) {
    if (SUPPORTED_LOCALES.includes(p)) return p;
  }
  for (const p of prefs) {
    const lang = p.split('-')[0];
    const hit = SUPPORTED_LOCALES.find((t) => t.split('-')[0] === lang);
    if (hit) return hit;
  }
  return 'en';
}

/** The explicit override, or null when following the device (localStorage,
 *  same rationale as Android's separate `fortis_locale` SharedPreferences file:
 *  must be readable synchronously, before the async wallet state loads, so
 *  onboarding already renders in the right language). */
export function overrideTag() {
  try {
    return localStorage.getItem(LS_KEY);
  } catch {
    return null;
  }
}

export function resolvedTag() {
  const o = overrideTag();
  return o && SUPPORTED_LOCALES.includes(o) ? o : pickSystemLocale();
}

/** Load `en` (always, as the fallback target) plus the resolved locale. Call
 *  once at boot, before the first render. */
export async function init() {
  en = (await fetchLocale('en')) || {};
  activeTag = resolvedTag();
  active = activeTag === 'en' ? en : (await fetchLocale(activeTag)) || en;
  return activeTag;
}

/** `tag` is a BCP47 string, or null to follow the device. */
export async function setLocale(tag) {
  try {
    if (tag) localStorage.setItem(LS_KEY, tag);
    else localStorage.removeItem(LS_KEY);
  } catch {
    /* private browsing / storage disabled — locale just won't persist */
  }
  activeTag = resolvedTag();
  active = activeTag === 'en' ? en : (await fetchLocale(activeTag)) || en;
}

export function currentTag() {
  return activeTag;
}

/** The language's own name for itself, region in parens when the tag carries
 *  one (e.g. "Deutsch", "Português (Brasil)") — derived at runtime, no
 *  hand-maintained label table, mirroring Android's `localeLabel()`. */
export function localeLabel(tag) {
  try {
    const loc = new Intl.Locale(tag);
    const lang = new Intl.DisplayNames([tag], { type: 'language' }).of(loc.language);
    const name = lang ? lang.charAt(0).toUpperCase() + lang.slice(1) : tag;
    if (loc.region) {
      const region = new Intl.DisplayNames([tag], { type: 'region' }).of(loc.region);
      if (region) return `${name} (${region})`;
    }
    return name;
  } catch {
    return tag;
  }
}

function subst(str, args) {
  return str.replace(/%%|%(\d+)\$[sd]/g, (m, n) => {
    if (m === '%%') return '%';
    const v = args[Number(n) - 1];
    return v == null ? m : String(v);
  });
}

/** Look up `key` in the active locale, falling back to English per-key (a
 *  locale need not be translation-complete — same behaviour as Android's
 *  resource fallback), then to a loud placeholder if even English lacks it
 *  (JS has no compile-time guarantee the base key exists). */
export function t(key, ...args) {
  const val = active?.[key] ?? en?.[key];
  if (typeof val !== 'string') return `[[${key}]]`;
  return args.length ? subst(val, args) : val;
}

/** `key` must resolve to a `{one, other, ...}` object (Android `<plurals>`).
 *  `count` selects the CLDR category via Intl.PluralRules, falling back to
 *  "other" when the active table lacks that category — the same fallback
 *  Android's own plural-resource resolution performs for an unlisted
 *  category, not an approximation of it. */
export function tPlural(key, count, ...args) {
  const table = active?.[key] ?? en?.[key];
  if (typeof table !== 'object' || !table) return `[[${key}]]`;
  const cat = new Intl.PluralRules(activeTag).select(count);
  const val = table[cat] ?? table.other;
  if (typeof val !== 'string') return `[[${key}]]`;
  return subst(val, [count, ...args]);
}
