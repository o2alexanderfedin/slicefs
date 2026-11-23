# Phase 20 deferred items

- **Commit timer during unmount (found in 20-03, older than Phase 20).** The commit timer stops only after `mount()` returns. A timed commit can still run while `destroy` runs its own commit. Commits are serialized, and the 20-03 shutdown test passed 10 of 10 runs. Decision (orchestrator, reversible): not in Phase 20 scope; the Phase 20 code review checks it. If the review finds a real race, a fix plan stops the timer in `shutdown` before the destroy work.
