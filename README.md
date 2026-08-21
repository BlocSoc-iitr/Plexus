# Plexus

This branch is used for active development.

## Contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md) before opening an issue or pull request.

## BAL devnet

Plexus reads EIP-7928 block access lists, which do not exist on mainnet yet.
[`devnet/`](devnet/) holds a Kurtosis config that brings up a local two-client
Glamsterdam network producing real BAL data, and the end-to-end tests that run
against it. Those tests are ignored by default, so `cargo test` and CI never
need a devnet.
