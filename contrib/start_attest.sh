#!/usr/bin/env sh
export ATTEST_CONFIG_JSON=$(cat attest_config.json)
echo $ATTEST_CONFIG_JSON | jq 
../target/release/attest