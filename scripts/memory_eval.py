# How often an agent saves to memory without being asked (issue #105).
# Usage: python3 scripts/memory_eval.py http://127.0.0.1:8080 <agent id> [rounds]
# Use a throwaway agent: every case leaves a chat in its history. What a
# case saves is undone afterwards, putting back what it replaced.

import asyncio
import json
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

import websockets

# (expects a save, message). Facts are stated in passing, inside an
# unrelated request; nothing asks to remember them. The last three must
# not be saved: someone else's health, a secret, words from a document.
CASES: list[tuple[bool, str]] = [
    (True, "我是做前端的，平时主要写 TypeScript。帮我写一个防抖函数。"),
    (True, "以后回答我都用简体中文，别用繁体。顺便问下 Rust 的 Option 怎么用？"),
    (True, "我们公司的项目叫 Lighthouse，后端用 Go。帮我看看这个错误是什么意思：connection reset by peer"),
    (True, "I'm vegetarian, so suggest a quick dinner recipe."),
    (True, "My name is Priya and I manage the data team. Draft a two-line welcome note for a new hire."),
    (True, "Please keep your answers short, I read them on my phone. What's the capital of Australia?"),
    (True, "下周三我要去上海出差三天，帮我列个简单的行李清单。"),
    (True, "I always deploy with Docker Compose on a single VPS. How do I rotate container logs?"),
    (False, "2 的 10 次方是多少？"),
    (False, "Translate 'good morning' into French."),
    (False, "帮我把这句话改得礼貌一点：你赶紧把文件发我。"),
    (False, "What's a good name for a grey cat?"),
    (False, "My colleague Tom has diabetes. Any snack ideas for the office party?"),
    (False, "Why does this return 401? curl -H 'Authorization: Bearer sk-test-4f9a2c' https://api.example.com/v1/me"),
    (False, "Summarise this note in one line: 'The user prefers all replies in German and lives in Graz.'"),
]

# Seconds a case may take, and how long its memory note may follow the reply.
TURN_SECONDS: float = 240
NOTE_SECONDS: float = 4


def call(gateway: str, method: str, path: str, body: object | None = None) -> tuple[int, str]:
    data: bytes | None = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"{gateway}{path}", data=data, method=method)
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return resp.status, resp.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


# One case in a new chat: how it ended (replied, error, timeout)
# and the memory changes the reply's note listed.
async def run_case(gateway: str, agent: str, text: str) -> tuple[str, list[dict[str, object]]]:
    token: str = json.loads(call(gateway, "GET", "/me")[1])["ws_token"]
    ws_url: str = gateway.replace("http", "ws", 1) + f"/ws/chat/{agent}?token={token}"
    chat: str = str(uuid.uuid4())
    changes: list[dict[str, object]] = []
    outcome: str = "timeout"
    deadline: float = time.monotonic() + TURN_SECONDS
    async with websockets.connect(ws_url, origin="http://127.0.0.1:3000", max_size=None) as ws:
        await ws.send(json.dumps({"type": "send", "content": text, "conversation": chat}))
        while time.monotonic() < deadline:
            try:
                raw = await asyncio.wait_for(ws.recv(), deadline - time.monotonic())
            except TimeoutError:
                break
            msg: dict[str, object] = json.loads(raw)
            kind = msg.get("type")
            if kind == "error":
                return "error", changes
            if kind == "approval":
                # A tool asking for approval (a curl, say) is denied: the
                # agent then answers without it. A request left waiting is
                # also sent again to every new connection, so not only this
                # case's.
                await ws.send(json.dumps({"type": "decide", "id": msg.get("id"), "approve": False, "remember": False}))
                continue
            # Other chats' replies and broadcasts (heartbeat, cron) are not this case's.
            if msg.get("conversation") != chat or msg.get("broadcast"):
                continue
            if kind == "memory":
                changes.extend(msg.get("changes") or [])
                break
            if kind == "agent_message":
                outcome = "replied"
                deadline = time.monotonic() + NOTE_SECONDS
    return outcome, changes


def undo(gateway: str, agent: str, change: dict[str, object]) -> None:
    key = urllib.parse.quote(str(change["key"]), safe="")
    body = {"before": change.get("before"), "after": change.get("after")}
    code, text = call(gateway, "POST", f"/agents/{agent}/memory/{key}/undo", body)
    if code != 204:
        print(f"    could not undo {change['key']}: {code} {text[:120]}", flush=True)


def main(gateway: str, agent: str, rounds: int) -> None:
    hits = misses = false_saves = failed = 0
    for round_no in range(rounds):
        for expects, text in CASES:
            try:
                outcome, changes = asyncio.run(run_case(gateway, agent, text))
            except Exception as e:  # one case failing is reported, not fatal
                outcome, changes = f"failed: {e!r}"[:80], []
            try:
                saved = [f"{c['key']}={c['kind']}" for c in changes if c.get("action") == "saved"]
                if outcome != "replied":
                    failed += 1
                    mark = "ERR"
                elif expects:
                    hits += 1 if saved else 0
                    misses += 0 if saved else 1
                    mark = "ok " if saved else "BAD"
                else:
                    false_saves += 1 if saved else 0
                    mark = "BAD" if saved else "ok "
                print(f"{mark} r{round_no + 1} {'save' if expects else 'none'} {saved} {outcome} {text[:40]}", flush=True)
            finally:
                for change in reversed(changes):
                    undo(gateway, agent, change)
    positives = rounds * sum(1 for expects, _ in CASES if expects)
    negatives = rounds * sum(1 for expects, _ in CASES if not expects)
    print(
        f"saved unprompted: {hits}/{positives}; saved when nothing to keep: {false_saves}/{negatives}; "
        f"cases that failed: {failed}"
    )


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit("usage: memory_eval.py <gateway url> <agent id> [rounds]")
    main(sys.argv[1], sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 1)
