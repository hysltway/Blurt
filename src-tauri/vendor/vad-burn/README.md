# Vendored vad-burn

This directory contains the FSMN-VAD portion of `vad-burn` 0.1.3 from
<https://github.com/di-osc/vad-burn>, licensed under MIT.

Blurt removes the FireRed VAD, Python, and ModelScope download modules. The
C++ `kaldi-fbank-rust-kautism` frontend is replaced with the vendored pure
Rust `kaldi-native-fbank` frontend to keep the Windows CRT linkage consistent.
