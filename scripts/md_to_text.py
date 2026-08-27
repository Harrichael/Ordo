#!/usr/bin/env python3
"""Minimal Markdown -> monospaced plain text for the shortcuts cheat sheet.

Handles just what docs/SHORTCUTS.md uses: ATX headings, pipe tables (rendered as
aligned columns), and paragraphs. Not a general Markdown implementation — it
exists so `cupsfilter` can turn the result into a readable PDF with only stock
macOS tools.
"""
import sys


def is_table_row(line):
    return line.strip().startswith("|") and line.strip().endswith("|")


def is_divider(cells):
    return all(set(c.strip()) <= set("-: ") and "-" in c for c in cells)


def split_row(line):
    return [c.strip() for c in line.strip().strip("|").split("|")]


def render_table(rows):
    grids = [split_row(r) for r in rows if not is_divider(split_row(r))]
    if not grids:
        return []
    width = max(len(r) for r in grids)
    grids = [r + [""] * (width - len(r)) for r in grids]
    colw = [max(len(r[c]) for r in grids) for c in range(width)]
    out = []
    for i, r in enumerate(grids):
        out.append("  ".join(cell.ljust(colw[c]) for c, cell in enumerate(r)).rstrip())
        if i == 0:
            out.append("  ".join("-" * colw[c] for c in range(width)))
    return out


def main():
    src = open(sys.argv[1], encoding="utf-8").read().splitlines()
    out = []
    i = 0
    while i < len(src):
        line = src[i]
        if is_table_row(line):
            block = []
            while i < len(src) and is_table_row(src[i]):
                block.append(src[i])
                i += 1
            out.extend(render_table(block))
            out.append("")
            continue
        if line.startswith("# "):
            t = line[2:].strip()
            out.append(t.upper())
            out.append("=" * len(t))
        elif line.startswith("## "):
            t = line[3:].strip()
            out.append("")
            out.append(t)
            out.append("-" * len(t))
        elif line.startswith("### "):
            out.append("")
            out.append(line[4:].strip())
        else:
            out.append(line)
        i += 1

    # Collapse 3+ blank lines to 1 for tidiness.
    text = "\n".join(out)
    while "\n\n\n" in text:
        text = text.replace("\n\n\n", "\n\n")
    sys.stdout.write(text.strip() + "\n")


if __name__ == "__main__":
    main()
