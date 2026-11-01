.PHONY: run run-learn

run:
	cargo build --bin true-switcher
	chcon -t bin_t "$(CURDIR)/target/debug/true-switcher"
	sudo systemd-run --pty --wait --collect \
		--uid="$$(id -u)" \
		--property=SupplementaryGroups=input \
		"$(CURDIR)/target/debug/true-switcher"

run-learn:
	cargo run --bin learn
