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
"""Generate the Glyde showcase files: three realistic, synthetic logs built to
exercise every Glyde feature in a single open each (see README.md here).

    01_engine_testbench.csv        in-RAM CSV (';' delimiter, ',' decimals, 1 kHz)
    02_pv_inverter_spill.csv       CSV large enough to spill to disk (20 kHz, ~1.2 GB)
    03_gearbox_cms_bursts.parquet  periodic event-triggered 25 kHz captures

Requires: numpy, pyarrow. Deterministic (fixed seeds).
Usage: python3 generate.py [output_dir] [engine] [inverter] [gearbox]
Default output_dir: testdata/generated/showcase/ (git-ignored).
"""
import math
import sys
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.csv as pacsv
import pyarrow.parquet as pq

OUT = (Path(sys.argv[1]) if len(sys.argv) > 1
       else Path(__file__).resolve().parent.parent / "generated" / "showcase")
OUT.mkdir(parents=True, exist_ok=True)


# ----------------------------------------------------------------- helpers
def lag(x, dt, tau, y0=None):
    """First-order lag (exact discretisation), plain loop: use on coarse grids."""
    a = 1.0 - math.exp(-dt / tau)
    y = np.empty_like(x, dtype=np.float64)
    acc = x[0] if y0 is None else y0
    for i, v in enumerate(x):
        acc += a * (v - acc)
        y[i] = acc
    return y


def piecewise(t, schedule):
    """schedule: list of (t_start, value); value holds until the next start."""
    starts = np.array([s for s, _ in schedule])
    vals = np.array([v for _, v in schedule], dtype=np.float64)
    return vals[np.searchsorted(starts, t, side="right") - 1]


def fmt_num(values, decimals, comma=False):
    """Float array -> Arrow string array, NaN written as 'NaN'."""
    arr = pa.array(np.round(values, decimals), type=pa.float64())
    s = pc.cast(arr, pa.string())
    s = pc.if_else(pc.is_nan(arr), pa.scalar("NaN"), s)
    if comma:
        s = pc.replace_substring(s, ".", ",")
    return s


# ======================================================================
# 01 - Engine test bench, 1 kHz, three runs separated by pauses (in RAM)
# ======================================================================
def engine():
    rng = np.random.default_rng(1)
    fs = 1000
    dt = 1.0 / fs
    run_len = 480.0  # s
    pause = 180.0  # s between runs -> time-axis gaps (~20% of the span)
    t0 = np.datetime64("2026-03-12T09:00:00.000", "ms")

    # Throttle schedules per run: (t, %) -- steps are steep, sweep is slow.
    schedules = [
        [(0, 0), (60, 15), (120, 30), (180, 0), (210, 50), (270, 0), (300, 25), (420, 0)],
        [(0, 0), (40, 45), (300, 70), (360, 0), (420, 0)],  # long steady 3000 rpm
        [(0, 0), (30, "ramp"), (150, 100), (200, 0), (230, 60), (330, 85), (400, 0)],
    ]
    events = {
        0: [(8.0, "cal map v2 loaded"), (60.0, "throttle step 15%"), (180.0, "operator: warm-up done")],
        1: [(45.0, "steady state 3000 rpm"), (150.0, "oil pressure sensor connector loose"),
            (200.0, "connector reseated")],
        2: [(30.0, "sweep start"), (150.0, "sweep end"), (152.0, "fuel cut"),
            (262.0, "DTC P0300 misfire"), (300.0, "thermocouple EGT open circuit")],
    }

    cols = {k: [] for k in [
        "ts", "rpm", "thr", "trq", "cool", "oil", "egt", "vib", "knock", "lam", "batt",
        "fan", "mil", "state", "event", "amb", "cyc"]}
    coolant_prev, egt_prev, fan_prev, cyc_prev, mil_prev = 18.0, 25.0, False, 0, False

    for r, sched in enumerate(schedules):
        n = int(run_len * fs)
        t = np.arange(n) * dt
        # throttle setpoint (with the slow sweep in run 3)
        thr = np.zeros(n)
        thr[:] = piecewise(t, [(s, 0 if v == "ramp" else v) for s, v in sched])
        if r == 2:
            m = (t >= 30) & (t < 150)
            thr[m] = (t[m] - 30) / 120 * 100
        # engine off -> cranking -> running -> stop at end of run
        crank = (t >= 3) & (t < 5)
        off = (t < 3) | (t >= run_len - 10)
        target = 800 + thr * 52.0
        target[off] = 0.0
        target[crank] = 250.0
        # rpm: fast lag (tau 0.35 s) on a 100 Hz grid, then interpolated
        tc = t[::10]
        rpm = np.interp(t, tc, lag(target[::10], 0.01, 0.35, 0.0))
        rpm += rng.normal(0, 4, n) * (rpm > 100)
        rpm = np.maximum(rpm, 0)

        running = (rpm > 400) & ~crank
        decel = (thr == 0) & (rpm > 1400)
        state = np.where(off & (rpm < 50), "OFF", "RUNNING").astype(object)
        state[crank] = "CRANKING"
        state[running & (thr == 0) & (rpm < 1100)] = "IDLE"
        state[running & decel] = "FUEL_CUT"
        state[off & (rpm >= 50)] = "STOPPING"

        load = thr / 100.0
        trq = load * 320 * (1 - ((rpm - 3800) / 4200) ** 2) * running
        trq[running & decel] = -28.0
        trq += rng.normal(0, 1.5, n) * running

        # slow thermal signals on a 10 Hz grid
        ts10 = t[::100]
        heat = np.interp(ts10, t, (load * 0.9 + 0.25) * running)
        cool_target = 18 + heat * 85  # high sustained load overshoots the thermostat
        coolant = lag(cool_target, 0.1, 140.0, coolant_prev)
        coolant = np.minimum(coolant, 88 + np.maximum(coolant - 88, 0) * 0.6)
        egt = lag(np.interp(ts10, t, 25 + (180 + rpm * 0.09 + load * 380) * running), 0.1, 9.0, egt_prev)
        coolant_prev, egt_prev = coolant[-1] - 4.0, egt[-1] * 0.5  # cools during pause
        coolant = np.interp(t, ts10, coolant) + rng.normal(0, 0.05, n)
        egt = np.interp(t, ts10, egt) + rng.normal(0, 1.5, n)

        # fan relay with hysteresis (85 on / 80 off), evaluated at 10 Hz
        fan10 = np.empty(len(ts10), dtype=bool)
        c10 = coolant[::100]
        for i, c in enumerate(c10):
            fan_prev = True if c > 85 else (False if c < 80 else fan_prev)
            fan10[i] = fan_prev
        fan = np.repeat(fan10, 100)[:n]

        oil = np.clip(0.4 + rpm / 1400, 0, 5.2) * (1.15 - (coolant - 18) / 400) * (rpm > 100)
        oil += rng.normal(0, 0.03, n)

        # crank vibration: orders 1 and 2 from integrated crank angle + 217 Hz resonance
        phase = 2 * np.pi * np.cumsum(rpm / 60.0) * dt
        vib = (0.6 * np.sin(phase) + (1.0 + trq / 120) * np.sin(2 * phase + 0.3)
               + 0.3 * np.sin(4 * phase) + 0.25 * np.sin(2 * np.pi * 217 * t) * (rpm > 400)
               + rng.normal(0, 0.15, n))
        if r == 2:  # misfire: half-order content for 10 s
            m = (t >= 262) & (t < 272)
            vib[m] += 1.8 * np.sin(0.5 * phase[m])
        vib *= running | crank

        knock = rng.normal(0, 0.05, n)
        hot = np.flatnonzero(load > 0.6)
        if len(hot):
            k = rng.choice(hot, size=12, replace=False)
            knock[k] = rng.uniform(2.0, 4.5, 12)  # one-sample knock spikes (outliers)

        lam = 1.0 + rng.normal(0, 0.008, n)
        lam[thr > 80] = 0.88 + rng.normal(0, 0.006, (thr > 80).sum())
        lam[decel] = 2.5  # fuel cut: sensor rails lean (steep edge)
        lam[~running] = 2.5
        g = rng.choice(np.flatnonzero(running), size=6, replace=False)
        lam[g] = 0.0  # ground-glitch outliers

        batt = np.where(running, 14.1, 12.6) + 0.04 * np.sin(2 * np.pi * (rpm / 60 * 3) * t)
        batt[crank] = 9.8 + 0.3 * np.sin(2 * np.pi * 8 * t[crank])
        batt += rng.normal(0, 0.01, n)

        # long holes: oil pressure (45 s) in run 2, EGT (120 s) in run 3, short dropouts elsewhere
        if r == 1:
            oil[(t >= 150) & (t < 195)] = np.nan
        if r == 2:
            egt[(t >= 300) & (t < 420)] = np.nan
        for s in rng.choice(n - 10, size=8, replace=False):
            oil[s:s + 5] = np.nan

        mil = np.full(n, mil_prev)
        if r == 2:
            mil[t >= 262] = True
            mil_prev = True
        cyc = cyc_prev + np.floor(np.cumsum(rpm / 120.0) * dt).astype(np.int64)
        cyc_prev = int(cyc[-1])

        ev = np.full(n, "", dtype=object)
        for et, label in events[r]:
            ev[int(et * fs)] = label

        start = t0 + np.timedelta64(int(r * (run_len + pause) * 1000), "ms")
        stamps = start + np.arange(n).astype("timedelta64[ms]")
        cols["ts"].append(pc.replace_substring(pa.array(np.datetime_as_string(stamps, unit="ms")), "T", " "))
        for key, vals, dec in [("rpm", rpm, 1), ("thr", thr, 1), ("trq", trq, 1), ("cool", coolant, 2),
                               ("oil", oil, 3), ("egt", egt, 1), ("vib", vib, 3), ("knock", knock, 3),
                               ("lam", lam, 3), ("batt", batt, 3)]:
            cols[key].append(fmt_num(vals, dec, comma=True))
        cols["fan"].append(pa.array(np.where(fan, "true", "false")))
        cols["mil"].append(pa.array(np.where(mil, "1", "0")))
        cols["state"].append(pa.array(state.astype(str)))
        cols["event"].append(pa.array(ev.astype(str)))
        cols["amb"].append(pa.array(np.full(n, "1013")))
        cols["cyc"].append(pa.array(cyc.astype(str)))

    header = ["timestamp", "Engine speed [rpm]", "Throttle [%]", "Torque [Nm]", "Coolant temp [°C]",
              "Oil pressure [bar]", "Exhaust temp [°C]", "Crank vibration [m/s²]", "Knock sensor [V]",
              "Lambda [-]", "Battery [V]", "Fan on", "MIL lamp", "Engine state", "Event",
              "Ambient pressure [mbar]", "Combustion cycles"]
    keys = ["ts", "rpm", "thr", "trq", "cool", "oil", "egt", "vib", "knock", "lam", "batt",
            "fan", "mil", "state", "event", "amb", "cyc"]
    path = OUT / "01_engine_testbench.csv"
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(";".join(header) + "\n")  # NaN, not blank cells: see issue #189
        for r in range(len(schedules)):
            lines = pc.binary_join_element_wise(*[cols[k][r] for k in keys], ";")
            f.write("\n".join(lines.to_pylist()) + "\n")
    print(f"{path.name}: {path.stat().st_size / 1e6:.0f} MB")


# ======================================================================
# 02 - Grid-tied PV inverter, 20 kHz, ~10.5 min -> spills to disk
# ======================================================================
def inverter():
    rng = np.random.default_rng(2)
    fs = 20_000
    dt = 1.0 / fs
    duration = 630.0
    gap = (300.0, 302.5)  # logger buffer overrun: rows missing -> time gap
    epoch0_us = int(np.datetime64("2026-06-21T10:00:00", "us").astype(np.int64))

    # slow plan on a 10 Hz grid
    tg = np.arange(0, duration, 0.1)
    irr = 0.85 + 0.1 * np.sin(2 * np.pi * tg / 400)
    for c0, c1, depth in [(120, 160, 0.45), (380, 395, 0.3), (540, 600, 0.55)]:  # clouds
        m = (tg >= c0) & (tg < c1)
        irr[m] -= depth * np.sin(np.pi * (tg[m] - c0) / (c1 - c0))
    online = ((tg >= 10) & (tg < 450)) | (tg >= 480)
    derate = (tg >= 360) & (tg < 450)
    p_cmd = irr * 40.0 * online  # kW
    p_cmd[derate] = np.minimum(p_cmd[derate], 30.0)
    p_slow = lag(p_cmd, 0.1, 1.2, 0.0)
    t_igbt = lag(35 + p_slow * 1.3 + np.where(tg > 300, 22, 0) * online, 0.1, 25.0, 30.0)
    t_hs = lag(30 + p_slow * 0.8, 0.1, 90.0, 28.0)
    fan_pct = np.clip(np.round((t_hs - 35) * 4), 0, 100)
    f_grid = 50 + np.cumsum(rng.normal(0, 0.0004, len(tg)))
    f_grid = 50 + (f_grid - f_grid.mean()) * 0.6

    def state_at(t):
        s = np.full(len(t), "MPPT", dtype=object)
        s[t < 5] = "STANDBY"
        s[(t >= 5) & (t < 8)] = "PRECHARGE"
        s[(t >= 8) & (t < 10)] = "GRID_SYNC"
        s[(t >= 360) & (t < 450)] = "DERATING"
        s[(t >= 450) & (t < 470)] = "FAULT"
        s[(t >= 470) & (t < 475)] = "PRECHARGE"
        s[(t >= 475) & (t < 480)] = "GRID_SYNC"
        return s

    events = [(5.0, "precharge start"), (10.0, "contactor closed"), (240.0, "grid voltage dip"),
              (360.0, "IGBT overtemp derating"), (450.0, "fault: overcurrent phase B"),
              (470.0, "operator reset"), (480.0, "back online")]
    ev_idx = {int(round(et * fs)): label for et, label in events}

    path = OUT / "02_pv_inverter_spill.csv"
    chunk = 2_000_000
    total = int(duration * fs)
    vpk = 230 * math.sqrt(2)
    phase_acc = 0.0
    first = True
    glitch_rows = set(rng.choice(total, size=10, replace=False).tolist())
    with open(path, "wb") as f:
        for c0 in range(0, total, chunk):
            idx = np.arange(c0, min(c0 + chunk, total))
            t = idx * dt
            n = len(idx)
            fg = np.interp(t, tg, f_grid)
            ph = phase_acc + 2 * np.pi * np.cumsum(fg) * dt
            phase_acc = ph[-1]
            p = np.interp(t, tg, p_slow)
            on = np.interp(t, tg, online.astype(float)) > 0.5
            ipk = p * 1000 / (1.5 * vpk) * on
            dip = (t >= 240.0) & (t < 240.15)
            vscale = np.where(dip, 0.6, 1.0)
            ipk = np.where(dip, ipk * 1.5, ipk)  # LVRT: reactive current injection
            ia, ib, ic = (
                ipk * (np.sin(ph - k) + 0.03 * np.sin(5 * (ph - k)) + 0.02 * np.sin(7 * (ph - k)))
                + 1.5 * on * np.sin(2 * np.pi * 4000 * t + k) + rng.normal(0, 0.2, n)
                for k in (0.0, 2 * np.pi / 3, 4 * np.pi / 3))
            fault = (t >= 450.0) & (t < 470.0)
            spike = (t >= 449.9995) & (t < 450.0)  # 10 samples overcurrent on phase B
            ib = np.where(spike, 185.0 + rng.normal(0, 3, n), ib)
            ia = np.where(fault, rng.normal(0, 0.2, n), ia)
            ib = np.where(fault, rng.normal(0, 0.2, n), ib)
            ic = np.where(fault, rng.normal(0, 0.2, n), ic)
            vab = vpk * math.sqrt(3) * vscale * np.sin(ph + np.pi / 6) + rng.normal(0, 1.0, n)
            vdc = np.where(on, 650 + 4 * np.sin(6 * ph), 780 - 0.01 * t) + rng.normal(0, 0.8, n)
            vdc = np.where(dip, vdc + 35, vdc)
            for g in glitch_rows.intersection(idx.tolist()):
                vdc[g - c0] = 1021.5  # single-sample measurement glitches (outliers)
            pac = (ia * np.sin(ph) + ib * np.sin(ph - 2 * np.pi / 3) + ic * np.sin(ph - 4 * np.pi / 3)) \
                * vpk * vscale / 1000
            tig = np.interp(t, tg, t_igbt) + rng.normal(0, 0.1, n)
            ths = np.interp(t, tg, t_hs) + rng.normal(0, 0.05, n)
            ths[(t >= 520) & (t < 600)] = np.nan  # heatsink sensor: 80 s hole
            fan = np.interp(t, tg, fan_pct).round().astype(np.int64)
            contactor = on & ~fault
            st = state_at(t)
            ev = np.array([ev_idx.get(i, "") for i in idx.tolist()], dtype=object) \
                if any(c0 <= i < c0 + n for i in ev_idx) else np.full(n, "", dtype=object)

            keep = ~((t >= gap[0]) & (t < gap[1]))
            ts_us = epoch0_us + (idx * 50).astype(np.int64)
            table = pa.table({
                "timestamp_us": pa.array(ts_us[keep]),
                "I_a [A]": fmt_num(ia[keep], 2), "I_b [A]": fmt_num(ib[keep], 2),
                "I_c [A]": fmt_num(ic[keep], 2), "V_ab [V]": fmt_num(vab[keep], 1),
                "V_dc [V]": fmt_num(vdc[keep], 1), "P_ac [kW]": fmt_num(pac[keep], 2),
                "f_grid [Hz]": fmt_num(fg[keep], 4), "T_igbt [°C]": fmt_num(tig[keep], 2),
                "T_heatsink [°C]": fmt_num(ths[keep], 2), "Fan [%]": pa.array(fan[keep]),
                "Contactor closed": pa.array(contactor[keep]), "Fault": pa.array(fault[keep]),
                "State": pa.array(st[keep].astype(str)),
                "Event": pa.array(ev[keep].astype(str)),
            })
            if first:  # Arrow always quotes header names; write a plain one
                f.write((",".join(table.column_names) + "\n").encode("utf-8"))
            opts = pacsv.WriteOptions(include_header=False, quoting_style="none")
            pacsv.write_csv(table, f, write_options=opts)
            first = False
            print(f"  inverter: {min(c0 + chunk, total) / total:.0%}", flush=True)
    print(f"{path.name}: {path.stat().st_size / 1e6:.0f} MB")


# ======================================================================
# 03 - Gearbox condition monitoring: 2 s @ 25 kHz every 10 min for 24 h
# ======================================================================
def gearbox():
    rng = np.random.default_rng(3)
    fs = 25_000
    burst_s = 2.0
    period_s = 600
    bursts = 144
    n = int(fs * burst_s)
    t = np.arange(n) / fs
    day0 = np.datetime64("2026-09-14T00:00:00", "ns")

    writer = None
    alarmed = False
    path = OUT / "03_gearbox_cms_bursts.parquet"
    modes = ["idling", "partial_load", "full_load", "curtailed"]
    schema = pa.schema([
        ("timestamp", pa.timestamp("ns", tz="Europe/Rome")),
        ("accel_gearbox_HSS [g]", pa.float32()),
        ("accel_generator_DE [g]", pa.float32()),
        ("shaft_speed [rpm]", pa.float64()),
        ("power [kW]", pa.float32()),
        ("oil_temp [°C]", pa.float32()),
        ("bearing_temp [°C]", pa.float64()),
        ("adc_raw_HSS [counts]", pa.int16()),
        ("adc_overrange", pa.bool_()),
        ("burst_id", pa.uint32()),
        ("status_byte", pa.uint8()),
        ("operating_mode", pa.dictionary(pa.int8(), pa.string())),
        ("event", pa.string()),
    ])
    for b in range(bursts):
        hour = b * period_s / 3600
        wind = 0.55 + 0.35 * math.sin(2 * math.pi * (hour - 9) / 24) + rng.normal(0, 0.08)
        mode = "idling" if wind < 0.3 else ("full_load" if wind > 0.8 else "partial_load")
        if 60 <= b < 72:
            mode = "curtailed"  # grid operator curtailment block
        rpm0 = {"idling": 600, "partial_load": 1300, "full_load": 1500, "curtailed": 1150}[mode]
        rpm = rpm0 + rng.normal(0, 8) + 6 * np.sin(2 * np.pi * 0.7 * t)
        rpm_g = rpm + rng.normal(0, 0.3, n)
        if b in (37, 101):
            rpm_g[rng.integers(0, n)] = 0.0  # tacho dropout glitch (outlier)
        f1 = rpm / 60
        ph = 2 * np.pi * np.cumsum(f1) / fs
        power = {"idling": 15, "partial_load": 1300, "full_load": 2050, "curtailed": 900}[mode]
        load = power / 2050

        # bearing outer-race defect: starts at burst 60, grows over the day
        sev = 0.0 if b < 60 else ((b - 60) / 84) ** 1.6 * 9.0
        bpfo = 3.58 * f1.mean()
        impulses = np.zeros(n)
        if sev > 0:
            hits = np.arange(0, burst_s, 1 / bpfo)
            for h in hits:
                k = int(h * fs)
                m = slice(k, min(k + 250, n))
                tt = np.arange(m.stop - m.start) / fs
                impulses[m] += sev * np.exp(-tt * 900) * np.sin(2 * np.pi * 3200 * tt)
        hss = (0.25 * np.sin(ph) + (0.6 + load) * np.sin(31 * ph)  # gear mesh = 31 x shaft
               + 0.2 * np.sin(31 * ph) * np.sin(ph) + impulses + rng.normal(0, 0.08, n))
        gen = 0.15 * np.sin(ph) + 0.35 * load * np.sin(2 * np.pi * 100 * t) + rng.normal(0, 0.05, n)
        raw = np.clip(np.round(hss * 4000), -32768, 32767).astype(np.int16)
        overrange = (raw == 32767) | (raw == -32768)

        oil = 42 + 18 * load + 6 * math.sin(2 * math.pi * (hour - 14) / 24) + rng.normal(0, 0.05, n)
        brg = 55 + 22 * load + sev * 1.5 + rng.normal(0, 0.1, n)
        if 90 <= b < 100:
            brg[:] = np.nan  # PT100 open circuit for ~100 min: long hole across bursts
        if b == 120:
            brg[20_000:26_000] = np.nan  # short hole inside one burst

        status = np.full(n, 0x01, dtype=np.uint8) | (0x02 if mode == "full_load" else 0) \
            | (0x80 if sev > 4 else 0)
        status = status | np.where(overrange, 0x40, 0).astype(np.uint8)

        ev = [None] * n
        ev[0] = f"capture {b + 1}/144"
        if b == 60:
            ev[n // 2] = "curtailment ordered by grid operator"
        if sev > 4 and not alarmed:
            ev[n // 2] = "ALARM: BPFO envelope > 4 g"
            alarmed = True

        stamps = day0 + np.timedelta64(b * period_s, "s") + (np.arange(n) * 40).astype("timedelta64[ns]")
        bn = np.where(np.isnan(brg), None, brg)
        table = pa.table({
            "timestamp": pa.array(stamps.astype("datetime64[ns]"), type=pa.timestamp("ns")).cast(
                pa.timestamp("ns", tz="Europe/Rome")),
            "accel_gearbox_HSS [g]": pa.array(hss, type=pa.float32()),
            "accel_generator_DE [g]": pa.array(gen, type=pa.float32()),
            "shaft_speed [rpm]": pa.array(np.round(rpm_g, 2)),
            "power [kW]": pa.array(power * (1 + rng.normal(0, 0.01, n)), type=pa.float32()),
            "oil_temp [°C]": pa.array(oil, type=pa.float32()),
            "bearing_temp [°C]": pa.array(bn, type=pa.float64()),
            "adc_raw_HSS [counts]": pa.array(raw),
            "adc_overrange": pa.array(overrange),
            "burst_id": pa.array(np.full(n, b + 1, dtype=np.uint32)),
            "status_byte": pa.array(status),
            "operating_mode": pa.array([mode] * n).dictionary_encode().cast(pa.dictionary(pa.int8(), pa.string())),
            "event": pa.array(ev, type=pa.string()),
        }, schema=schema)
        if writer is None:
            writer = pq.ParquetWriter(path, schema, compression="zstd")
        writer.write_table(table)
    writer.close()
    print(f"{path.name}: {path.stat().st_size / 1e6:.0f} MB")


if __name__ == "__main__":
    which = set(sys.argv[2:]) or {"engine", "inverter", "gearbox"}
    if "engine" in which:
        engine()
    if "inverter" in which:
        inverter()
    if "gearbox" in which:
        gearbox()
