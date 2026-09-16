# Contribute
1. Fork, branch from `main`, keep PRs under 300 lines.
2. Run `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
3. Put UI code in `br0x-shell-gtk`, logic in `br0x-core`.
4. Add a test for each fix in `br0x-core`.
5. Use Conventional Commits, e.g. `fix(core): prune history`.
6. Be direct and kind. Maintainers decide scope for v1.
