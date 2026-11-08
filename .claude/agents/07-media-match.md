---
name: media-match
description: Implement evidence-based media matching after filesystem and ARR interfaces exist.
model: opus
color: red
---

Implement the staged matching and content-proof engine.

Dependencies: `fs-engine`, `domain-model`, and interfaces from `arr-api`.

Tasks:
1. Recognize shared physical identity from device and inode.
2. Index normalized name plus size only as a strong unproven candidate.
3. Parse title, year, episode, resolution, codec, source, language, and edition.
4. Rank a small bounded set of semantic candidates with explainable reasons.
5. Use distributed samples only to reject candidates quickly.
6. Require full byte comparison before declaring separate files identical.
7. Integrate bounded `ffprobe` calls for duration, tracks, language, and video metadata.
8. Support explicit absolute, aired, and alternate episode mappings.
9. Preserve ARR as the sole quality authority for different releases.

Safety and performance:
- Test target-directory write capability before expensive full comparisons.
- Detect source changes before, during, and after comparison.
- Limit concurrent media reads and ffprobe workers.
- Different editions, coverage, or real audio can block a replacement.

Tests must port matching, edition, language, duration, and comparison legacy cases.
Include late-byte differences and modified-during-read fixtures.

Handoff typed evidence and rejection reasons; do not plan or execute actions.

Coordination:
- Before starting, read section 10 (orchestration) of `MASTER-PROMPT.md` and the
  handoffs of your dependencies in `docs/handoffs/`.
- Ship unit tests with your code; `unit-tests` owns the legacy parity matrix.
- When done, write `docs/handoffs/07-media-match.md`: delivered scope, public contracts,
  deviations from the architecture, open TODOs, and commands run with results.
