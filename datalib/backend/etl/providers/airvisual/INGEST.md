# AirVisual ingest

The ingest step of an `airvisual` group reads an IQAir **AirVisual Pro**'s
own history files into a doltlite raw store:

```
<data_root>/<group>/ingest/entities.doltlite_db
  airvisual_devices            one row per device: name, serial, model, timezone, last sample
  airvisual_samples            one row per logged line, a REAL column per measurement
  airvisual_unplaced_samples   lines logged before the clock was set
  ingested_files               which history files have been read, by content hash
```

The one method is `export`: the folder the Pro serves over Samba
(`smb://<ip>/airvisual`, user `airvisual`, password shown on the device
under *Settings › Network › Access Pro data*), mounted, or a copy of it.
Every `*_AirVisual_values.txt` under the path is read, archive folders
included.

## Why the device's own files, and not IQAir's cloud

The Pro keeps every sample it ever took on its own flash — IQAir says
five years — and the share is the only free route to that history. The
cloud alternatives were measured on 2026-09-14 and are recorded in
[`docs/dev/plans/airvisual.md`](../../../../../docs/dev/plans/airvisual.md):
the no-credential device API (`device.iqair.com/v2/<id>`) keeps only
trailing windows and only for *published* devices, and the dashboard's
CSV export is refused for a private device on the free plan. Reading
the share never touches IQAir's servers, so no plan, no retention
window and no endpoint to be turned off.

## What is on the share

Measured on one Pro (firmware `1.1937`, system `KBG66F85`) on
2026-09-14: 31 history files, 8.5 MB, 125,487 lines from 2025-01-06.

```
202607_AirVisual_values.txt         the current months, at the root
202608_AirVisual_values.txt
202609_AirVisual_values.txt         still being written
archive1/ … archive4/               one per clock change (see below)
  YYYYMM_AirVisual_values.txt
  corrupt_202509_AirVisual_values.txt
  restored_202509_AirVisual_values.txt
  197001_AirVisual_values.txt       lines logged before NTP set the clock
history.txt                         one JSON line per archive: the clock before and after
latest_config_measurements.json     the device's name, serial, settings and current reading
logs/  update/                      empty
```

**The device starts a new `archiveN/` every time its clock jumps** —
each DST change and the move between time zones, on this unit — and
`history.txt` records the before/after. The files inside are the same
shape as the ones at the root; the step reads them all.

**A line is `;`-separated and ends in a `;`.** Two header variants:

```
Date;Time;Timestamp;PM2_5(ug/m3);AQI(US);AQI(CN);PM10(ug/m3);PM1(ug/m3);Outdoor AQI(US);Outdoor AQI(CN);Temperature(C);Temperature(F);Humidity(%RH);CO2(ppm);
Date;Time;Timestamp;PM2_5(ug/m3);PM10(ug/m3);PM1(ug/m3);Temperature(C);CO2(ppm);          restored_* only
```

`Timestamp` is epoch seconds and is the only time the parser uses:
`Date`/`Time` are local to whatever zone the device was in and change
format between files (`2026/09/01` at the root, `9/1/2025` in a
`restored_` file). A blank cell is the sensor being off — the
`corrupt_` files are ~95% blank-sensor lines carrying only the followed
outdoor station's index — and `-1` is a sentinel; both become NULL.
`Temperature(F)` is `Temperature(C)` converted and is kept in the
payload only. A column the parser does not know is kept in the payload
and warned about once per file (`airvisual_unknown_column`).

**The month being written ends in a block of NULs** the device has
reserved; the parser trims them. A line cut short by a concurrent write
costs that line (`airvisual_bad_line`, counted as `bad_lines`), not the
file, and the file is re-read next run because its hash moved.

**Sampling is uneven by design.** The interval follows the Pro's
sensor-mode schedule: on this unit 10 s, 5 min and 15 min all appear,
in runs. Thirteen timestamps appear in two files (archive boundaries;
`corrupt_`/`restored_` pairs), which the primary key absorbs — files
are read in path order, so the later path wins, and `source_file` on
the row says which.

**Pre-clock lines.** 27 lines in `197001_…` carry timestamps of a few
seconds to a few days since a boot, not since 1970. Every pre-NTP boot
counts from zero again, so they cannot be keyed on their timestamp; they
go whole into `airvisual_unplaced_samples`, keyed
`{device}#{file}#{line}`, which is stable because the device only
appends.

## Incrementality

`fsscan` + `file_checkpoint`, the same pair `contacts`' `.vcf` mode and
`google_takeout` use: the host-wide fingerprint cache decides which
files to re-hash (`(mtime, size, inode, dev)` unchanged → reuse the
hash), and `ingested_files` decides which to re-parse (hash unchanged →
skip). Measured over the SMB mount: **17 s cold for the 31 files, 3.9 s
warm with 30 skipped** — the month being written is re-read whole
every run, which is a few thousand lines. macOS's SMB client
synthesizes inodes; on a filesystem without them the cursor falls back
to `(mtime, size)`, and the worst case after a remount is a re-hash of
8.5 MB, never a re-ingest.

Reset (`--reset-and-redownload`) empties the three tables and the
cursor; the next run re-reads everything from the share.

## Naming the device

`export.device` is the row key for everything the device logged.
Left out, it is `settings.node_name` from the folder's
`latest_config_measurements.json` — the name shown on the device and in
the app — and the step fails if neither is there (a copied folder
without the JSON). Renaming re-keys the history; the render notices
samples under a name no device row carries and says so.
