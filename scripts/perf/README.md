Run `bash scripts/perf/bench.sh [runs]` against the live server; set `SM_BENCH_ID` to a live session ID.
Compare before and after only when the one-minute load averages are within 30%.
Run `bash scripts/perf/capture.sh OUTPUT_DIR` through `sm queue run --type tests` to capture six 10-second profiles, with 110 seconds between captures.
Run `python3 scripts/perf/agg.py OUTPUT_DIR/s*.txt` to aggregate busy CPU and lock waits.
`prof.py` excludes waiting leaves and attributes busy samples to the outermost application frame below a thread entry, as specified in memo #2416 Appendix A.
