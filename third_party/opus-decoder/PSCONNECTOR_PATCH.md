# opus-decoder 0.1.1 (patched)

Copy of [opus-decoder 0.1.1](https://crates.io/crates/opus-decoder) (MIT OR Apache-2.0,
https://github.com/TadeuszWolfGang/Rusopus), used through `[patch.crates-io]` in the workspace manifest.

Only change: `flat_fft_forward` in `src/celt/kiss_fft.rs` looks its twiddle factors up in a per-size table
instead of calling `sin_cos` for every element of every transform. That call was most of the audio
decoding cost (about 8 % of one core for a stereo 48 kHz stream). Remove this directory and the patch entry
once upstream has an equivalent fix.
