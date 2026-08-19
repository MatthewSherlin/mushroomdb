# Homebrew formula

`mushroomdb.rb.in` is a template. It is **not** tap-ready.

Each `v*` GitHub Release attaches `mushroomdb.rb` with sha256 values taken
from that release's artifacts. After every tag:

1. Download `mushroomdb.rb` from the Release assets.
2. Copy it over the formula in the tap (`mushroomdb.rb`).
3. Open a tap PR / `brew bump-formula-pr` as usual.

Do not `brew install` from `packaging/homebrew/` on `main` — the in-tree
`mushroomdb.rb` stub refuses to load until replaced by the release asset.

- Before the first multi-arch push: verify the distroless base digest is the multi-platform MANIFEST-LIST digest (not a single-arch manifest), or the arm64 image silently resolves differently.
