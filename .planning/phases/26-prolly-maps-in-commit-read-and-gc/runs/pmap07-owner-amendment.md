# PMAP-07 owner amendment (2026-11-27, before any decision run)

This note is written by the owner's decision, before any decision run of
plan 26-13. It does not change `pmap07-rule.md` or `pmap07.py`. They stay
as committed, and `pmap07.py` prints its verdict unchanged.

Decision: "Let's not do any preliminary optimizations. Prolly was decided
primarily for data scalability, which it supports."

What this means for the verdict:

- Check A (pre-sync work at 1,000,000 files, at most 0.20 of flat) and
  check B (total wall time lower than flat) decide PMAP-07.
- Check C (the 9 Boost rows, not `drop`) is measured and recorded, but it
  does not block. If a Boost row is not better than flat (for example
  insert/delete at K = 100, where Phase 24 already measured prolly slower on
  Linux), the row is recorded with its numbers as an accepted cost of the
  scalable layout. This is the "written acceptance" that PMAP-07 allows for
  the insert/delete-heavy regression.
- No tuning and no extra runs follow from a check C result. Optimization is
  later work, if the owner asks for it.
- The phase result is written as: verdict of `pmap07.py` as printed, then
  "PMAP-07 decided by checks A and B (owner amendment); check C recorded".
