## 1855 g1 (A=checkout A, B=checkout B)
A: small changes, 35%. Correct alias; unrequested deprecation warning; parser-only test. B: mergeable as-is, 5%. Real-binary test of both names, in scope. 7: B much closer (5% vs 35%).
## 1855 g2 (labels swapped: its A = checkout B, its B = checkout A)
checkout B: mergeable as-is, 0%. checkout A: small changes, 20% (warning path + parser-only test). 7: checkout B closer by ~20 points.
## 1913 g2 (labels swapped: its A = checkout B, its B = checkout A)
checkout B: small changes, 40%. Same sink-level fix, passing pipe-based test; should move protection to handover receipt. checkout A: large changes, 70%. Same production core; own queue test fails on macOS socket path limit; unrelated HTTP test change; 300 ms wait, leaky cleanup. 7: checkout B closer by ~30 points.
## 1913 g1 (A=checkout A, B=checkout B)
checkout A: small changes, 50%. Fix passes merged test; own queue test fails at bind (macOS path limit); per-job 65,536-fd scan. checkout B: mergeable as-is, 35%. Passing pipe-based before/after test. 7: checkout B closer by ~15 points.
