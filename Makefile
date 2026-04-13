PREFIX ?= $(HOME)/.local

.PHONY: install
install:
	cargo install --force --path . --root $(PREFIX)
