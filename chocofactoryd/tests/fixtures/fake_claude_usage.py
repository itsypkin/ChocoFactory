#!/usr/bin/env python3
"""`claude` stand-in whose one turn reports the same scripted usage figures
as `fake_omp.py`'s default turn, for the usage-parity test: 300 input, 60
output, 90 cache-read and 15 cache-write tokens, a cost of 0.0369 USD, one
model, three model turns and a 1234 ms duration, on a subscription login.
"""
import json
import sys
import uuid

from fake_common import emit, iter_turns


def main():
    session_id = str(uuid.uuid4())
    emit({"type": "system", "subtype": "init", "session_id": session_id,
          "apiKeySource": "none"})
    for _ in iter_turns(sys.stdin):
        emit({"type": "assistant",
              "message": {"content": [{"type": "text", "text": "done"}]},
              "session_id": session_id})
        emit({
            "type": "result", "subtype": "success", "is_error": False,
            "result": "done", "session_id": session_id,
            "total_cost_usd": 0.0369, "duration_ms": 1234, "num_turns": 3,
            "usage": {"input_tokens": 300, "output_tokens": 60,
                      "cache_read_input_tokens": 90,
                      "cache_creation_input_tokens": 15},
            "modelUsage": {"scripted-model": {
                "inputTokens": 300, "outputTokens": 60,
                "cacheReadInputTokens": 90, "cacheCreationInputTokens": 15,
                "costUSD": 0.0369}},
        })


if __name__ == "__main__":
    main()
