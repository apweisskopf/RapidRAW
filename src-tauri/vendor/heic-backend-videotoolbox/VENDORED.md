# heic-backend-videotoolbox (vendored)

Copied from https://github.com/imazen/heic at `dbf35c02b349f108e06f99a7f459b7e6a5299173`
(the `heic` revision pinned in `src-tauri/Cargo.toml`) and wired in through `[patch]`.
Workspace-inherited manifest fields were replaced with their literal values.

Changes in `src/imp.rs`:

- Request full-range or video-range output from VideoToolbox to match the bitstream's
  `video_full_range_flag`. Upstream always asked for video range, so VideoToolbox
  compressed full-range sources (every iPhone photo) into 16..235 while the frame stayed
  tagged full range, which made every image low-contrast.
- Unpack 10-bit (P010) samples from the high 10 bits (`>> 6`). Upstream masked the low
  10 bits, which produced noise for any 10-bit HEIC.
- Include the range in the session cache key.

Drop this directory and the `[patch]` entry once upstream has equivalent fixes.
