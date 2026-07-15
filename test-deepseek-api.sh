#!/usr/bin/env bash
# Test different DeepSeek API request variations to identify 400 error cause

API_KEY="${DEEPSEEK_API_KEY}"
BASE_URL="https://api.deepseek.com"

if [ -z "$API_KEY" ]; then
    echo "Error: DEEPSEEK_API_KEY not set"
    exit 1
fi

echo "=== Testing DeepSeek API 400 Error ==="
echo "API Key: ${API_KEY:0:10}..."
echo ""

# Test 1: Minimal request (single message, no tools, no reasoning)
echo "[TEST 1] Minimal request (single message, no tools, no reasoning_effort)"
curl -s -X POST "$BASE_URL/chat/completions" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-v4-pro",
    "messages": [{"role": "user", "content": "hello"}],
    "stream": false
  }' | jq '.' | head -20
echo ""

# Test 2: With reasoning_effort=max, no tools
echo "[TEST 2] With reasoning_effort=max, no tools"
curl -s -X POST "$BASE_URL/chat/completions" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-v4-pro",
    "messages": [{"role": "user", "content": "hello"}],
    "stream": false,
    "reasoning_effort": "max"
  }' | jq '.' | head -20
echo ""

# Test 3: With streaming
echo "[TEST 3] With streaming"
curl -s -X POST "$BASE_URL/chat/completions" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-v4-pro",
    "messages": [{"role": "user", "content": "hello"}],
    "stream": true,
    "reasoning_effort": "max"
  }' | head -5
echo ""

# Test 4: With tools, no reasoning_effort
echo "[TEST 4] With tools, no reasoning_effort"
curl -s -X POST "$BASE_URL/chat/completions" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-v4-pro",
    "messages": [{"role": "user", "content": "hello"}],
    "stream": true,
    "tools": [{"type": "function", "function": {"name": "test", "description": "test", "parameters": {}}}]
  }' | head -5
echo ""

# Test 5: With tools AND reasoning_effort=max
echo "[TEST 5] With tools AND reasoning_effort=max"
curl -s -X POST "$BASE_URL/chat/completions" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-v4-pro",
    "messages": [{"role": "user", "content": "hello"}],
    "stream": true,
    "reasoning_effort": "max",
    "tools": [{"type": "function", "function": {"name": "test", "description": "test", "parameters": {}}}]
  }' | head -5
echo ""

echo "=== Tests Complete ==="
