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

## Spherical denoising (development)

The panel's **Auto denoise sphere** button dispatches `sphere360.denoise` through
the same engine/job path as CLI and automation. It estimates noise from bounded
opaque samples and filters on the sphere, including longitude wrap and poles.
The strength slider is saved with UI state; Reset view keeps its setting.

```json
["engine.execute", {"command":"sphere360.denoise", "params":{"strength":0.8}}]
```

Optional parameters: `sigma` (null for automatic, otherwise 0..0.15 in linear RGB)
and `radius_pixels` (0.5..3, default 1.5 equatorial pixels). Unknown fields fail.
The active raster layer must be RGB or grayscale, unmasked, and on a mono full-
sphere 2:1 canvas, without selection/Quick Mask/channel targeting. CMYK/Lab and
packed stereo are not validated finishing workflows. The operation does not move
pixels, change canvas dimensions, change source depth, or edit other layers.

Pixels convert through the document's embedded ICC profile (or its working
profile if untagged) to linear RGB and back. Invalid/mismatched profiles fail.
Alpha and hidden fully transparent samples remain unchanged, and skipped denoising
does not incur a colour round trip. The job stages its edits, supports cancellation,
and commits a single undo step. Source files are not overwritten.

This is conservative **spatial still denoising**. It cannot promise noiseless
texture or replace temporal video restoration. It does not automatically detect
opaque tripods or repair lens parallax. The shared kernel is a pinned first-party
snapshot under `vendor/sphere-core`; see `SOURCE.json` and `ATTRIBUTION.md`.

Working memory includes two linear RGBA float frames (32 bytes per source pixel),
24 latitude-dependent sampling taps per row (576 bytes per row), up to 8 MiB
of native-channel band storage, plus document tiles/history. The core
rejects images over 128 Mi-pixels; it does not silently downscale large panoramas.
Run `cargo run --release -p photocraft-engine --example sphere360_denoise_bench`
for a 33.6 MP end-to-end command timing and byte-exact undo check. This entry
documents the implementation contract; see the current verification report for
tested status rather than interpreting source code as acceptance.

Before the latitude-tap optimization on 2026-10-27, the release example processed an 8192 x 4096 RGBA16 procedural
image (33.6 MP), automatically estimating sigma 0.029655 from 32,768 sampled
blocks. Denoising applied, dimensions stayed unchanged, and undo restored every
native source byte. Command time was 264.03 seconds with two Rayon threads while
native compilation was active on an i7-6820HK. This is one loaded-host measurement,
not a speed comparison or visual-quality acceptance. Linear frames used 1 GiB
of storage plus native bands, document/history and other process allocations.
The ignored suite artifacts `photocraft-denoise-bench-v1.json` and
`photocraft-denoise-bench-run-v1.json` record output hashes and run conditions.

## Supplied clean-plate pole repair

The 360 workspace now has a collapsible **Nadir / zenith repair** section. Open
a rectilinear repair image in another tab, select its intended raster layer,
return to the panorama and choose that image in the source selector. Set its
actual horizontal field of view, the pole, rotation, cap radius and feather.
**Inspect pole** points the preview toward the cap; **Apply pole repair** applies
the supplied image as one undoable edit. This section is under native acceptance
testing; implementation alone is not release approval.

The command is `sphere360.patchPole`:

```json
{"donor_document":42,"horizontal_fov":90,"patch":{"pole":"nadir","radius_degrees":20,"feather_degrees":5,"yaw":0}}
```

`donor_document` is the open document's identity, not its tab index or a file
path. The active donor raster's raw pixels are used; its layer opacity, blend
mode, other layers and selection are not composited. Apply masks to a copy first.
The target must be a 2:1 RGB/grayscale raster with no selection or layer masks,
and pixel/transparency locks must allow editing. Source and target profiles are
converted independently through linear RGB; native target depth/profile and
canvas dimensions remain intact. Partial-alpha donors composite over the target,
and pixels outside the cap remain exact. The complete cap must fit inside the
donor's projection, including the bilinear footprint.

Background jobs capture the donor pixels when started. Later edits to another
tab do not change the pending repair. Cancellation discards the staged result.
The shared pixel writer avoids rewriting unchanged bands. No original file is
saved by this command; normal Save/Export remains a separate action.

This workflow does not extract a retouching view yet, find opaque tripods or
drones, align arbitrary photographs, reconstruct missing scenery, or track a
repair through video. Those remain separate requirements. A wrong FOV or poorly
aligned plate can produce visible mismatch and requires visual review.

Run the 33.6 MP release command benchmark with
`cargo run --release -p photocraft-engine --example sphere360_denoise_bench -- --pole`.
It verifies output change and byte-exact undo; it does not certify image quality.

The first 2026-10-27 release run completed a 20° nadir repair with 5° feather on
an 8192×4096 RGBA16 procedural panorama in 36.39 seconds. The donor was a 512×512
white rectilinear plate with 90° horizontal FOV. Dimensions remained unchanged,
output pixels changed, and undo restored every native source byte. Two Rayon
threads were used while other compilation was active on the i7-6820HK. There is
no controlled speed comparison. Linear source/output frames used 1 GiB, the donor
4 MiB, plus native bands capped at 8 MiB and document/history allocations. A future
cap-only conversion path could avoid allocating and converting the full sphere;
that optimization is not implemented. This benchmark started before subsequent
TLS dependency updates and is image-command evidence, not patched-app acceptance.

## Previous workspace verification

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


## Pole workspace interaction check � 2026-10-28

Six focused native UI tests and all-target native lint pass with the patched TLS
lockfile; menu parity remains 631/631. Seven pole/finishing engine integration
tests pass. The full command stress test still fails in `file.printers` and
`file.printOneCopy` with the Windows printer-service timeouts described above.

A fresh 1440 x 1100 offscreen interaction check created a transparent 1024 x 512
RGBA16 panorama and a separate 256 x 256 procedural blue donor. Pointer clicks
opened Nadir / zenith repair, selected that donor, scrolled the controls and
pressed Apply pole repair with background jobs enabled. The final capture shows
the blue feathered nadir, unchanged canvas dimensions and one 360 Pole Repair
history entry. It used the Microsoft Basic Render Driver/DX12; this is control
acceptance, not a real-scene stitch, GPU performance or headset test.

The workspace now opens taller so the main editing controls are immediately
visible. Smaller displays retain scrolling. The snapshot harness accepts
`--scroll-at X,Y --scroll-y N` to verify controls below the fold before a settled
pointer click. Native build/lint and the UI wasm check pass; wasm retains six
existing dead-code warnings. The canonical suite's new bounded pole API is not
yet vendored into this editor, whose pole command still converts full frames.
