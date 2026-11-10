#!/usr/bin/env python3
"""Generate and check the English edition of the static site.

Source:  site/<page>.html          (Chinese, hand-written)
Map:     site/i18n/en-<page>.json  (Chinese string -> English string)
Output:  site/en/<page>.html

The generator is strict: if the Chinese page contains a visible string that has
no entry in the map, it reports the missing strings and exits non-zero instead
of writing a half-translated page.  Use --check to validate without writing.

Usage:
    python site/tools/translate.py --all
    python site/tools/translate.py --all --check
    python site/tools/translate.py memory pipeline
"""

import argparse
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SITE = os.path.dirname(HERE)
I18N = os.path.join(SITE, "i18n")
EN_DIR = os.path.join(SITE, "en")

PAGES = [
    "index",
    "architecture",
    "companion",
    "engineering",
    "evidence",
    "memory",
    "pipeline",
    "reliability",
    "solo",
]

CJK = re.compile(r"[\u4e00-\u9fff]")
# Strings that are allowed to stay Chinese in the English edition: the language
# switch itself points back to the Chinese page.
ALLOWED = {"中文", "切换到中文"}
NOISE = [
    re.compile(r"<!--[\s\S]*?-->"),
    re.compile(r"<script[\s\S]*?</script>"),
    re.compile(r"<style[\s\S]*?</style>"),
]


def norm(s):
    return re.sub(r"\s+", " ", s).strip()


def strip_noise(html):
    for pat in NOISE:
        html = pat.sub("", html)
    return html


def collect_keys(html):
    """Visible Chinese text nodes and attribute values, normalised and de-duplicated."""
    clean = strip_noise(html)
    keys = []
    for m in re.finditer(r">([^<>]*)<", clean):
        t = norm(m.group(1))
        if t and CJK.search(t):
            keys.append(t)
    for m in re.finditer(
        r'(aria-label|alt|title|content|placeholder|data-[a-zA-Z-]+)="([^"]*)"', clean
    ):
        t = norm(m.group(2))
        if t and CJK.search(t):
            keys.append(t)
    seen = set()
    out = []
    for k in keys:
        if k not in seen:
            seen.add(k)
            out.append(k)
    return out


def load_map(page):
    path = os.path.join(I18N, f"en-{page}.json")
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as f:
        raw = json.load(f)
    return {norm(k): v for k, v in raw.items()}


def missing_keys(page, html, mapping):
    return [k for k in collect_keys(html) if k not in mapping]


def render(page, html, mapping):
    s = html.replace('<html lang="zh-CN">', '<html lang="en">', 1)
    s = s.replace('href="assets/', 'href="../assets/')
    s = s.replace('src="assets/img/', 'src="../assets/img/')
    s = s.replace('src="assets/', 'src="../assets/en/')
    s = s.replace(
        f'<a class="nav__lang" href="en/{page}.html" aria-label="Switch to English" hreflang="en">EN</a>',
        f'<a class="nav__lang" href="../{page}.html" aria-label="切换到中文" hreflang="zh-CN">中文</a>',
        1,
    )

    def attr_repl(m):
        key = norm(m.group(2))
        if key in mapping:
            return m.group(1) + '="' + mapping[key] + '"'
        return m.group(0)

    s = re.sub(
        r'(aria-label|alt|title|content|placeholder|data-[a-zA-Z-]+)="([^"]*)"',
        attr_repl,
        s,
    )

    def text_repl(m):
        raw = m.group(1)
        key = norm(raw)
        if key in mapping:
            lead = raw[: len(raw) - len(raw.lstrip())]
            trail = raw[len(raw.rstrip()) :]
            return ">" + lead + mapping[key] + trail + "<"
        return m.group(0)

    return re.sub(r">([^<>]*)<", text_repl, s)


def residual_cjk(html):
    clean = strip_noise(html)
    found = []
    for m in re.finditer(r">([^<>]*)<", clean):
        t = norm(m.group(1))
        if t and CJK.search(t) and t not in ALLOWED:
            found.append(t)
    for m in re.finditer(
        r'(aria-label|alt|title|content|placeholder|data-[a-zA-Z-]+)="([^"]*)"', clean
    ):
        t = norm(m.group(2))
        if t and CJK.search(t) and t not in ALLOWED:
            found.append(t)
    return found


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("pages", nargs="*", help="page names, e.g. index memory")
    ap.add_argument("--all", action="store_true", help="process every page")
    ap.add_argument("--check", action="store_true", help="validate only, do not write")
    args = ap.parse_args()

    targets = PAGES if args.all else args.pages
    if not targets:
        ap.print_help()
        return 2

    failed = False
    for page in targets:
        src_path = os.path.join(SITE, page + ".html")
        if not os.path.exists(src_path):
            print(f"[{page}] source missing: {src_path}")
            failed = True
            continue
        with open(src_path, encoding="utf-8", newline="") as f:
            html = f.read()
        mapping = load_map(page)
        if mapping is None:
            print(f"[{page}] map missing: {os.path.join(I18N, 'en-' + page + '.json')}")
            failed = True
            continue

        gaps = missing_keys(page, html, mapping)
        if gaps:
            failed = True
            print(f"[{page}] {len(gaps)} untranslated string(s):")
            for g in gaps:
                print("   -", g[:160])

        if args.check or gaps:
            print(f"[{page}] checked, {len(collect_keys(html))} source strings")
            continue

        out = render(page, html, mapping)
        left = residual_cjk(out)
        os.makedirs(EN_DIR, exist_ok=True)
        out_path = os.path.join(EN_DIR, page + ".html")
        with open(out_path, "w", encoding="utf-8", newline="") as f:
            f.write(out)
        if left:
            failed = True
            print(f"[{page}] written but {len(left)} Chinese string(s) remain:")
            for x in left:
                print("   -", x[:160])
        else:
            print(f"[{page}] ok -> {os.path.relpath(out_path, SITE)}")

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
