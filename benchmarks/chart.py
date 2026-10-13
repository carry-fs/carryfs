"""SVG charts of benchmark results, one light and one dark file each.

  python3 benchmarks/chart.py CONFIG OUT_DIR

CONFIG is JSON; copy benchmarks/example-charts.json and edit it. Every entry of
`charts` draws one panel per workload in ROWS with the measured values. Its
`results` is a results.tsv written by benchmarks/run.py, and the chart shows
the medians of its rounds.

`columns` maps each displayed filesystem to a label of the results file.
CarryFS is highlighted. `comparison` draws the charts it names side by side as
speed relative to its `against` filesystem, on a log scale. Paths are relative
to the repository root. Standard library only.
"""
import csv
import json
import math
import statistics
import sys
from collections import defaultdict
from pathlib import Path
from xml.sax.saxutils import escape

ROOT = Path(__file__).resolve().parents[1]

# Label, unit, higher is better, (phase, metric) in results.tsv.
ROWS = [
    ("Sequential write, durable", "MiB/s", True, ("write", "sequential_write_durable")),
    ("Sequential read, cold", "MiB/s", True, ("rw-cold", "sequential_read")),
    ("1,000 small files, durable", "files/s", True, ("write", "small_file_create_durable")),
    ("Create a tree of 1,000 files", "files/s", True, ("write", "tree_create")),
    ("Create 10,000, fsync every 128", "ms", False, ("write", "create_10000_batch128_durable")),
    ("Remove 1,000 small files", "ms", False, ("read", "remove_small_files")),
    ("Remove 10,000, fsync every 128", "ms", False, ("delete", "remove_10000_batch128_durable")),
    ("List 10,000 names, cold", "ms", False, ("ro-cold", "list_10000_names")),
    ("List and stat 10,000 files, cold", "ms", False, ("ro-cold", "list_10000_list-stat")),
]

# The blue and red pair was checked for color-blind readers on both surfaces.
THEMES = {
    "light": dict(surface="#fcfcfb", ink="#0b0b0b", ink2="#52514e", muted="#898781", grid="#e1e0d9",
                  axis="#c3c2b7", border="rgba(11,11,11,0.10)", blue="#2a78d6", red="#e34948"),
    "dark": dict(surface="#1a1a19", ink="#f0efec", ink2="#c3c2b7", muted="#898781", grid="#2c2c2a",
                 axis="#383835", border="rgba(255,255,255,0.10)", blue="#3987e5", red="#e66767"),
}
FONT = "system-ui, -apple-system, 'Segoe UI', sans-serif"
W, PAD = 960, 24


def results_values(lines, columns):
    """Per filesystem, the ROWS medians of a benchmarks/run.py results.tsv."""
    wanted = {key for *_, key in ROWS}
    samples = defaultdict(list)
    for row in csv.DictReader(lines, delimiter="\t"):
        if (row["phase"], row["metric"]) in wanted:
            if row["status"] or not row["value"]:
                raise ValueError(f"failed sample: {row}")
            samples[row["label"], row["phase"], row["metric"]].append(float(row["value"]))
    values = {}
    for fs, label in columns.items():
        missing = [key for *_, key in ROWS if (label, *key) not in samples]
        if missing:
            raise ValueError(f"label {label!r} has no samples for {missing}")
        values[fs] = [statistics.median(samples[(label, *key)]) for *_, key in ROWS]
    return values


def load(chart, root):
    with (root / chart["results"]).open(newline="") as lines:
        values = results_values(lines, chart["columns"])
    if any(v <= 0 for column in values.values() for v in column):
        raise ValueError(f"{chart['name']}: values must be positive")
    return values


def text(x, y, s, fill, size=12, anchor="start", weight=400):
    return (f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" font-weight="{weight}" '
            f'text-anchor="{anchor}" fill="{fill}">{escape(s)}</text>')


def bar(x0, y, length, h, fill):
    """Horizontal bar from the baseline x0, square there and rounded at the data end. A negative length grows left."""
    length = math.copysign(max(abs(length), 2), length)
    r = math.copysign(min(4, abs(length)), length)
    x1 = x0 + length
    return (f'<path fill="{fill}" d="M{x0:.1f},{y:.1f}H{x1 - r:.1f}Q{x1:.1f},{y:.1f} {x1:.1f},{y + 4:.1f}'
            f'V{y + h - 4:.1f}Q{x1:.1f},{y + h:.1f} {x1 - r:.1f},{y + h:.1f}H{x0:.1f}Z"/>')


def frame(height, title, subtitle, note, t, body):
    return "\n".join([
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {height}" width="{W}" height="{height}" '
        f'role="img" font-family="{FONT}">',
        f"<title>{escape(title)}</title><desc>{escape(subtitle)}</desc>",
        f'<rect x="0.5" y="0.5" width="{W - 1}" height="{height - 1}" rx="8" fill="{t["surface"]}" stroke="{t["border"]}"/>',
        text(PAD, 36, title, t["ink"], 16, weight=600),
        text(PAD, 56, subtitle, t["ink2"]),
        *body,
        text(PAD, height - 18, note, t["muted"], 11),
        "</svg>\n",
    ])


def fmt(v):
    return f"{v:,.0f}" if v >= 100 else f"{v:.1f}" if v >= 10 else f"{v:.2f}"


def panels(chart, values, t):
    """One small panel per workload with measured values; CarryFS in the accent, the rest gray."""
    cols, gap, label_w, value_w, pitch = 3, 24, 58, 64, 22
    pw = (W - 2 * PAD - (cols - 1) * gap) / cols
    ph = 40 + len(values) * pitch + 20
    body = []
    for i, (label, unit, higher, *_) in enumerate(ROWS):
        px, py = PAD + (i % cols) * (pw + gap), 84 + (i // cols) * ph
        x0 = px + label_w
        top = max(column[i] for column in values.values())
        body.append(text(px, py + 12, label, t["ink"], weight=600))
        body.append(text(px, py + 28, f'{unit} · {"higher" if higher else "lower"} is better', t["muted"], 11))
        for j, (fs, column) in enumerate(values.items()):
            y = py + 40 + j * pitch
            carry = fs == "CarryFS"
            length = column[i] / top * (pw - label_w - value_w)
            body.append(text(px, y + 11, fs, t["ink"] if carry else t["ink2"], 11, weight=600 if carry else 400))
            body.append(bar(x0, y, length, 14, t["blue"] if carry else t["muted"]))
            body.append(text(x0 + max(length, 2) + 6, y + 11, fmt(column[i]), t["ink"] if carry else t["ink2"], 11))
        body.append(f'<line x1="{x0:.1f}" x2="{x0:.1f}" y1="{py + 36:.1f}" y2="{py + 36 + len(values) * pitch:.1f}" stroke="{t["axis"]}"/>')
    bottom = 84 + math.ceil(len(ROWS) / cols) * ph
    return frame(bottom + 24, f"CarryFS on {chart['system']}", chart["subtitle"], chart["note"], t, body)


def comparison(spec, facets, t):
    """CarryFS speed relative to one filesystem per workload, one facet per chart, log scale."""
    against = spec["against"]
    label_w, gap, pitch, lo, hi = 220, 20, 28, -4.0, 5.6
    fw = (W - 2 * PAD - label_w - (len(facets) - 1) * gap) / len(facets)
    scale = fw / (hi - lo)
    top, bottom = 122, 122 + len(ROWS) * pitch
    body = []
    lx = PAD
    for fill, name in ((t["blue"], "CarryFS faster"), (t["red"], f"{against} faster"), (t["muted"], "within 2%")):
        body.append(f'<rect x="{lx}" y="71" width="10" height="10" rx="2" fill="{fill}"/>')
        body.append(text(lx + 16, 80, name, t["ink2"], 11))
        lx += 16 + 6 * len(name) + 20
    for i, row in enumerate(ROWS):
        body.append(text(PAD, top + i * pitch + 18, row[0], t["ink"]))
    for k, (name, values) in enumerate(facets):
        fx = PAD + label_w + k * (fw + gap)
        x1 = fx - lo * scale
        body.append(text(fx, 108, name, t["ink"], weight=600))
        for tick, tick_label in ((-2, "¼×"), (0, "1×"), (2, "4×"), (4, "16×")):
            x = x1 + tick * scale
            body.append(f'<line x1="{x:.1f}" x2="{x:.1f}" y1="{top}" y2="{bottom}" stroke="{t["axis"] if tick == 0 else t["grid"]}"/>')
            body.append(text(x, bottom + 16, tick_label, t["muted"], 11, "middle"))
        for i, row in enumerate(ROWS):
            carry, other = values["CarryFS"][i], values[against][i]
            ratio = carry / other if row[2] else other / carry
            log = math.log2(ratio)
            # Past these the value at the bar's tip leaves its facet; widen lo and hi first.
            if not -2.6 < log < 4.4:
                raise ValueError(f"{name}, {row[0]}: {ratio:.2f}x does not fit the axis")
            tie = abs(log) < math.log2(1.02)
            fill = t["muted"] if tie else t["blue"] if log > 0 else t["red"]
            y = top + i * pitch + 7
            body.append(bar(x1, y, log * scale, 14, fill))
            shown = f"{ratio:.0f}×" if ratio >= 10 else f"{ratio:.1f}×" if ratio >= 0.995 else f"{ratio:.2f}×"
            tip = x1 + math.copysign(max(abs(log * scale), 2) + 6, log)
            body.append(text(tip, y + 11, shown, t["ink2"], 11, "start" if log >= 0 else "end"))
    return frame(bottom + 60, spec["title"], spec["subtitle"], spec["note"], t, body)


def render(config, root, out):
    """Write every chart of `config` under `out` in both themes and return the paths."""
    charts = {chart["name"]: (chart, load(chart, root)) for chart in config["charts"]}
    out.mkdir(parents=True, exist_ok=True)
    written = []
    for theme, t in THEMES.items():
        drawn = {name: panels(chart, values, t) for name, (chart, values) in charts.items()}
        spec = config.get("comparison")
        if spec:
            drawn[spec["name"]] = comparison(spec, [(charts[n][0]["system"], charts[n][1]) for n in spec["charts"]], t)
        for name, svg in drawn.items():
            written.append(out / f"{name}-{theme}.svg")
            written[-1].write_text(svg, encoding="utf-8")
    return written


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    for path in render(json.loads(Path(sys.argv[1]).read_text()), ROOT, Path(sys.argv[2])):
        print(path)
