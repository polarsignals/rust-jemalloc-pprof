.PHONY: capi
capi:
	cargo build -p capi --release

.PHONY: fmt
fmt:
	cargo fmt --all -- --check

# Run Clippy with default and all features, to ensure both variants compile.
.PHONY: lint
lint:
	cargo clippy --workspace -- -D warnings
	cargo clippy --workspace --all-features -- -D warnings

.PHONY: doc
doc:
	RUSTDOCFLAGS="--cfg docsrs -D warnings" cargo doc --all-features --no-deps

.PHONY: test
test: fmt lint doc
	cargo test --workspace --all-features

.PHONY: clean
clean:
	cargo clean
