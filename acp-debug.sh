#!/usr/bin/env bash
set -u

LOG_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/acp-llm-adapter"
mkdir -p "$LOG_DIR"

ACP_BINARY="$(basename "$1")"

DETECTED_BACKEND=""
ARGS=("$@")
for ((i = 1; i < ${#ARGS[@]}; i++)); do
  case "${ARGS[$i]}" in
    --backend=*)
      DETECTED_BACKEND="${ARGS[$i]#--backend=}"
      break
      ;;
    --backend)
      if ((i + 1 < ${#ARGS[@]})); then
        DETECTED_BACKEND="${ARGS[$((i + 1))]}"
      fi
      break
      ;;
  esac
done

RAW_LABEL="${ACP_DEBUG_LABEL:-$DETECTED_BACKEND}"
SANITIZED_LABEL="$(printf '%s' "$RAW_LABEL" | sed 's/[^A-Za-z0-9._-]/_/g')"
LABEL_SEGMENT=""
if [ -n "$SANITIZED_LABEL" ]; then
  LABEL_SEGMENT="-$SANITIZED_LABEL"
fi

TS="$(date +%Y%m%d-%H%M%S)"
PID="$$"
STDERR_LOG="$LOG_DIR/$TS-$PID-$ACP_BINARY$LABEL_SEGMENT-stderr.log"
STDOUT_LOG="$LOG_DIR/$TS-$PID-$ACP_BINARY$LABEL_SEGMENT-stdout-jsonrpc.log"

echo "cwd: $(pwd)" >> "$STDERR_LOG"
echo "argv: $*" >> "$STDERR_LOG"
echo "label: ${SANITIZED_LABEL:-<none>}" >> "$STDERR_LOG"
echo "env PATH: $PATH" >> "$STDERR_LOG"

# Enable TRACE logging for the llm module if not already set
export RUST_LOG="${RUST_LOG:-acp_llm_adapter::llm=trace}"
echo "RUST_LOG: $RUST_LOG" >> "$STDERR_LOG"

# stdout is ACP JSON-RPC, so tee it without adding anything.
# stderr is safe to redirect to a log file.
exec "$@" 2>>"$STDERR_LOG" | tee -a "$STDOUT_LOG"
