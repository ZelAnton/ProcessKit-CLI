"""Validate local links and fragments in an mdBook HTML output directory."""

from __future__ import annotations

import argparse
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit


class _Document(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.ids: set[str] = set()
        self.hrefs: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        values = dict(attrs)
        element_id = values.get("id")
        if element_id is not None:
            self.ids.add(element_id)
        href = values.get("href")
        if tag == "a" and href is not None:
            self.hrefs.append(href)


def _read_document(path: Path) -> _Document:
    document = _Document()
    document.feed(path.read_text(encoding="utf-8"))
    return document


def check_book(book: Path) -> list[str]:
    """Return actionable errors for broken local links in a built mdBook."""
    root = book.resolve()
    pages = {path: _read_document(path) for path in root.rglob("*.html")}
    if not pages:
        return [f"{root}: no HTML pages found"]

    problems: list[str] = []
    for source, document in sorted(pages.items()):
        source_name = source.relative_to(root).as_posix()
        for raw_href in document.hrefs:
            link = urlsplit(raw_href)
            if link.scheme or link.netloc or raw_href.startswith("/"):
                continue

            target = source if not link.path else (source.parent / unquote(link.path)).resolve()
            if not target.is_relative_to(root):
                problems.append(f"{source_name} -> {raw_href}: target is outside the book")
                continue
            if target.is_dir():
                target /= "index.html"
            elif not target.suffix:
                target = target.with_suffix(".html")

            target_document = pages.get(target)
            if target_document is None:
                if not target.exists():
                    problems.append(f"{source_name} -> {raw_href}: missing target")
                continue

            fragment = unquote(link.fragment)
            if fragment and fragment not in target_document.ids:
                problems.append(f"{source_name} -> {raw_href}: missing fragment")

    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("book", nargs="?", type=Path, default=Path("book"))
    args = parser.parse_args()

    problems = check_book(args.book)
    if problems:
        print(f"Found {len(problems)} broken rendered documentation link(s):")
        for problem in problems:
            print(f"- {problem}")
        return 1

    page_count = sum(1 for _ in args.book.rglob("*.html"))
    print(f"Validated local links and fragments across {page_count} HTML pages.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
