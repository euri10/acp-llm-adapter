#!/usr/bin/env bash
exec 2>>/tmp/acp-debug.log
exec /home/lotso/code/deepseek-acp-adapter/target/release/deepseek-acp-adapter "$@"
