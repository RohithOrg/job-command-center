# Developer shortcuts for the desktop app.
.PHONY: up run test mac tidy

up:             ## install what's missing, build and launch (Windows: scripts\start.ps1)
	./scripts/start.sh

run:            ## debug build and launch
	cargo run

test:
	cargo test

mac:            ## Job Command Center.app + .dmg in dist/
	./scripts/macos-bundle.sh

tidy:           ## remove compiler leftovers (target/ grows with every dependency change)
	cargo clean
	@echo "Build cache cleared. The next build starts from scratch (a few minutes)."
