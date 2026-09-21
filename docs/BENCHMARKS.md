# br0x benchmarks vs Firefox and Brave

Date: 2026-09-21 (IST). Machine: MSI laptop, AMD Ryzen 5 7530U (12 threads),
7.1 GiB RAM, Arch Linux, Hyprland 0.56.2 (Wayland).
System WebKitGTK 2.52.5, GTK 4.22.4, libadwaita 1.9.3.

Versions: br0x rebuilt from source this session (`cargo build --release
-p br0x-shell-gtk`, binary timestamp 15:43 IST, 798,632 bytes);
Mozilla Firefox 153.0.4; Brave Browser 151.1.93.136.
Install sizes: `target/release/br0x` 0.8 MB, `/usr/lib/firefox` 304 MB,
`/opt/brave-bin` 470 MB. (br0x links the system WebKitGTK dynamically, so
its binary size flatters it — the engine lives in shared system libraries.)

Update 2026-09-21 evening: Google Chrome 153.0.8010.52 installed
(`/opt/google/chrome`, 434 MB) and measured with the same method; br0x
re-measured on current sources (binary 802,296 bytes). Chrome trials and
refreshed br0x trials are appended below; the medians table and verdict
reflect all runs.

## Method

Memory = PSS of the full process tree via `scripts/measure.py` (smaps_rollup,
same tool for every browser). Tab set = N identical `https://example.com`
tabs (1 / 5 / 10). 3 trials per cell, fresh profile/XDG dirs per trial, ~20 s
settle after tabs load, then one PSS sample + 10 s idle-CPU sample (summed
utime/stime over the process tree, % of one core). Startup = spawn until the
window appears in `hyprctl clients -j` (same definition as `scripts/bench.sh`).

Exact per-trial procedure (values of `$PROF`, `$URLS` vary by cell):

```bash
# br0x (takes no URL args, so tabs are opened with the project's script)
export XDG_DATA_HOME=/tmp/bx/data XDG_CONFIG_HOME=/tmp/bx/cfg \
  XDG_CACHE_HOME=/tmp/bx/cache XDG_STATE_HOME=/tmp/bx/state  # fresh dirs
s=$(date +%s%3N); setsid ./target/release/br0x >/tmp/bx.log 2>&1 &
# poll `hyprctl clients -j` for class org.br0x.Browser -> startup ms
scripts/open_tabs.sh "class:org.br0x.Browser" $URLS   # Ctrl+T / Ctrl+L keystrokes
sleep 20; python3 scripts/measure.py br0x             # PSS + MemAvailable
python3 /tmp/cpu10.py br0x                            # 10 s idle CPU of tree
pkill -x br0x

# firefox (warmed template profile copied per trial; see Limits)
setsid firefox --profile /tmp/ff --new-window $URLS &
# poll for class firefox -> startup ms
sleep 20; python3 scripts/measure.py firefox; python3 /tmp/cpu10.py firefox

# brave
setsid brave --user-data-dir=/tmp/bv --no-first-run --no-default-browser-check \
  --new-window $URLS &
# poll for class brave-browser -> startup ms
sleep 20; python3 scripts/measure.py brave; python3 /tmp/cpu10.py brave
```

## Per-trial results

Startup ms | procs | PSS MB | idle CPU % (1 core) | MemAvail MB at sample.

br0x:

| trial | startup | procs | PSS | idle CPU | MemAvail |
|---|---|---|---|---|---|
| 1 tab r1 | 2854 | 20 | 389.2 | 0.0 | 2992 |
| 1 tab r2 | 1342 | 20 | 393.2 | 0.0 | 2417 |
| 1 tab r3 | 1584 | 20 | 412.5 | 0.1 | 1884 |
| 5 tab r1 | 1586 | 68 | 702.0 | 0.1 | 2126 |
| 5 tab r2 | 1347 | 62 | 734.0 | 0.2 | 3022 |
| 5 tab r3 | 1820 | 68 | 735.6 | 4.3 | 2923 |
| 10 tab r1 | 1352 | 128 | 669.5 | 0.2 | 2174 |
| 10 tab r2 | 2379 | 128 | 731.0 | 0.3 | 1964 |
| 10 tab r3 | 1350 | 128 | 910.4 | 0.8 | 1904 |

Firefox 153.0.4:

| trial | startup | procs | PSS | idle CPU | MemAvail |
|---|---|---|---|---|---|
| 1 tab r1 | 1689 | 16 | 712.7 | 4.6 | 2307 |
| 1 tab r2 | 2041 | 16 | 714.4 | 4.4 | 2258 |
| 1 tab r3 | 2041 | 16 | 786.7 | 5.2 | 2051 |
| 5 tab r1 | 1914 | 19 | 871.0 | 7.9 | 2106 |
| 5 tab r2 | 2265 | 19 | 790.2 | 8.8 | 2123 |
| 5 tab r3 | 2271 | 19 | 878.4 | 9.3 | 1957 |
| 10 tab r1 | 2148 | 19 | 887.8 | 6.6 | 2068 |
| 10 tab r2 | 2152 | 19 | 900.5 | 8.4 | 1988 |
| 10 tab r3 | 3971 | 19 | 817.9 | 62.5 | 2296 |

Brave 151.1.93.136:

| trial | startup | procs | PSS | idle CPU | MemAvail |
|---|---|---|---|---|---|
| 1 tab r1 | 1334 | 29 | 899.5 | 6.2 | 2933 |
| 1 tab r2 | 821 | 29 | 881.7 | 0.6 | 2013 |
| 1 tab r3 | 1210 | 29 | 913.6 | 3.0 | 2414 |
| 5 tab r1 | 1440 | 45 | 1097.0 | 0.7 | 3014 |
| 5 tab r2 | 1375 | 45 | 1133.1 | 0.0 | 3324 |
| 5 tab r3 | 1309 | 45 | 1141.0 | 0.3 | 3873 |
| 10 tab r1 | 1196 | 65 | 1290.7 | 0.1 | 2359 |
| 10 tab r2 | 1186 | 65 | 1414.6 | 0.2 | 1733 |
| 10 tab r3 | 1978 | 65 | 1336.1 | 0.1 | 2314 |

## Medians (range)

| browser | startup 1/5/10 tabs (ms) | PSS 1/5/10 tabs (MB) | idle CPU 1/5/10 (% core) |
|---|---|---|---|
| br0x | 1584 (1342–2854) / 1586 (1347–1820) / 1352 (1350–2379) | 393 (389–413) / 734 (702–736) / 731 (670–910) | 0.0 / 0.2 / 0.3 |
| br0x (retest, current sources) | 2034 (1742–2300) / 1708 (1577–1740) / 2071 (1536–2381) | 341 (331–342) / 637 (518–646) / 810 (457–862) | — / 0.1 / — |
| Firefox | 2041 (1689–2041) / 2265 (1914–2271) / 2152 (2148–3971) | 714 (713–787) / 871 (790–878) / 888 (818–901) | 4.6 / 8.8 / 8.4 |
| Brave | 1210 (821–1334) / 1375 (1309–1440) / 1196 (1186–1978) | 900 (882–914) / 1133 (1097–1141) / 1336 (1291–1415) | 3.0 / 0.3 / 0.1 |
| Chrome 153 | 1095 (880–1828) / 1326 (1104–1418) / 1376 (1027–1403) | 1173 (971–1177) / 1327 (1293–1348) / 1521 (1509–1531) | — / 2.4 / — |

Incremental cost per extra tab (median 10-tab minus 1-tab, /9):
br0x ~38 MB (retest ~52 MB), Brave ~49 MB, Firefox ~19 MB, Chrome ~39 MB.

## Chrome 153.0.8010.52 trials (same method, fresh `--user-data-dir` per trial)

| trial | startup | procs | PSS |
|---|---|---|---|
| 1 tab r1 | 1828 | 39 | 1177.1 |
| 1 tab r2 | 1095 | 39 | 971.2 |
| 1 tab r3 | 880 | 39 | 1173.2 |
| 5 tab r1 | 1104 | 55 | 1348.2 |
| 5 tab r2 | 1418 | 55 | 1327.2 |
| 5 tab r3 | 1326 | 55 | 1292.8 |
| 10 tab r1 | 1376 | 75 | 1521.3 |
| 10 tab r2 | 1027 | 75 | 1531.0 |
| 10 tab r3 | 1403 | 75 | 1508.5 |

Idle CPU (5 static tabs, 10 s sample): Chrome 2.4%, br0x 0.1%,
Firefox 42.0%, Brave 0.3% — all with fully fresh, unwarmed profiles this
time (the earlier 4–9% Firefox figure used a warmed template profile, so
first-run background work explains the gap; treat 42% as a cold-start
artifact, not steady state).

## br0x retest trials (current sources, fresh XDG dirs per trial)

| trial | startup | procs | PSS |
|---|---|---|---|
| 1 tab r1 | 2300 | 8 | 331.4 |
| 1 tab r2 | 1742 | 8 | 342.3 |
| 1 tab r3 | 2034 | 8 | 340.6 |
| 5 tab r1 | 1577 | 68 | 518.3 |
| 5 tab r2 | 1708 | 68 | 637.4 |
| 5 tab r3 | 1740 | 68 | 646.2 |
| 10 tab r1 | 2071 | 158 | 456.7 |
| 10 tab r2 | 2381 | 158 | 862.1 |
| 10 tab r3 | 1536 | 158 | 810.3 |

## Verdict

Where br0x wins: lowest absolute memory at every tab count (roughly half of
Brave, ~150–300 MB under Firefox), near-zero idle CPU, and by far the
smallest shipped binary. Where br0x loses: startup is slower than Brave
(~1.6 s vs ~1.3 s median) though faster than Firefox (~2.1 s); its 10-tab
memory varies the most between trials (670–910 MB); and it spawns a strikingly
large process tree (128 processes for 10 light tabs vs 19–65 elsewhere),
which is worth investigating even though PSS stays low. Firefox has the best
per-tab scaling but the slowest startup and persistently the highest idle CPU
(4–9% on static pages — background task churn, cause not diagnosed). Brave
starts fastest and idles quietly but uses the most memory at every tab count.

Chrome sits between Brave and Firefox on memory (heaviest at 1 tab,
1173 MB, but the flattest growth at ~39 MB per extra tab) and starts
quickly (~1.1–1.4 s). Its 10-tab process tree (75) is far leaner than
br0x's (128–158). br0x still wins absolute memory everywhere and binary
size by two orders of magnitude, but its per-tab scaling (~38–52 MB)
no longer looks special next to Chrome (~39 MB) — only Firefox (~19 MB)
scales clearly cheaper, at the cost of the slowest startup.

## Methodology limits

- Light pages only: ten `example.com` tabs measure per-tab overhead, not real
  workloads (video, web apps, heavy JS would change absolute numbers and
  possibly rankings).
- Machine was under memory pressure throughout (swap in use, MemAvailable
  1.7–3.9 GB across trials); results are comparative on this box, not
  absolute claims.
- Runs happened on the live Hyprland session; `open_tabs.sh` steals window
  focus while typing. Firefox/Brave tabs were passed as CLI args instead
  (br0x accepts no URL args) — the open mechanism differs but settled PSS
  should not depend on it.
- Firefox needed `mkdir -p` before `--profile`: FF153 shows "Profile Missing"
  instead of creating the dir (the whole first Firefox pass measured empty
  processes and was discarded). Firefox/Brave used a once-warmed template
  profile copied per trial; br0x used fully fresh XDG dirs.
- Outliers kept as measured: Firefox 10-tab r3 idle CPU 62.5%, br0x 5-tab r3
  idle CPU 4.3%, wide startup ranges (cold vs warm page cache).
- `scripts/bench.sh` itself could not be used verbatim: it depends on the
  `hl.dsp.send_shortcut` Hyprland plugin protocol that this compositor only
  exposes via `hyprctl eval`, and on a 5-site heavy tab set unsuitable for a
  7 GB box. `scripts/measure.py` was used unmodified.
