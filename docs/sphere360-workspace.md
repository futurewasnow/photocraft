# 360 workspace (development fork)

Open a **monoscopic 360 × 180-degree equirectangular panorama**, then choose
**View → 360° Workspace**. The panel requires a 2:1 canvas; invoking it explicitly
selects that interpretation. Dimensions alone do not detect projection or stereo.
Packed stereo and VR180 images need separate modes and are not supported here.

Drag the preview to look around, or use the yaw, pitch and field-of-view sliders.
Front, Seam, Nadir and Zenith frame the corresponding directions. These controls
change only the view, leaving the document and its undo history untouched.
After a heading edit starts in the background, the view resets only when it
succeeds, provided the document and horizontal viewing angle are unchanged. Cancellation
or failure leaves the view unchanged.

The display preview uses a colour-managed composite of the committed document,
reduced to at most 2048 pixels wide. It is not a full-resolution detail inspection
or export. Native builds compute one thumbnail in a background worker at a time;
Snapshot/revision/display keys discard stale results after edits, switching tabs,
or reopening a native project that retains the same saved document ID.
View changes reuse that composite. The WebAssembly path computes a bounded preview
synchronously on a cache miss. In-progress adjustment/stroke previews are not included
until the edit is committed.

## Lossless heading edits

**Set heading as front** moves the selected yaw to the centre by shifting whole
columns to the nearest source pixel. It applies yaw only, not the preview pitch or
field of view. **Move seam 180°** swaps the two panorama halves, placing the previous
left/right boundary at the centre for inspection or repair. Both operations affect
only the active raster layer within the panorama canvas and make one undoable,
journaled engine edit. Stored pixels outside the canvas remain unchanged.

Native channel values and bit depth are retained without colour conversion or
interpolation. Masks, selected regions, alpha-channel targeting and locked pixels
are rejected instead of silently applying a partial spherical edit. Apply/remove
attached masks deliberately before using this initial layer-only operation.
For a locked Background layer, use **Layer → New → Layer from Background** first,
then remove any remaining position or transparency locks deliberately.
The native app uses its existing cancellable background-job mechanism. The engine
shifts in row bands with at most 8 MiB of temporary samples, checking cancellation
between reads and writes and reporting progress. The document's copy-on-write
tiles and undo history use additional memory. Other layers remain in place.

Ordinary painting, healing and planar filters do not become sphere-aware merely
because this panel is open. Wrap-aware editing tools, project projection metadata,
pole repair, stereo/VR180 and spherical export metadata remain separate work.

## Automation

The existing control protocol can invoke the same shell commands:

```json
["ui.menu.invoke", {"id":"view.sphere360"}]
["ui.menu.invoke", {"id":"view.sphere360.look", "params":{"yaw":180,"pitch":0,"fov":80}}]
["engine.execute", {"command":"sphere360.yaw", "params":{"columns":1024}}]
```

`view.sphere360` toggles the panel. `view.sphere360.look` opens it and atomically
patches any of `yaw` (-180..180), `pitch` (-90..90) and `fov` (30..120), in degrees.
Unknown fields and invalid numbers fail without partially changing the view.
Serialized UI state includes `sphere360`; older state gets the default closed view.
`ui.inspect` reports the current view under `sphere360` without changing it.

`sphere360.yaw` accepts only an integer `columns`. Positive values move pixels right;
negative values move them left. Large integers wrap modulo the canvas width. Use
`edit.undo` / `edit.redo` through the normal command path.

## Verification scope

Tests cover directional sampling at the front, seam and poles; premultiplied
transparency; malformed geometry; atomic view commands, menu-cache invalidation and state round-trip;
stale-preview rejection; native 8/16/32-bit shifts; cancellation; masks; position
locks; band boundaries; undo/redo and native-document save/reopen.
UI snapshots use an original procedural panorama, not personal photography.
On 2026-10-26, all nine focused 360 tests passed. The broader library runs
reported 2,178 engine and 3,803 UI passes, with 14 and 18 intentionally ignored
tests respectively. Five engine printing tests and four UI printer-dialog tests
stalled on the Windows printer service and were explicitly excluded from the
successful runs. Formatting, all-target clippy with warnings denied, and layering
passed (31 crates, zero violations). WebAssembly checks passed for 24 crates and
the optional HEIF codec feature. The menu-parity generator ran under the test
profile to reuse compiled UI artifacts: 631/631 menu entries, checklist unchanged.
Engine integration targets passed 289 tests with six ignored. UI integration
reported 126 passes, two failures, one ignored and four printer-paper tests
excluded. Both failures reproduced serially on the default DX12 software adapter;
diagnostics identified a lost device and a `WrongSubmissionIndex` readback error.
All three related full-resolution preview tests passed on the GTX 1070/Vulkan
with `PHOTOCRAFT_TEST_HARDWARE_GPU=1`. This opt-in test setting fails if no physical
GPU canvas is available. It does not change the default tests or erase the
software-renderer failures.
Offscreen 1440 × 900 snapshots of the seam, nadir and zenith were inspected using
the procedural panorama. The wrap was continuous, poles had no visible holes,
and the locked-background explanation and unlocked controls were visible. These
captures used the Microsoft Basic Render Driver (DX12 software adapter), not a
hardware-performance or physical-headset test. A settled pointer click on
**Set heading as front**, with background jobs enabled, shifted the panorama and
reset yaw from 90 to 0 while retaining the same visible direction. The screenshot
harness now supports `--settle-before-click-ms` for asynchronous preview layout.
The full command panic-hunt failed on two Windows printer-service timeouts:
`file.printers` and `file.printOneCopy` each exceeded four seconds twice, even in
dry-run mode. No other command was reported as failing. This is a failed gate,
not a pass with exclusions. Printer-service and software-renderer gates remain
unresolved; the development fork is not a release candidate.

Run `cargo run --release -p photocraft-engine --example sphere360_bench` for a
33.6-megapixel, 16-bit procedural comparison against the former full-buffer yaw.
It reports three pairs of timings and verifies every output pixel using a hash
of the native interleaved bytes. Temporary sample storage is 512 MiB for that
reference and at most 8 MiB for the banded command, excluding document tiles,
history and allocator overhead. Timings must be measured, not inferred from this
memory reduction.

Measured on 2026-10-26 on the i7-6820HK, with other builds/tests stopped, all three
33.6 MP RGBA16 runs matched the reference's native-byte hash. Median command time
was 0.8947 seconds for the banded command versus 0.9378 seconds for the full-buffer
reference. Individual pairs (reference → banded seconds) were 0.9144 → 0.8845,
0.9378 → 0.8947 and 0.9694 → 0.9111. The temporary sample buffer is 64 times smaller;
this is not a measurement of total process memory. The small timing difference
from three runs on one workstation is not a general speed guarantee.
