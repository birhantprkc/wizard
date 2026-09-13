#!/usr/bin/env python3
"""A scripted OpenAI-compatible server for the completion-review demo and bench.

    python3 bench/review-mock-server.py 8099 /tmp/calls.jsonl [--honest] [--steps=N]

Nothing here is a model. It replays a fixed script so the run is reproducible:

  agent, step 1      run one command (so the run has done something)
  agent, step 2      claim the deliverable is written, without writing it
  review, step 1     look for the deliverable
  review, step 2     FAIL, naming the missing file
  agent, rework      write the file, then report done

With --honest the agent writes the file first and then does --steps=N calls of
busywork before reporting done, so the overhead can be read against a run of a
realistic length. The review follows what the machine says, so that arm passes. Every request is appended to the
JSONL log as {role, prompt_chars, messages} so a caller can count model calls
and prompt size with the review on and off.
"""

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = "mock-model"
REVIEW_MARKER = "You are reviewing a finished autonomous run"
REWORK_MARKER = "An independent review checked the machine"
DELIVERABLE = "/tmp/wizard-review-demo/report.txt"
WRITE_ARGS = {"path": DELIVERABLE, "content": "summary: 3 files, 0 errors\n"}
CALLS = None
HONEST = False
WORK_STEPS = 1


def text_of(content):
    """OpenAI content is a string or a list of blocks; we only want the text."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return " ".join(
            block.get("text", "") for block in content if isinstance(block, dict)
        )
    return ""


def transcript(messages):
    return "\n".join(text_of(m.get("content")) for m in messages)


def tool_results(messages):
    return sum(1 for m in messages if m.get("role") == "tool")


def reply(messages):
    """(role, content, tool_calls) for this request."""
    system = text_of(messages[0].get("content")) if messages else ""

    if REVIEW_MARKER in system:
        if tool_results(messages) == 0:
            return "review", "", [tool_call("execute", {"command": f"ls -l {DELIVERABLE}"})]
        # The verdict follows what the machine actually said, so the arm that
        # wrote the file passes and the arm that did not fails.
        seen = _tool_output(messages)
        if "No such file" in seen or "cannot access" in seen:
            return (
                "review",
                f"FAIL\n{DELIVERABLE} does not exist: `ls` reports "
                "'No such file or directory'. The run reported it written.",
                [],
            )
        return "review", f"PASS\n`ls -l {DELIVERABLE}` shows it written and non-empty.", []

    rework_at = _marker_index(messages, REWORK_MARKER)
    if rework_at is not None:
        if tool_results(messages[rework_at:]) == 0:
            return "agent-rework", "", [tool_call("write_file", WRITE_ARGS)]
        return "agent-rework", f"Fixed. {DELIVERABLE} is written.", []

    done = tool_results(messages)
    if done == 0:
        return "agent", "", [tool_call("write_file", WRITE_ARGS)] if HONEST else [
            tool_call("execute", {"command": "ls /tmp"})
        ]
    # Busywork, so the overhead can be read against a run of a realistic length
    # rather than against a two-call toy.
    if done < WORK_STEPS:
        return "agent", "", [tool_call("execute", {"command": f"echo step {done}"})]
    return "agent", f"Done. I wrote the summary to {DELIVERABLE}.", []


def _marker_index(messages, marker):
    """Index of the first message containing `marker`, or None."""
    for index, message in enumerate(messages):
        if marker in text_of(message.get("content")):
            return index
    return None


def _tool_output(messages):
    """Text of every tool result so far."""
    return "\n".join(
        text_of(m.get("content")) for m in messages if m.get("role") == "tool"
    )


def tool_call(name, args):
    return {
        "id": f"call_{name}_{int(time.time() * 1000) % 100000}",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(args)},
    }


def chunk(delta, finish=None):
    body = {
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": MODEL,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }
    return f"data: {json.dumps(body)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def send_json(self, body, status=200):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self.send_json({"object": "list", "data": [{"id": MODEL, "object": "model"}]})
        else:
            self.send_json({"error": "not found"}, 404)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        request = json.loads(self.rfile.read(length) or b"{}")
        if not self.path.rstrip("/").endswith("/chat/completions"):
            return self.send_json({"error": "not found"}, 404)

        messages = request.get("messages", [])
        role, content, calls = reply(messages)
        prompt_chars = len(json.dumps(messages))
        if CALLS:
            with open(CALLS, "a") as log:
                log.write(
                    json.dumps(
                        {
                            "role": role,
                            "prompt_chars": prompt_chars,
                            "at": time.time(),
                            "tool_calls": [c["function"]["name"] for c in calls],
                            "text": content,
                        }
                    )
                    + "\n"
                )

        if not request.get("stream"):
            message = {"role": "assistant", "content": content or None}
            if calls:
                message["tool_calls"] = calls
            return self.send_json(
                {
                    "id": "chatcmpl-mock",
                    "object": "chat.completion",
                    "model": MODEL,
                    "choices": [
                        {"index": 0, "finish_reason": "tool_calls" if calls else "stop",
                         "message": message}
                    ],
                }
            )

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(chunk({"role": "assistant", "content": ""}))
        if content:
            self.wfile.write(chunk({"content": content}))
        for index, call in enumerate(calls):
            self.wfile.write(
                chunk(
                    {
                        "tool_calls": [
                            {
                                "index": index,
                                "id": call["id"],
                                "type": "function",
                                "function": call["function"],
                            }
                        ]
                    }
                )
            )
        self.wfile.write(chunk({}, "tool_calls" if calls else "stop"))
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    HONEST = "--honest" in sys.argv
    for flag in sys.argv[1:]:
        if flag.startswith("--steps="):
            WORK_STEPS = int(flag.split("=", 1)[1])
    port = int(args[0]) if args else 8099
    CALLS = args[1] if len(args) > 1 else None
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
