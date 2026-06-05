#!/usr/bin/env bash
exec 2>>/tmp/acp-debug.log
exec deepseek-acp-adapter "$@"
