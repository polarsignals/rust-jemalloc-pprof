.PHONY: capi
capi:
	cargo build -p capi --release

.PHONY: fmt
fmt:
	cargo fmt --all -- --check

.PHONY: lint
lint:
	cargo clippy --workspace -- -D warnings

.PHONY: doc
doc:
	RUSTDOCFLAGS="--cfg docsrs -D warnings" cargo doc --all-features --no-deps

.PHONY: test
test: fmt lint doc
	cargo test --workspace

.PHONY: clean
clean:
	cargo clean
