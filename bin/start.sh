#!/bin/bash

if [ -z "$LIQUIDATOR_SRC_PATH" ]; then
    echo "ERROR: LIQUIDATOR_SRC_PATH is not set."
    exit 1  
else
    echo "LIQUIDATOR_SRC_PATH is set to: $LIQUIDATOR_SRC_PATH"
fi

if [ -z "$1" ]; then
    echo "The Configuration file argument is missing."
    exit 1
fi
echo "Configuration file: $1"

pushd $LIQUIDATOR_SRC_PATH > /dev/null

export RUST_LOG=debug,hyper=info
export RUST_BACKTRACDE=full
cargo run -- run $1

popd > /dev/pull


