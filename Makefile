PREFIX ?= $(HOME)/.local

.PHONY: install
install:
	cargo install --path . --root $(PREFIX)
