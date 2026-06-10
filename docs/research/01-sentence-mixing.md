# Prior art: pop123123123/sentence-mixing + CLI_sentence_mixing

Research date: 2026-06-10. All code quotes verified against `master` of both repos.

## Findings

### What it is

- `sentence-mixing` (Python library, 8 stars, GPL-ish hobby project by two French devs):
  takes YouTube URLs of *subtitled* videos, force-aligns the subtitles to audio with
  Montreal Forced Aligner (MFA), builds an in-memory phoneme corpus, and ranks
  candidate phoneme assemblies ("combos") for a typed target sentence.
- `CLI_sentence_mixing`: thin interactive REPL over the library plus final video
  assembly via moviepy.
- Supported languages: fr, en, de — via pronunciation dictionaries in a sibling repo
  `nbusser/SM-Dictionaries`.

### Ingest / alignment pipeline (`logic/video_processing.py`)

End to end, per video:

1. `yt_dlp` downloads `bestaudio` → wav, plus subtitles (`writesubtitles` +
   `writeautomaticsub`, `.vtt` in the configured language).
2. Each VTT caption becomes a `SubtitleLine`; each line is cut out of the wav into its
   own `<hash><i>.wav` + a `.lab` text file (`_split_audio_in_files`) — MFA's corpus
   format.
3. MFA is shelled out via `os.system`:
   `command = f'{align_exe} "{folder}" "{dict_path}" "{trained_model}" "{out_dir}" -s 1 --quiet'`
   — this is the **MFA 1.x CLI** (`bin/mfa_align` from the 1.1.0 Beta 2 binary release).
   Telling detail: `# MFA returns a unexpected db error` with `assert ret == 0`
   commented out — alignment was already flaky for them.
4. Per-line `TextGrid` output is parsed (`textgrid` lib): tier 0 = words, tier 1 =
   phones. Builds `AudioWord` and `AudioPhonem` objects with absolute timestamps
   (caption start offset added back). `''` transcripts normalized to `"sp"` (silence);
   `sil`/`spn` discarded.
5. The whole object graph (`Video → SubtitleLine → AudioWord → AudioPhonem`) is
   pickled (`serialize.save/load`) so re-runs skip alignment.

There is **no database**. The "index" is a dict built lazily in
`logic/global_audio_data.py`: `get_transcription_dict_audio_phonem()` maps
`phoneme transcription → [AudioPhonem, ...]` over the whole corpus (deterministically
shuffled with a seeded RNG). The indexed unit is the **whole phoneme** — there is no
diphone concept and no mid-phoneme cutting; segments are concatenated at MFA's phoneme
boundaries.

### Target sentence processing (`logic/text_parser.py`, `model/target.py`)

Pure dictionary lookup: word → token → phoneme list from the SM-Dictionaries `.dict`
file; `num2words` for digits. Consequences (documented in their README):

- OOV words raise `KeyError` — user must edit the dictionary.
- Words with multiple pronunciations raise `TokenAmbiguityError` — flatly unsupported
  ("it is for the moment forbidden to write down word 'Est'").
- Punctuation becomes `<BLANK>` pseudo-words (pauses); a `<BLANK>` is interleaved
  between every pair of target words.

There is no G2P model and no ASR — only pre-existing YouTube subtitles.

### The solver (`logic/analyze.py`, `model/choice.py`, `model/association.py`)

The docstring of `get_n_best_combos` states the architecture plainly:

> Scores are assigned in three different steps:
> - Step 1: the audio phonems are scored individually
> - Step 2: the target phonem and audio phonem associations are scored individually
> - Step 3: an association is scored comparatively to all previous chosen associations

This is exactly the unit-selection decomposition: **step 1+2 ≈ target cost, step 3 ≈
join cost**, though they never use those words.

**Candidate generation** (`AudioData.get_candidates`): for a target phoneme, candidates
are *only* audio phonemes with the **identical transcription**, sorted by step-1+2
score. No substitution matrix, no phonetic-similarity fallback — if a phoneme never
occurs in the corpus, `PhonemError` is raised and the user is told to add videos.

**Step 1** (`logic/analyze_step_1.py`, `score_length`): duration malus only.
Quadratic penalty for phonemes shorter than `MINIMAL_PHONEM_LENGTH = 0.1 s`, or longer
than `MAXIMAL_CONSONANT_LENGTH = 0.25 s` / `MAXIMAL_VOWEL_LENGTH = 0.5 s` (caps at
1000). Effectively prunes MFA misalignments and droned vowels.

**Step 2** (`model/association.py`, class `Association`): per (target_phonem,
audio_phonem) pair, cached via `functools.lru_cache` on a free-function
`association_builder`:

- `SCORE_SAME_TRANSCRIPTION = 200` (always earned, given exact-match candidates).
- `SCORE_SAME_AUDIO_WORD = 200` if target word and source word are homophones.
- `_step_2_word_sequence_score`: length of the *homophone word run* starting here ×
  `RATING_LENGTH_SAME_WORD = 100`.
- `_step_2_phonem_sequence_score` (+ a `reverse=True` backward variant): length of the
  *identical contiguous phoneme run* between target and source × 
  `RATING_LENGTH_SAME_PHONEM = 80`.
- For `<BLANK>` targets: silence rating (`1 - normalized RMS`, squared) ×
  `SCORE_SILENCE_AMPLITUDE = 200` plus a soft-rectangle duration score
  (tanh window over 0.1–0.2 s) × `SCORE_DURATION = 400`.

The dominant signal is "**how long a contiguous chunk of source can I keep using from
here**" — their core quality insight, since real coarticulation comes for free inside
a contiguous span.

**Step 3** (`logic/analyze_step_3.py`, weights applied in
`Choice.compute_child_step_3_score`): the join cost, computed against the path of
previously chosen units:

- *Spectral continuity* (vowels only): `scipy.signal.csd` cross power spectral
  density between the candidate vowel's waveform and the **last chosen vowel's**
  waveform, `log(|Pxy|.sum())`, × `RATING_SPECTRAL_SIMILARITY = 50` (after resampling
  both to the higher rate).
- *Amplitude continuity*: |RMS difference| vs the last `AMPLITUDE_STEPS_BACK = 4`
  chosen phonemes, raised to increasing powers and summed, ×
  `−RATING_AMPLITUDE_DIFFERENCE = −500` (a penalty; the largest weight in the system).
- *Contiguity bonus*: `step_3_n_following_previous_phonems` counts how many of the
  previous chosen audio phonemes are literally consecutive in the source
  (`a.audio_phonem.previous_in_seq() == parent.audio_phonem`), ×
  `RATING_LENGTH_SAME_PHONEM = 80`.

**Search** (`analyze.compute_children` + `Choice` tree): not Viterbi — a best-first
tree expansion with a global **node budget** `NODES = 1 << 12`:

- Children of a node are candidate associations scored by (step1+2 total) +
  sum(step3), taken in descending order with a geometric decay `modif /= RATE_POWER
  (1.1)` per sibling — so the branching factor shrinks fast.
- Expansion stops when `has_at_least_one_node(): (base_nodes-1)*rating >= total*steps_left`
  fails — i.e., the remaining budget, split proportionally to score share
  (`compute_nodes_left`), couldn't sustain one node per remaining target phoneme.
- **Skip shortcut** (`Choice._create_children`): before normal branching, if the
  current association begins a homophone word-sequence (by dictionary or by aligner
  phonemes) or an identical phoneme-sequence, the tree is forced down that contiguous
  source run via `SkippedChoice` (a filiform, branchless path). This is greedy
  longest-match chunk reuse, and it is checked *before* any scoring.
- Leaves become `Combo`s; `Combo.get_score()` = sum of `get_total_score()` along the
  path; combos sorted descending, top `n = 100` returned.

Determinism/diversity: a seeded `Randomizer` shuffles the phoneme lists and adds noise
(`RANDOM_SPAN = 50`) inside the step-2 sequence ratings, so reruns are reproducible
per seed but ties break differently across seeds.

All scoring is aggressively memoized with `functools.lru_cache` on methods — including
`lru_cache(maxsize=None)` keyed on self, a known memory-leak pattern; fine for a CLI
session, fatal for a long-lived process.

### The combo-audition loop (`CLI_sentence_mixing/cli_interface.py`)

`loop_interface(audio_command, video_futures)` — the part most directly relevant to
dipho's TUI:

1. User types a **chunk** (they explicitly recommend ~1 word per chunk in the README:
   "There is no big difference in accuracy between long and short chunks").
2. `sm.process_sm(sentence, videos)` returns the ranked combo list; the loop does
   `combo = available_combos.pop(0)` — candidates are auditioned **one at a time, in
   rank order**, never shown as a list.
3. Audition render is brutally simple (`video_creator/audio.py`):
   `np.concatenate([phonem.get_wave()[1] ...])` → `out.wav`. No crossfade, no
   zero-crossing snap, not even resampling (`# TODO resample to the highest rate`).
   Playback = `os.system(audio_command.format("out.wav"))` with a user-supplied shell
   command (default `tycat`; docs show vlc).
4. Keys: `ENTER` = next combo; `e` = re-edit chunk text; `s` = stash current combo in
   a buffer; `l <n>` = reload stashed combo n; `y` = accept chunk and continue.
   Stash list is printed each round. Dictionary `KeyError` / `PhonemError` /
   `TokenAmbiguityError` drop you back to the prompt with a hint.
5. Accepted phoneme lists accumulate; state is pickled to `video.json` after every
   chunk (crash recovery). Empty chunk ends the session; `video_creator/video.py`
   then cuts per-phoneme `VideoFileClip` subclips and
   `moviepy.concatenate_videoclips` → `out.mp4`.

`video_creator_main.py` overlaps work nicely: video download and audio/alignment run
in a `ThreadPoolExecutor` while the user is already typing.

### Why the project died

Verified facts:

- **MFA pin**: README requires "release executable version 1.1.0 Beta 2" of MFA
  (a 2018-era prebuilt binary). The `os.system` invocation is MFA-1.x CLI syntax;
  MFA 2.x/3.x (conda-distributed, `mfa align` subcommand, different model format)
  is incompatible. The commented-out `assert ret == 0` ("MFA returns a unexpected db
  error") shows it was already breaking. A large share of open issues are MFA
  install/run failures: CLI #12 "error with mfa align", #9 "[2180] Failed to execute
  script align", #5 "command 'aligner' ... not found", #6 "English accoustic model
  brings me to a 404 page".
- **Downloader rot**: originally youtube-dl (commit e91f4216, 2020-05-02 "workaround
  youtube-dl issue #5710"; README section "Unknown encoding idna" telling users to
  pip-upgrade youtube-dl). Library v2.x (Feb 2023) migrated to `yt-dlp` but **pinned
  `yt-dlp==2022.4.8`**, which has since been broken by YouTube changes — open issue
  sentence-mixing #13 (2025-11-24): "yt-dlp doesn't work". A dependabot PR bumping
  yt-dlp (2023-09-25) was never merged.
- **Version skew**: `CLI_sentence_mixing/requirements.txt` pins
  `sentence-mixing==1.1.3` (the youtube-dl era) while the library is at 2.0.4 on PyPI
  — the CLI was never updated past a 2021-04-21 compatibility commit.
- **Last commits**: library — 2023-02-22 ("release version 2.0.2"; pushed_at
  2023-09-25 is the dependabot branch). CLI — last real commit 2021-04-21; a README
  dead-link fix on 2024-01-14. Releases stopped at CLI v1.1.0, 2021-03-23.
- **Open issues**: 5 on the library, 13 on the CLI, nearly all unanswered
  environment breakage (Windows binary broken, no Mac release, MFA failures,
  downloader failures) — i.e., the *infrastructure* killed it, not the algorithm.
- Structural dead-ends acknowledged in their own README: subtitle-only sources (no
  ASR), dictionary-only G2P (OOV and ambiguous words unsupported), naive audio
  concatenation quality.

## Code pointers

- Solver entry + node-budget search: 
  https://github.com/pop123123123/sentence-mixing/blob/master/sentence_mixing/logic/analyze.py
  (`get_n_best_combos`, `compute_children`)
- Decision tree, skip logic, step-3 wiring:
  https://github.com/pop123123123/sentence-mixing/blob/master/sentence_mixing/model/choice.py
  (`Choice._create_children`, `Choice.compute_child_step_3_score`, `SkippedChoice`, `Combo`)
- Target-cost scoring:
  https://github.com/pop123123123/sentence-mixing/blob/master/sentence_mixing/model/association.py
  (class `Association`) and `logic/analyze_step_1.py` (`score_length`)
- Join-cost features:
  https://github.com/pop123123123/sentence-mixing/blob/master/sentence_mixing/logic/analyze_step_3.py
  and `logic/audio_analysis.py` (`cross_power_spectral_density_sum`,
  `get_normalized_rms`, `rate_amplitude_similarity`, `rate_silence`, `rate_duration`)
- All weights in one file:
  https://github.com/pop123123123/sentence-mixing/blob/master/sentence_mixing/logic/parameters.py
- Phoneme "index":
  https://github.com/pop123123123/sentence-mixing/blob/master/sentence_mixing/logic/global_audio_data.py
  (`get_transcription_dict_audio_phonem`, `get_candidates`)
- Ingest + MFA invocation + TextGrid parsing:
  https://github.com/pop123123123/sentence-mixing/blob/master/sentence_mixing/logic/video_processing.py
- Budget arithmetic + homophone/sequence utilities:
  https://github.com/pop123123123/sentence-mixing/blob/master/sentence_mixing/logic/utils.py
  (`has_at_least_one_node`, `compute_nodes_left`, `get_sequence_dictionary_homophones`)
- Audition loop:
  https://github.com/pop123123123/CLI_sentence_mixing/blob/master/cli_interface.py
  (`loop_interface`); orchestration in `video_creator_main.py`
- Audition/final render:
  https://github.com/pop123123123/sentence-mixing/blob/master/sentence_mixing/video_creator/audio.py
  (`concat_wav`) and `video_creator/video.py` (moviepy concat)
- READMEs (MFA 1.1.0-beta.2 requirement, UX docs, restrictions):
  both repos' `README.md`; dictionaries at https://github.com/nbusser/SM-Dictionaries

## Recommendation

Port ideas, not code. Per component:

1. **Lift the 3-step score decomposition** as the skeleton of dipho's solver — it maps
   1:1 onto Hunt & Black target cost (steps 1–2) + join cost (step 3). Their
   `parameters.py` weight table is a sane starting point for tuning ratios (e.g.,
   amplitude-join penalty 500 ≫ spectral-join 50; contiguity ~80–100/phoneme;
   duration caps 0.25 s consonant / 0.5 s vowel).
2. **Lift the skip/contiguity idea — it is their best result.** Greedy forced
   following of homophone-word and identical-phoneme runs (`SkippedChoice`) plus the
   step-2 "sequence length" scores all encode one principle: *maximally reuse
   contiguous source spans, because intra-span joins are free*. In dipho this becomes:
   join cost = 0 for diphones adjacent in the source, plus an explicit n-gram/span
   bonus in the search. dipho's diphone units make this even stronger — their
   phoneme-boundary cuts are the main audible weakness their join cost tries to paper
   over.
3. **Lift the audition-loop UX** into the TUI: chunk-by-chunk authoring (~1 word),
   rank-ordered candidate cycling, a stash buffer, re-edit, accept-and-advance,
   autosaved session. Improve on it: show a candidate *list* with scores/source
   context instead of pop-one-at-a-time, and audition via mpv EDL (zero render)
   instead of writing `out.wav` and shelling to a player.
4. **Rewrite candidate generation.** Their exact-transcription-match-only candidates
   cause hard `PhonemError` failures. dipho should add a phoneme substitution-cost
   matrix (fallback to phonetically near units) and back the index with SQLite
   (their in-memory dict + pickle + unbounded `lru_cache` is the right semantics,
   wrong substrate).
5. **Rewrite the search as beam/Viterbi over a lattice.** Their node-budget tree is a
   clever anytime heuristic but ad hoc (geometric sibling decay, budget arithmetic);
   with step-3 cost depending only on a short suffix (last vowel + last 4 RMS values
   + contiguity), beam search over per-position candidate lists gets the same
   diversity with cleaner complexity. Keep their seeded-noise trick for generating
   *alternative* rankings.
6. **Replace all infrastructure** — this is what killed them: MFA 1.1.0-beta-2 →
   WhisperX forced alignment (also removes the subtitles-required restriction);
   dictionary-only G2P → proper G2P with pronunciation variants (their
   `TokenAmbiguityError` is a UX cliff); pinned yt-dlp → unpinned/updatable yt-dlp;
   numpy sample concat + moviepy → dipho's planned mpv-EDL preview + ffmpeg render
   with DSP cut refinement (zero-crossing snap / spectral-flux minimization), which
   directly addresses their acknowledged `# TODO` audio quality gap.
7. Two small features worth copying verbatim: scoring `<BLANK>`/pause targets by
   silence-RMS + duration window (punctuation-as-pause is great authoring UX), and
   overlapping download/alignment with the user's first interactions via async tasks.
