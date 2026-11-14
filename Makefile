.PHONY: help release

SHELL := /bin/bash

help:  ## Show this help.
	@grep -Eh '\s##\s' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m  %-30s\033[0m %s\n", $$1, $$2}'

release:  ## Create a GitHub release; CI publishes it to crates.io
	@bash ./release.sh
