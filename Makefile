.PHONY: run run-learn fmt-check test

run:
	cargo build --bin true-switcher
	chcon -t bin_t "$(CURDIR)/target/debug/true-switcher"
	sudo systemd-run --pty --wait --collect \
		--uid="$$(id -u)" \
		--property=SupplementaryGroups=input \
		--setenv=DBUS_SESSION_BUS_ADDRESS="$${DBUS_SESSION_BUS_ADDRESS:-unix:path=/run/user/$$(id -u)/bus}" \
		"$(CURDIR)/target/debug/true-switcher"

run-learn:
	cargo run --bin learn

fmt-check:
	cargo fmt --check

test:
	cargo test --bin true-switcher
