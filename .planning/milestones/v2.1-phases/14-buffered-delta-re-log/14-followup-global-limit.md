Title: One memory limit for all open files

Each open file has its own memory limit. A file with random writes holds up to the dirty limit of written bytes (DIRTY_LIMIT, 32 MiB, #98). A file that streams holds up to 64 MiB of out-of-order writes (REORDER_WINDOW). So N open files can hold N times these limits.

Proposal: one budget for all open files. When the sum is above the budget, the file with the most bytes rebases (overlay) or settles its pending writes (stream) first.

Acceptance: a test with many open files, each with random writes, keeps the daemon peak RSS below one fixed cap.
