.PHONY: install build test nextest review coverage coverage-lcov deny mutants fuzz bench udeps check

install:
	cargo install --path .

build:
	cargo build

test:
	cargo test

nextest:
	cargo nextest run

review:
	cargo insta review

coverage:
	cargo llvm-cov --html --open

coverage-lcov:
	cargo llvm-cov --lcov --output-path lcov.info

deny:
	cargo deny check

mutants:
	cargo mutants --timeout 60

fuzz:
	@echo "Run individual fuzz targets:"
	@echo "  cargo fuzz run fuzz_normalize_capture -- -max_total_time=60"
	@echo "  cargo fuzz run fuzz_jsonl_parsing -- -max_total_time=60"
	@echo "  cargo fuzz run fuzz_extract_message -- -max_total_time=60"
	@echo "  cargo fuzz run fuzz_diff_numstat -- -max_total_time=60"

bench:
	cargo bench

udeps:
	cargo +nightly udeps --all-targets

# Run all quality checks (good for CI)
check: test deny udeps
	cargo fmt --check
	cargo clippy -- -D warnings
