# Third-party notices

The suite retains the upstream applications' separate licenses. These notices
cover additional references used by the suite's original Rust camera reader and
factory projection implementation; no vendor SDK is bundled.

## raw360-formats

Source: https://github.com/freeKC/raw360-formats

Reference revision: `a14f0d81c7ea027c40db6d89afa4f229a716d0a8` (2026-10-23).
The Insta360 trailer layout, field meanings, crop/track mapping, Mei projection,
and lens-pose conventions in `docs/insta360.md` and `docs/stitching-math.md` informed
`sphere-camera` and `sphere-core::factory`. The Rust parser is independently
implemented with bounded records and fail-closed support for the tested X4 mode.

## GyroView

Source: https://github.com/bubo-squared/GyroView

The mirrored-roll and camera-body conventions are attributed by raw360-formats
to GyroView. Retained here for provenance of those conventions. No GyroView
application code or assets are bundled.

## MIT notices for the above references

Copyright (c) 2026 FreeKC

Copyright (c) 2026 Bubo²

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
