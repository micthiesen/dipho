# 04 — Ingest/Alignment Stack (state of the art, mid-2026)

Research date: 2026-06-10. Critical question: how does dipho get **phoneme-level
timestamps** (needed to derive diphone units)?

## Findings

### WhisperX (m-bain/whisperX)

- **Actively maintained.** Latest release May 25, 2026; ~22k stars, 110+
  contributors; BSD-2-Clause; on PyPI as `whisperx` (3.7.x line as of early
  2026). Known community pain point: it pins a heavy dependency set
  (faster-whisper, ctranslate2, pyannote, torch) and causes version conflicts
  in shared environments — isolate it in its own venv/uv environment.
- **Alignment output is word-level, with optional CHARACTER-level — not
  phoneme-level.** Verified in `whisperx/alignment.py` on `main`:
  - `align()` returns `{"segments": aligned_segments, "word_segments": word_segments}`.
  - Each segment dict is `{"start", "end", "text", "words": [...], "chars": None}`;
    each word has `{"word", "start", "end", "score"}`.
  - Passing `return_char_alignments=True` populates a `"chars"` list per
    segment (per-character start/end). Characters ≠ phonemes (English
    orthography), so this is **not** usable directly for diphone extraction.
  - Alignment models are wav2vec2 **ASR letter/character models**
    (torchaudio `WAV2VEC2_ASR_BASE_960H` for English, HF
    `jonatasgrosman/wav2vec2-large-xlsr-53-*` for other languages) — CTC over
    graphemes, not phones. The README's "phoneme-based ASR" framing is
    misleading for English: the English label set is letters.
- **Conclusion: WhisperX alone cannot give dipho phonemes.** It gives a
  high-quality transcript + accurate word timestamps + (via bundled pyannote)
  speaker labels. A second pass is required for phones.

### Montreal Forced Aligner (MFA) — the phoneme pass

- **Current version 3.3.9 (Feb 2, 2026); actively maintained**
  (MontrealCorpusTools/Montreal-Forced-Aligner; docs at
  montreal-forced-aligner.readthedocs.io).
- **Install story: conda-forge is the supported path.** Recommended:
  `conda create -n aligner -c conda-forge montreal-forced-aligner` (or mamba).
  The conda-forge package itself is now `noarch` Python, but it pulls
  arch-specific Kaldi/kalpy binaries from conda-forge. Pip install of MFA
  exists on PyPI but Kaldi binaries and pynini (G2P) still must come from
  conda — **not cleanly uv/pip-installable**. Practical consequence for dipho:
  run MFA in its own conda/micromamba env, invoked as a subprocess CLI
  (`mfa align ...`) by the Python sidecar; don't import it.
  - Apple Silicon: the docs don't call out arm64 explicitly; conda-forge
    Kaldi/kalpy feedstocks ship osx-arm64 builds and MFA is widely used on
    M-series Macs via conda. (Not verified by me on M4 specifically — flag
    for a smoke test.) MFA is CPU-only (Kaldi GMM-HMM), which is fine on an
    M4 Max — alignment is fast relative to transcription.
- **Models:** `mfa model download acoustic english_mfa` (or `english_us_arpa`)
  plus matching dictionary `english_mfa` / `english_us_arpa`. ARPA models give
  ARPAbet phones (AA, AE, ... — convenient, CMUdict-compatible); `english_mfa`
  uses an IPA-flavored phone set with better accuracy reputation. OOV words
  handled via G2P models (`english_us_arpa` G2P) — important for YTP source
  material full of names/slang.
- **Output:** Praat **TextGrid per audio file with a word tier and a phone
  tier** (interval start/end in seconds). Trivially parsed with `praatio` or
  `textgrid` PyPI packages into dipho's SQLite phoneme table; diphones are
  then derived as midpoint(phone[i]) → midpoint(phone[i+1]).
- **Accuracy reputation:** de facto standard in phonetics/linguistics research
  for a decade (Kaldi triphone GMM-HMM + speaker adaptation); phone boundaries
  typically within 10–20 ms of human annotation on clean read speech. It is a
  *forced* aligner: it needs a transcript, which WhisperX provides.
- **Pipeline composition (verified pattern, widely used in TTS dataset prep):**
  1. WhisperX transcribes + gives segment/word times.
  2. Cut audio into utterance chunks at segment boundaries (MFA aligns much
     better on short utterances than on a 2-hour file; also lets bad segments
     fail in isolation).
  3. Write `chunk.wav` + `chunk.txt` (or .lab) pairs into an MFA corpus dir.
  4. `mfa align corpus_dir english_mfa english_mfa out_dir`.
  5. Parse TextGrids; offset phone times by chunk start; store words+phones.
  - Caveat: MFA can fail/garbage-align chunks where Whisper hallucinated text
    (music, noise). Use MFA's per-utterance alignment scores and Whisper's
    word confidence to flag/drop bad regions.

### Alternatives (one paragraph each)

- **torchaudio forced-alignment API (`torchaudio.functional.forced_align` +
  `MMS_FA` bundle).** A pure-PyTorch CTC Viterbi aligner; MMS_FA is a wav2vec2
  model trained on 23k hours / 1100+ languages, label set is romanized chars.
  Status note: torchaudio entered a **maintenance phase at 2.8** and
  `forced_align` was slated for removal in 2.9, but per the migration tracker
  (pytorch/audio#3902) and the 2.10 release (Jan 2026), `forced_align` was
  **preserved after user feedback**. It runs on MPS/CPU. To get *phones* you'd
  feed it phone-level tokens from a phoneme CTC model (below) — it's the
  alignment engine, not a phoneme solution by itself. Viable plan-B building
  block; slight platform risk given maintenance mode.
- **wav2vec2 phoneme models (`facebook/wav2vec2-lv-60-espeak-cv-ft`).** CTC
  over **espeak IPA phone tokens** (Xu et al. 2021, zero-shot cross-lingual
  phoneme recognition). Recipe: phonemize the WhisperX transcript with
  `phonemizer`/espeak-ng → phone token sequence → CTC forced alignment
  (torchaudio `forced_align` or own Viterbi) against this model's emissions →
  phone timestamps. Fully pip/uv-installable, GPU/MPS-capable, no conda.
  Downsides: espeak IPA phone set needs mapping to dipho's canonical set;
  20 ms CTC frame quantization; boundary accuracy of CTC aligners is good but
  CTC's peaky behavior makes boundaries less precise than HMM aligners like
  MFA; English model quality is fine but less battle-tested for boundary work.
- **ctc-forced-aligner (MahmoudAshraf97).** Thin, pip-installable wrapper
  around the torchaudio CTC alignment math using `MahmoudAshraf/mms-300m-1130-forced-aligner`;
  ~5x less memory than the raw torchaudio API; emits sentence/word/**char**
  level JSON. Same grapheme limitation as WhisperX — chars not phones — unless
  you drive it with a phonemized text + phoneme model. Useful reference
  implementation; not a phoneme answer out of the box.
- **whisper-timestamped (linto-ai).** Word timestamps via DTW over Whisper
  cross-attention (no separate alignment model). Word-level only, generally
  less precise than wav2vec2 forced alignment for boundary work; still
  maintained. Not interesting for dipho given WhisperX already covers
  word-level better; no phoneme capability.

### Apple Silicon (M4 Max) practicalities

- **faster-whisper / CTranslate2: still no Metal/MPS backend as of mid-2026**
  (open since SYSTRAN/faster-whisper#515). On Apple Silicon it runs CPU
  (int8 via Accelerate). Usable, but leaves the GPU idle.
- **mlx-whisper** (ml-explore): Metal/unified-memory native, fast on M-series,
  supports large-v3 / large-v3-turbo and word_timestamps (DTW-based; a
  memory-growth issue with word_timestamps on long chunked audio was reported,
  ml-explore/mlx-examples#1254). Strong choice for transcription speed if
  word times come from a separate aligner anyway.
- **whisper.cpp / WhisperKit**: whisper.cpp has full Metal support; WhisperKit
  (Argmax, v1.0 May 2026) runs CoreML/ANE — both excellent but Swift/C++
  surface; less convenient from a Python sidecar than mlx-whisper.
- **pyannote.audio: yes, runs on MPS** — `pipeline.to(torch.device("mps"))`
  is the documented pattern. Current version **4.0.4 (Feb 2026)**, Python
  ≥3.10, pure PyTorch (no onnxruntime since 3.1). The open pipeline is
  `pyannote/speaker-diarization-community-1` (4.x) or
  `speaker-diarization-3.1` (3.x); **HF-gated**: must accept conditions and
  pass an HF token. MIT-licensed code; a paid hosted "precision-2" tier exists
  but is not required. Note: WhisperX currently pins pyannote 3.x — another
  reason to keep env isolation or call diarization separately.
- **Ingest is batch/offline** for dipho, so CPU-only stages (MFA,
  faster-whisper fallback) are acceptable; they just lengthen ingest, not
  interactive use.

## Code pointers

- WhisperX align() schema: https://github.com/m-bain/whisperX/blob/main/whisperx/alignment.py
  (`align()` → `{"segments", "word_segments"}`; `return_char_alignments` flag;
  `DEFAULT_ALIGN_MODELS_TORCH` / `DEFAULT_ALIGN_MODELS_HF` dicts)
- WhisperX releases: https://github.com/m-bain/whisperX/releases (latest 2026-05-25)
- MFA docs/install: https://montreal-forced-aligner.readthedocs.io/en/latest/installation.html ;
  conda-forge: https://anaconda.org/conda-forge/montreal-forced-aligner (3.3.9, 2026-02-02)
- MFA pretrained models: https://mfa-models.readthedocs.io / `mfa model download`
- torchaudio CTC FA API: https://docs.pytorch.org/audio/stable/generated/torchaudio.functional.forced_align.html ;
  future-of-torchaudio tracker: https://github.com/pytorch/audio/issues/3902
- Phoneme CTC model: https://huggingface.co/facebook/wav2vec2-lv-60-espeak-cv-ft
- ctc-forced-aligner: https://github.com/MahmoudAshraf97/ctc-forced-aligner
- CTranslate2 Metal gap: https://github.com/SYSTRAN/faster-whisper/issues/515
- pyannote: https://github.com/pyannote/pyannote-audio (4.0.4);
  https://huggingface.co/pyannote/speaker-diarization-community-1 (gated, HF token)
- mlx-whisper: https://github.com/ml-explore/mlx-examples/tree/main/whisper
  (word_timestamps memory issue: ml-explore/mlx-examples#1254)

## Recommendation

**Primary stack for the Python sidecar (two isolated environments):**

1. **Transcription:** `mlx-whisper` + `large-v3-turbo` on the M4 Max GPU
   (fast, Metal-native). Keep `whisperx` (faster-whisper CPU int8) as the
   portable code path — same downstream interface either way.
2. **Word alignment:** WhisperX `align()` (wav2vec2) over the transcript →
   word start/end + scores → SQLite words table (FTS5).
3. **Phoneme alignment (the diphone source): MFA 3.3.9**, `english_mfa`
   acoustic model + dictionary + G2P for OOVs. Run in a dedicated
   micromamba env, invoked as a subprocess on per-segment chunks cut at
   WhisperX segment boundaries; parse TextGrid phone tiers with `praatio`;
   derive diphones midpoint-to-midpoint. MFA is the accuracy gold standard
   for phone boundaries, which is exactly what diphone cut points need.
4. **Diarization:** pyannote.audio 4.0.x `speaker-diarization-community-1`
   on MPS (HF token required, one-time gating acceptance). Run it directly,
   not through WhisperX's bundled integration (WhisperX pins pyannote 3.x).
5. **Prosody:** librosa `pyin` (or torchaudio on MPS) for f0 + RMS frames;
   no exotic dependency needed.

**Fallback if MFA proves painful** (conda friction, arm64 breakage, bad
alignments on noisy YTP sources): pure-pip CTC phoneme pipeline —
`phonemizer` (espeak-ng) on the WhisperX transcript →
`facebook/wav2vec2-lv-60-espeak-cv-ft` emissions → `torchaudio.functional.forced_align`
(confirmed retained in torchaudio 2.10). Same TextGrid-equivalent output
shape; expect somewhat softer boundary precision (CTC peakiness, 20 ms
frames) — mitigated post-MVP by dipho's planned DSP cut refinement
(zero-crossing snap / spectral-flux minimization), which makes the fallback
genuinely acceptable. Design the SQLite phoneme schema aligner-agnostic
(store aligner id + per-phone confidence) so both backends can coexist.
