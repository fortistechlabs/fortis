#!/usr/bin/env python3
"""Convert Android string resources into the web app's JSON locale files.

Reuses the Android translations directly (same keys, same %1$s/%1$d
placeholder syntax) instead of re-translating from scratch. Run manually
whenever android/app/src/main/res/values*/strings.xml changes:

    python web/tools/i18n_extract.py

Web-only keys (added straight to web/src/locales/en.json — no Android
equivalent, e.g. the backend-picker screen) are left untouched; this script
only ever writes keys that exist in Android's base values/strings.xml.
Web's en.json is otherwise treated as authoritative for the exact English
text of those shared keys, updated here from Android's source on every run.
"""
import json
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent  # repo root
RES = ROOT / "android" / "app" / "src" / "main" / "res"
OUT = Path(__file__).resolve().parent.parent / "src" / "locales"

# BCP47 tag -> Android res "values-<qualifier>" suffix, wherever it isn't a
# plain 1:1 match. Keep in sync with
# android/app/src/main/kotlin/com/fortis/wallet/ui/Locales.kt's
# SUPPORTED_LOCALES and res/xml/locales_config.xml.
DIR_OVERRIDES = {
    "en-GB": "en-rGB",
    "es-US": "es-rUS",
    "fr-CA": "fr-rCA",
    "pt-BR": "pt-rBR",
    "pt-PT": "pt-rPT",
    "zh-CN": "zh-rCN",
    "zh-HK": "zh-rHK",
    "zh-TW": "zh-rTW",
    "id": "in",  # Android still uses the legacy ISO code for Indonesian
    "he": "iw",  # ...and for Hebrew
}

SUPPORTED_LOCALES = [
    "af", "am", "ar", "az", "be", "bg", "bn", "ca", "cs", "da", "de", "el", "en-GB",
    "es", "es-US", "et", "eu", "fa", "fi", "fil", "fr", "fr-CA", "gl", "gu", "he", "hi",
    "hr", "hu", "hy", "id", "is", "it", "ja", "ka", "kk", "km", "kn", "ko", "ky", "lo",
    "lt", "lv", "mk", "ml", "mn", "mr", "ms", "my", "nb", "ne", "nl", "pa", "pl", "pt-BR",
    "pt-PT", "ro", "ru", "si", "sk", "sl", "sq", "sr", "sv", "sw", "ta", "te", "th", "tr",
    "uk", "ur", "vi", "zh-CN", "zh-HK", "zh-TW", "zu",
]

# Android string resources escape these three; XML entities (&amp; etc.) are
# already resolved by ElementTree during parsing.
UNESCAPE = (("\\'", "'"), ('\\"', '"'), ("\\n", "\n"))


def unescape(s: str) -> str:
    for a, b in UNESCAPE:
        s = s.replace(a, b)
    return s


def parse_strings_xml(path: Path) -> dict:
    """Flat {key: str}, with <plurals> flattened to {key: {category: str}}
    in the same namespace — one lookup shape for the JS runtime either way."""
    if not path.exists():
        return {}
    root = ET.parse(path).getroot()
    out = {}
    for node in root:
        name = node.get("name")
        if node.tag == "string":
            out[name] = unescape("".join(node.itertext()).strip())
        elif node.tag == "plurals":
            out[name] = {item.get("quantity"): unescape("".join(item.itertext()).strip()) for item in node}
    return out


def dir_for(tag: str) -> str:
    return DIR_OVERRIDES.get(tag, tag)


def main():
    base = parse_strings_xml(RES / "values" / "strings.xml")
    if not base:
        raise SystemExit(f"no strings found at {RES / 'values' / 'strings.xml'}")
    OUT.mkdir(parents=True, exist_ok=True)

    en_path = OUT / "en.json"
    en = json.loads(en_path.read_text(encoding="utf-8")) if en_path.exists() else {}
    web_only = {k: v for k, v in en.items() if k not in base}
    en = {**web_only, **base}
    en_path.write_text(json.dumps(en, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"en.json: {len(en)} keys ({len(base)} from Android, {len(web_only)} web-only)")

    total = 0
    for tag in SUPPORTED_LOCALES:
        d = dir_for(tag)
        translated = parse_strings_xml(RES / f"values-{d}" / "strings.xml")
        # Only keys Android actually translates for this locale — no
        # pre-merged English fill-in (the runtime loader falls back per key,
        # and pre-merging would make future re-syncs noisier to diff).
        translated = {k: v for k, v in translated.items() if k in base}
        (OUT / f"{tag}.json").write_text(
            json.dumps(translated, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        total += len(translated)
        missing = len(base) - len(translated)
        print(f"{tag}.json (values-{d}): {len(translated)} keys, {missing} fall back to English")

    print(f"\n{len(SUPPORTED_LOCALES)} locales written, {total} translated strings total.")


if __name__ == "__main__":
    main()
