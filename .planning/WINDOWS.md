---
schema_version: 1
open_count: 2
waived_count: 0
fixed_count: 0
total_count: 2
last_updated: 2026-11-22T21:23:38.007Z
---

# Broken Windows Ledger

> Cross-phase defect register. With `workflow.windows_enforce` enabled, `/gsd-ship` blocks while `open_count > 0`.
> Waive with `gsd-tools windows waive <id> "<reason>"` (reason required).
> Mark fixed with `gsd-tools windows fixed <id>`.

| id | phase | kind | file | line | description | status | reason | recorded_at | resolved_at |
|----|-------|------|------|------|-------------|--------|--------|-------------|-------------|
| 1 | 19 | unrun-verify | .planning/phases/19-fine-grained-locks/19-11-PLAN.md |  | 19-11 Task 3: the #21 body checks against gh issue view 21 (same locks >=1, old writer step 0, no licensing or home paths) wait for the orchestrator to apply the staged body | open |  | 2026-11-22T11:33:17.310Z |  |
| 2 | 19 | deviation | crates/slicefs-cli/src/filesystem.rs |  | 19-14: hard_cap() is limit + limit/GROW_DIV + 2*REBASE_STEP, not the planned limit + 2*REBASE_STEP; a switch above the cap pays one large gate step (owner choice) | open |  | 2026-11-22T21:23:38.007Z |  |

````json
[
  {
    "id": 1,
    "kind": "unrun-verify",
    "phase": "19",
    "file": ".planning/phases/19-fine-grained-locks/19-11-PLAN.md",
    "line": null,
    "description": "19-11 Task 3: the #21 body checks against gh issue view 21 (same locks >=1, old writer step 0, no licensing or home paths) wait for the orchestrator to apply the staged body",
    "status": "open",
    "reason": "",
    "recorded_at": "2026-11-22T11:33:17.310Z",
    "resolved_at": null,
    "milestone": "v2.2"
  },
  {
    "id": 2,
    "kind": "deviation",
    "phase": "19",
    "file": "crates/slicefs-cli/src/filesystem.rs",
    "line": null,
    "description": "19-14: hard_cap() is limit + limit/GROW_DIV + 2*REBASE_STEP, not the planned limit + 2*REBASE_STEP; a switch above the cap pays one large gate step (owner choice)",
    "status": "open",
    "reason": "",
    "recorded_at": "2026-11-22T21:23:38.007Z",
    "resolved_at": null,
    "milestone": "v2.2"
  }
]
````
