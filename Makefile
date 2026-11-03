.PHONY: run run-learn run-modifier-experiment-100 run-modifier-experiment-101 run-capture-experiment-102 fmt-check test

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

run-modifier-experiment-100:
	cargo build --bin experiment-100
	chcon -t bin_t "$(CURDIR)/target/debug/experiment-100"
	sudo systemd-run --pty --wait --collect \
		--uid="$$(id -u)" \
		--property=SupplementaryGroups=input \
		--working-directory="$(CURDIR)" \
		"$(CURDIR)/target/debug/experiment-100" $(EXPERIMENT_ARGS)

run-modifier-experiment-101:
	cargo build --bin experiment-101
	chcon -t bin_t "$(CURDIR)/target/debug/experiment-101"
	sudo systemd-run --pty --wait --collect \
		--uid="$$(id -u)" \
		--property=SupplementaryGroups=input \
		--working-directory="$(CURDIR)" \
		"$(CURDIR)/target/debug/experiment-101" $(EXPERIMENT_ARGS)

run-capture-experiment-102:
	cargo build --bin experiment-102
	chcon -t bin_t "$(CURDIR)/target/debug/experiment-102"
	sudo systemd-run --pty --wait --collect \
		--uid="$$(id -u)" \
		--property=SupplementaryGroups=input \
		--working-directory="$(CURDIR)" \
		"$(CURDIR)/target/debug/experiment-102" $(EXPERIMENT_ARGS)

fmt-check:
	cargo fmt --check

test:
	cargo test --bin true-switcher

.PHONY: install uninstall
install uninstall:
	./installer/installer.sh $@

.PHONY: build
build:
	cargo build --release --bin true-switcher
