# Showcase files: the whole of Glyde in three opens

Three synthetic but realistic logs, each built so that a **single open** exercises most of what Glyde does.
They complement the torture corpus. The corpus proves each behavior in isolation. These files show how the
behaviors look together, on data shaped like real work files. Use them for a quick overall check, for example
before the manual QA ritual (docs/QUALITY.md §5).

The data files are **not committed** (one is 1.2 GB). Generate them with:

```sh
pip install numpy pyarrow
python3 testdata/showcase/generate.py                 # all three -> testdata/generated/showcase/
python3 testdata/showcase/generate.py OUT_DIR engine  # just one: engine | inverter | gearbox
```

Generation is deterministic (fixed seeds) and takes about 3 minutes.

| File | Scenario | Rows | Size | Storage |
|---|---|---|---|---|
| `01_engine_testbench.csv` | Engine test bench, 1 kHz, three runs separated by pauses | 1.44 M | 155 MB | in RAM |
| `02_pv_inverter_spill.csv` | Grid-tied PV inverter, 20 kHz, 10.5 min | 12.55 M | 1.18 GB | **spilled to disk** |
| `03_gearbox_cms_bursts.parquet` | Wind-turbine gearbox monitoring: 2 s at 25 kHz every 10 min for 24 h | 7.2 M | 248 MB | in RAM |

File 02 spills on any machine. The RAM budget is `min(25% of RAM, 4 GB)`, and the file's in-memory estimate is above 6 GB.
Opening it needs about 1.7 GB free on the cache disk.

Missing samples in the CSVs are written as `NaN`, not as blank cells: blank cells currently turn a numeric
column into text (issue #189). Both CSVs currently show the delimiter at `Low` confidence, although it is
inferred correctly (issue #190). Outliers are not yet flagged in the anomaly gutter (ROADMAP M8); today they
are visible only because min/max decimation keeps them.

---

## 01 — Engine test bench (`;` delimiter, `,` decimal separator)

**Inference bar:** `;` and `,` decimal (resolved jointly), `YYYY-MM-DD HH:MM:SS.fff`, SegmentedUniform, 2 gaps.
Gaps are about 20% of the span, so the **Compress control must not appear**.

| Time (2026-03-12) | What to look at |
|---|---|
| 09:00:03 | `Engine state` OFF→CRANKING; `Battery` drops to 9.8 V and oscillates (steep edge) |
| 09:01–09:07 | `Throttle` steps 15/30/0/50/0/25 %; `Engine speed` follows with a ~0.35 s lag; FUEL_CUT on each release, `Lambda` jumps to 2.5 |
| 09:08 → 09:11 | **3-minute pause (time-axis gap)**; the second pause is 09:19 → 09:22 |
| 09:11:40–09:16:00 | Steady ~3140 rpm. **PSD of `Crank vibration`**: ~52 Hz (order 1), ~105 Hz (order 2), ~209 Hz (order 4), and a fixed **217 Hz** resonance next to order 4 (a resolution check) |
| 09:13:30–09:14:15 | **Long hole** in `Oil pressure` (45 s of NaN), plus 24 short 5 ms dropouts across the three runs |
| 09:16 and run 3 | `Knock sensor`: 24 one-sample spikes (2–4.5 V) on 0.05 V noise. They must stay visible at every zoom level |
| whole file | `Lambda`: 18 one-sample glitches to 0.0 (outliers) |
| 09:22:30–09:24:30 | Slow throttle sweep 0→100 %, then wide open until 09:25:20 (`Lambda` 0.88) |
| 09:26:22 | Marker "DTC P0300 misfire"; `MIL lamp` turns on and stays on; `Crank vibration` gains a half-order component for 10 s |
| 09:27:00–09:29:00 | **Long hole** in `Exhaust temp` (2 min) |
| 09:28:04 / 09:29:08 | `Fan on` switches on, then off |

Also included:
- **Slow signals:** `Coolant temp`, `Exhaust temp`.
- **Constant series:** `Ambient pressure` (1013).
- **Integer staircase:** `Combustion cycles`.
- **Booleans in two spellings:** `Fan on` (`true/false`) and `MIL lamp` (`0/1`, reported as ambiguous).
- **States:** `Engine state`, 6 values.
- **Markers:** `Event`, 11 events.
- **Units in square brackets in the header**, including `°C` and `m/s²`.

## 02 — PV inverter, 20 kHz, spilled (`,` delimiter, epoch-microsecond timestamps)

**Inference bar:** epoch µs, implicit UTC, SegmentedUniform, 1 gap. Use this file for continuous pan and zoom
over 12.5 M samples while watching memory. In testing, the open peaked at about 52 MB RSS.

| Time (2026-06-21, UTC) | What to look at |
|---|---|
| 10:00:00–10:00:10 | STANDBY→PRECHARGE→GRID_SYNC; `Contactor closed` at 10:00:10; currents ramp from 0 |
| 10:02:00–10:02:40, 10:06:20, 10:09:00–10:10:00 | Clouds: `P_ac` and current amplitude dip slowly |
| 10:04:00.000–10:04:00.150 | Grid voltage dip: `V_ab` at 60 % for 150 ms, currents +50 %, `V_dc` rises (short, steep event) |
| 10:05:00–10:05:02.5 | **Gap**: logger buffer overrun, 2.5 s of rows missing |
| 10:06:00 | Overtemperature derating (`T_igbt` climbs slowly from 10:05) |
| 10:07:30 | **Fault**: `I_b` at ~185 A for 10 samples (0.5 ms), then every current drops to zero at once; `Fault` = true, state FAULT |
| 10:07:50 / 10:08:00 | Operator reset; back online |
| 10:08:40–10:10:00 | **Long hole** in `T_heatsink` (80 s) |
| scattered | `V_dc`: 10 one-sample glitches to 1021.5 V (outliers) |

**PSD** of `I_a` in MPPT: 50 Hz fundamental, 250 and 350 Hz (5th and 7th harmonics), and 4 kHz (switching ripple).
`V_dc` has a 300 Hz ripple. A selection across 10:05 must say "averaged over 2 segments".

## 03 — Gearbox condition monitoring, event-triggered captures (Parquet)

**Inference bar:** Parquet timestamp in Europe/Rome (+02:00), SegmentedUniform, **143 gaps** (over 99 % of the
span is empty). Here the **Compress control must appear**, and clicking a burst must fit it to the view.

Native dtypes to preserve:
- `f32`: accelerations, power, oil temperature.
- `f64`: shaft speed, bearing temperature.
- `i16`: `adc_raw_HSS`.
- `u32`: `burst_id`.
- `u8`: `status_byte`.
- `bool`: `adc_overrange`.
- Dictionary-encoded string: `operating_mode`.
- Marker strings: `event`.
- Nulls.

| Time (2026-09-14) | What to look at |
|---|---|
| every 10 min | Marker "capture n/144" at the start of each burst |
| 06:10 and 16:50 | `shaft_speed`: one sample at 0 rpm (tacho dropout, outlier) |
| 10:00–11:50 | `operating_mode` = curtailed, with the grid operator's marker |
| from 10:00 | Outer-race bearing defect: impulses (BPFO ≈ 3.58× shaft speed) growing burst after burst |
| 15:00–16:30 | **Long hole** in `bearing_temp` (10 whole bursts null, so the hole spans gaps) |
| 18:30 | Marker "ALARM: BPFO envelope > 4 g" |
| 20:00 | Short hole in `bearing_temp` inside one burst |
| 22:00–23:50 | `adc_raw_HSS` clips at ±32767 (flat-topped, steep); `adc_overrange` = true on 359 samples |

**PSD** of `accel_gearbox_HSS` on one full-load burst: order 1 at ≈ 25 Hz and gear mesh at ≈ 775 Hz (31 teeth) with
±25 Hz sidebands. A resonance near 3.2 kHz appears in the evening bursts. `accel_generator_DE` peaks at 100 Hz.
A selection over several bursts must say "averaged over N segments".
