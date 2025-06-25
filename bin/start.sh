#!/bin/bash

if [ -z "$LIQUIDATOR_SRC_PATH" ]; then
    echo "ERROR: LIQUIDATOR_SRC_PATH is not set."
    exit 1  
else
    echo "LIQUIDATOR_SRC_PATH is set to: $LIQUIDATOR_SRC_PATH"
fi

pushd $LIQUIDATOR_SRC_PATH > /dev/null

export RUST_LOG=debug,hyper=info,h2::codec=info,eva01::geyser_processor=info,eva01::clock_manager=info
export RUST_BACKTRACDE=full
cargo run

popd > /dev/null


