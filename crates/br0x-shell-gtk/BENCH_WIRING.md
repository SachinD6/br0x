# Bench wiring (done)

`src/main.rs` contains the full hookup; this note records the shape:

- `mod bench;` at the top of `main.rs`.
- `start_bench_server(&shell)` runs in `build_ui()` right after
  `start_session()`. It returns immediately unless `BR0X_BENCH=1` is set.
- Socket threads enqueue `BenchJob`s on a shared queue; a 25ms main-loop
  pump (`handle_bench_job`) executes them on the GTK thread.
- Bench tabs live in a private registry: never in the tab strip, session
  file, history, or suggestions. `close` cannot reach user tabs.
