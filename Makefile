.PHONY: install build test review

install:
	cargo install --path .

build:
	cargo build

test:
	cargo test

review:
	cargo insta review
