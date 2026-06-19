.PHONY: fmt fmt-check clippy test build pr clean

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cargo test --workspace --all-features

build:
	cargo build --workspace --all-features

pr: fmt clippy test build

clean:
	cargo clean
