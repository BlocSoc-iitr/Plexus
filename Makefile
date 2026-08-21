.PHONY: fmt fmt-check clippy test devnet-test build pr clean

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cargo test --workspace --all-features

# Live BAL tests against a running Kurtosis devnet. Not part of `pr`: these are
# ignored by default and need a devnet plus both RPC URLs. See devnet/README.md.
devnet-test:
	cargo test -p parser --test devnet_bal_e2e -- --ignored --nocapture

build:
	cargo build --workspace --all-features

pr: fmt clippy test build

clean:
	cargo clean
