#!/usr/bin/env python3
# Copyright 2026 The Glyde Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
"""Build docs/design/glyde-mockup.html from the engine showcase file.

The redesign mockup (issues #229, #230, #231) draws the real data of
`01_engine_testbench.csv`. This script reads that file, reduces it to what a
single self-contained HTML page can carry, and injects it into
`mockup.src.html`:

- a min/max overview of every numeric channel, 250 ms per bucket, whole file;
- every raw 1 kHz sample of every numeric channel in three detail windows
  (cranking, a throttle release with fuel cut, the misfire);
- every run of every bool/state channel and every marker, exact;
- two Welch PSDs computed like glyde-core's defaults (Hann, symmetric; 50 %
  overlap; constant detrend; segment = largest power of two <= N/8;
  one-sided density in unit^2/Hz).

Usage:
    python3 testdata/showcase/generate.py OUT_DIR engine
    python3 docs/design/build_mockup.py OUT_DIR/01_engine_testbench.csv [FRAGMENT.html]

The optional second argument also writes the page without its <html>/<head>
skeleton, for hosts that add their own.

Requires numpy and pyarrow.
"""
import base64
import json
import sys
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.csv as pacsv

HERE = Path(__file__).resolve().parent
SRC = HERE / "mockup.src.html"
OUT = HERE / "glyde-mockup.html"

FS = 1000.0
BUCKET_S = 0.25
# Detail windows, seconds from 09:00:00.000.
WINDOWS = [
    ("Cranking", 1.0, 9.0),
    ("Throttle release, fuel cut", 177.0, 185.0),
    ("Misfire (DTC P0300)", 1320 + 259.0, 1320 + 269.0),
]
# PSD selections, seconds from 09:00:00.000.
PSDS = [
    ("Steady 3140 rpm", 660 + 60.0, 660 + 300.0, ["Crank vibration [m/s²]", "Knock sensor [V]"]),
    ("Misfire", 1320 + 262.0, 1320 + 272.0, ["Crank vibration [m/s²]"]),
]
BOOL_COLS = {"Fan on": {"true": True, "false": False}, "MIL lamp": {"1": True, "0": False}}
STATE_COLS = ["Engine state"]
MARKER_COLS = ["Event"]


def b64f32(a):
    return base64.b64encode(np.asarray(a, dtype="<f4").tobytes()).decode("ascii")


def runs(values, t):
    """[(t_start, t_end_exclusive, value)] for consecutive equal values."""
    change = np.flatnonzero(values[1:] != values[:-1]) + 1
    starts = np.concatenate([[0], change])
    ends = np.concatenate([change, [len(values)]])
    out = []
    for s, e in zip(starts, ends):
        # A run ends one sample period after its last sample, or at a gap.
        out.append([round(float(t[s]), 3), round(float(t[e - 1]) + 1 / FS, 3), values[s]])
    return out


def split_at_gaps(t, *arrays):
    """Yield contiguous pieces (time step <= 1.5 sample periods)."""
    cut = np.flatnonzero(np.diff(t) > 1.5 / FS) + 1
    bounds = np.concatenate([[0], cut, [len(t)]])
    for s, e in zip(bounds[:-1], bounds[1:]):
        yield (t[s:e],) + tuple(a[s:e] for a in arrays)


def welch(x, fs):
    """Same estimator as glyde-core's defaults (see module docstring)."""
    n = len(x)
    target = n // 8
    seg = 1 << (target.bit_length() - 1) if target else 1
    seg = min(max(seg, 256), 65536)
    step = seg // 2
    k = np.arange(seg)
    w = 0.5 - 0.5 * np.cos(2 * np.pi * k / (seg - 1))
    scale = 1.0 / (fs * np.sum(w * w))
    acc = np.zeros(seg // 2 + 1)
    count = 0
    for start in range(0, n - seg + 1, step):
        s = x[start:start + seg]
        if not np.all(np.isfinite(s)):
            continue
        s = (s - s.mean()) * w
        p = np.abs(np.fft.rfft(s)) ** 2 * scale
        p[1:-1] *= 2.0  # one-sided; DC and Nyquist not doubled
        acc += p
        count += 1
    return np.fft.rfftfreq(seg, 1 / fs), acc / count, fs / seg, count, seg


def main():
    csv = Path(sys.argv[1])
    table = pacsv.read_csv(
        csv,
        parse_options=pacsv.ParseOptions(delimiter=";"),
        convert_options=pacsv.ConvertOptions(
            strings_can_be_null=False,
            auto_dict_encode=False,
            check_utf8=True,
        ),
        read_options=pacsv.ReadOptions(block_size=64 << 20),
    )
    names = table.column_names
    ts = table["timestamp"]
    if pa.types.is_string(ts.type):
        ts = pc.strptime(ts, format="%Y-%m-%d %H:%M:%S.%f", unit="ms")
    t_ms = pc.cast(pc.cast(ts, pa.timestamp("ms")), pa.int64()).to_numpy()
    t0 = int(t_ms[0])
    t = (t_ms - t0) / 1000.0

    numeric, bools, states, markers = [], [], [], []
    for name in names[1:]:
        col = table[name]
        if name in BOOL_COLS:
            m = BOOL_COLS[name]
            v = np.array([m[s] for s in pc.cast(col, pa.string()).to_pylist()], dtype=bool)
            bools.append((name, v))
        elif name in STATE_COLS:
            states.append((name, np.array(pc.cast(col, pa.string()).to_pylist(), dtype=object)))
        elif name in MARKER_COLS:
            markers.append((name, np.array(pc.cast(col, pa.string()).to_pylist(), dtype=object)))
        else:
            s = pc.cast(col, pa.string())
            s = pc.replace_substring(s, ",", ".")
            sample = [x for x in s.slice(0, 200000).to_pylist() if x != "NaN"]
            dec = max((len(x.split(".")[1]) if "." in x else 0) for x in sample)
            numeric.append((name, pc.cast(s, pa.float64()).to_numpy(zero_copy_only=False), dec))

    span = float(t[-1]) + 1 / FS
    nb = int(np.ceil(span / BUCKET_S))
    bidx = np.minimum((t / BUCKET_S).astype(np.int64), nb - 1)

    out = {
        "file": csv.name,
        "bytes": csv.stat().st_size,
        "rows": int(len(t)),
        "t0_ms": t0,
        "span": span,
        "fs": FS,
        "bucket": BUCKET_S,
        "segments": [[round(float(p[0][0]), 3), round(float(p[0][-1]) + 1 / FS, 3)]
                     for p in split_at_gaps(t)],
        "numeric": [],
        "bools": [],
        "states": [],
        "markers": [],
        "windows": [],
        "psd": [],
    }
    for name, v, dec in numeric:
        mn = np.full(nb, np.nan)
        mx = np.full(nb, np.nan)
        finite = np.isfinite(v)
        np.fmin.at(mn, bidx[finite], v[finite])
        np.fmax.at(mx, bidx[finite], v[finite])
        nan_runs = []
        for tt, vv in split_at_gaps(t, v):
            bad = ~np.isfinite(vv)
            if bad.any():
                for s, e, val in runs(bad, tt):
                    if val:
                        nan_runs.append([s, e])
        out["numeric"].append({
            "name": name,
            "min": b64f32(mn),
            "max": b64f32(mx),
            "lo": float(np.nanmin(v)),
            "hi": float(np.nanmax(v)),
            "nan": nan_runs,
            "dec": dec,
        })
    for name, v in bools:
        r = []
        for tt, vv in split_at_gaps(t, v):
            r += [[s, e, bool(val)] for s, e, val in runs(vv, tt)]
        out["bools"].append({"name": name, "runs": r})
    for name, v in states:
        r = []
        for tt, vv in split_at_gaps(t, v):
            r += [[s, e, str(val)] for s, e, val in runs(vv, tt)]
        out["states"].append({"name": name, "runs": r})
    for name, v in markers:
        idx = np.flatnonzero(v != "")
        out["markers"].append({"name": name, "events": [[round(float(t[i]), 3), str(v[i])] for i in idx]})

    for label, a, b in WINDOWS:
        i0, i1 = np.searchsorted(t, [a, b])
        out["windows"].append({
            "label": label,
            "t": [round(float(t[i0]), 3), round(float(t[i1 - 1]) + 1 / FS, 3)],
            "n": int(i1 - i0),
            "data": [b64f32(v[i0:i1]) for _, v, _ in numeric],
        })

    by_name = {name: v for name, v, _ in numeric}
    for label, a, b, cols in PSDS:
        i0, i1 = np.searchsorted(t, [a, b])
        entry = {"label": label, "t": [round(float(t[i0]), 3), round(float(t[i1 - 1]) + 1 / FS, 3)],
                 "samples": int(i1 - i0), "series": []}
        for c in cols:
            f, p, df, count, seg = welch(by_name[c][i0:i1], FS)
            entry.update({"df": df, "segments": count, "seg": seg, "nfreq": len(f)})
            entry["series"].append({"name": c, "p": b64f32(p)})
        out["psd"].append(entry)

    page = SRC.read_text(encoding="utf-8")
    marker = "/*__MOCKUP_DATA__*/null"
    assert marker in page, "data placeholder missing from mockup.src.html"
    page = page.replace(marker, json.dumps(out, separators=(",", ":"), ensure_ascii=False))
    if len(sys.argv) > 2:
        # A page fragment, for hosts that wrap it in their own document skeleton.
        Path(sys.argv[2]).write_text(page, encoding="utf-8")
        print(f"{sys.argv[2]}: {Path(sys.argv[2]).stat().st_size / 1e6:.2f} MB")
    head, rest = page.split("<style>", 1)
    style, body = rest.split("</style>", 1)
    OUT.write_text(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n"
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1, viewport-fit=cover\">\n"
        f"{head}<style>{style}</style>\n</head>\n<body>{body}</body>\n</html>\n",
        encoding="utf-8")
    print(f"{OUT.relative_to(HERE.parent.parent)}: {OUT.stat().st_size / 1e6:.2f} MB")


if __name__ == "__main__":
    main()
