APP = FloonetPanel.app
DEST = /Applications

# Extra flags for `swift build`, so a caller can ask for a universal binary
# without this file having to know where SwiftPM puts one. Empty by default:
# someone building the panel for their own machine wants their own machine's
# architecture, and a universal build takes twice as long to get it.
SWIFT_FLAGS ?=

# Asked of SwiftPM rather than spelled out. A single-architecture build lands in
# .build/<triple>/release; a `--arch a --arch b` build lands in
# .build/apple/Products/Release, and .build/release does not exist at all. A
# hardcoded path is therefore not merely wrong for the universal case, it is
# wrong in the direction that still produces a bundle: `build` below runs first
# and creates .build/release for the host, so the cp succeeded and shipped one
# architecture out of a two-architecture build. Lazy `=`, so `clean` does not
# pay for a query it has no use for.
BIN = $(shell swift build -c release $(SWIFT_FLAGS) --show-bin-path)/FloonetPanel

.PHONY: bundle build clean install install-prebuilt place run

build:
	swift build -c release $(SWIFT_FLAGS)

# `bundle` signs the BUNDLE, not just the executable inside it. `swift build`
# leaves the binary linker-signed ad-hoc, but a bundle with no _CodeSignature
# makes `codesign -v` report "code has no resources but signature indicates they
# must be present" — a bundle macOS will launch and still considers malformed.
# It matters here beyond tidiness: TCC permissions (Automation, Accessibility)
# are keyed to a signature, and this app drives other applications, so an
# inconsistently signed bundle risks being treated as a different app on each
# reinstall and losing what the user already granted.
# The bundled Info.plist carries the COMMIT, not the hand-written "1.0" — the
# panel is installed separately from the binaries and drifts from them, so its
# version is only useful if it says which build it is. Stamped at bundle time
# because the Swift build has no access to git.
GIT_SHA := $(shell git rev-parse --short HEAD 2>/dev/null || echo unknown)$(shell git status --porcelain --untracked-files=no 2>/dev/null | head -c1 | sed 's/./-dirty/')

bundle: build
	rm -rf $(APP)
	mkdir -p $(APP)/Contents/MacOS
	cp $(BIN) $(APP)/Contents/MacOS/FloonetPanel
	sed 's|<string>1.0</string>|<string>0.1.0 ($(GIT_SHA), $(shell date -u +%Y-%m-%d))</string>|' \
		Info.plist > $(APP)/Contents/Info.plist
	@codesign --force --sign - $(APP)
	@codesign -v $(APP) && echo "Built and signed $(APP)"

# Place a bundle that is ALREADY built — the release-archive path, where the
# .app ships prebuilt and no Swift toolchain is present. Same atomic rename as
# `install`, which is the part that must not be duplicated: writing over a
# running bundle file-by-file is what killed the app mid-write before.
install-prebuilt:
	@test -d "$(APP)" || { echo "no $(APP) here — nothing to install" >&2; exit 1; }
	$(MAKE) place

install: bundle place

# The atomic half, shared by both entry points.
#
# `rm -rf` + `cp -R` writes a new bundle into the path a RUNNING app has its
# executable mapped from, one file at a time. On macOS that fails the code
# signature on the next page fault and the app is killed mid-write, which is the
# same mechanism that showed up as `OS_REASON_CODESIGNING` for tpd. It also
# leaves a HALF-INSTALLED bundle if the copy is interrupted, and a half bundle
# still looks installed to anyone checking whether the app is there.
#
# Staging next to the destination (not in /tmp) keeps the rename on one
# filesystem, which is what makes it atomic.
place:
	rm -rf "$(DEST)/.$(APP).new"
	cp -R $(APP) "$(DEST)/.$(APP).new"
	@if pgrep -x FloonetPanel >/dev/null 2>&1; then \
		echo "  quitting the running FloonetPanel first"; \
		osascript -e 'tell application "FloonetPanel" to quit' >/dev/null 2>&1 || pkill -x FloonetPanel || true; \
		sleep 1; \
	fi
	rm -rf "$(DEST)/$(APP)"
	mv "$(DEST)/.$(APP).new" "$(DEST)/$(APP)"
	@echo "Installed to $(DEST)/$(APP)"

run: bundle
	open $(APP)

clean:
	rm -rf .build $(APP)
