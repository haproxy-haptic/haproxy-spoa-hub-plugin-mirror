.PHONY: verify-release

verify-release:
ifndef VERSION
	$(error VERSION not set; usage: make verify-release VERSION=vX.Y.Z)
endif
	@scripts/verify-release.sh "$(VERSION)"
