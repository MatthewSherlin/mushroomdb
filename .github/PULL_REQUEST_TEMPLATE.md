## What and why

<!-- One or two sentences. Link the issue if there is one. -->

## How it was verified

<!-- The gates you ran, and anything you checked by hand. -->

- [ ] `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Language gates for the directories touched (`ui/`, `bindings/python/`, `clients/typescript/`) — see [CONTRIBUTING.md](https://github.com/MatthewSherlin/mushroomdb/blob/main/CONTRIBUTING.md)
- [ ] Docs and `CHANGELOG.md` updated if behavior changed

## Notes for the reviewer

<!-- Trade-offs, follow-ups, anything deliberately left out. -->
