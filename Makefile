.PHONY: run run-learn run-input-debug run-modifier-experiment-100 run-modifier-experiment-101 run-capture-experiment-102 fmt-check test logs

run:
	cargo build --bin true-switcher
	chcon -t bin_t "$(CURDIR)/target/debug/true-switcher"
	sudo systemd-run --pty --wait --collect \
		--uid="$$(id -u)" \
		--property=SupplementaryGroups=input \
		--setenv=DBUS_SESSION_BUS_ADDRESS="$${DBUS_SESSION_BUS_ADDRESS:-unix:path=/run/user/$$(id -u)/bus}" \
		"$(CURDIR)/target/debug/true-switcher"

run-input-debug:
	cargo build --bin input-debug
	chcon -t bin_t "$(CURDIR)/target/debug/input-debug"
	sudo systemd-run --pty --wait --collect \
		--uid="$$(id -u)" \
		--property=SupplementaryGroups=input \
		--working-directory="$(CURDIR)" \
		"$(CURDIR)/target/debug/input-debug"

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

logs:
	sudo journalctl -u true-switcher.service -n 1000 -f

.PHONY: install uninstall
install uninstall:
	./installer/installer.sh $@

.PHONY: build update-installed
build:
	cargo build --release --bin true-switcher

update-installed: build
	test -f /usr/local/bin/true-switcher
	sudo systemctl stop true-switcher.service
	sudo install -m 755 target/release/true-switcher /usr/local/bin/true-switcher
	if command -v restorecon >/dev/null 2>&1; then sudo restorecon /usr/local/bin/true-switcher; fi
	sudo systemctl start true-switcher.service
