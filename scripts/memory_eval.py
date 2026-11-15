# How often an agent saves to memory without being asked (issue #105).
# Usage: python3 scripts/memory_eval.py http://127.0.0.1:8080 <agent id> [rounds]

import asyncio
import json
import sys
import urllib.parse
import urllib.request
import uuid

import websockets

# (expects a save, message). Facts are stated in passing, inside an
# unrelated request; nothing asks to remember them.
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
]


def get(gateway: str, path: str) -> dict[str, object]:
    with urllib.request.urlopen(f"{gateway}{path}", timeout=60) as resp:
        return json.loads(resp.read().decode())


def keys(gateway: str, agent: str) -> dict[str, str]:
    entries = get(gateway, f"/agents/{agent}/memory?limit=1000")["entries"]
    assert isinstance(entries, list)
    return {str(e["key"]): str(e["updated_at"]) for e in entries}


def forget(gateway: str, agent: str, key: str) -> None:
    quoted = urllib.parse.quote(key, safe="")
    req = urllib.request.Request(f"{gateway}/agents/{agent}/memory/{quoted}", method="DELETE")
    with urllib.request.urlopen(req, timeout=60):
        pass


async def ask(gateway: str, agent: str, text: str) -> None:
    token = get(gateway, "/me")["ws_token"]
    ws_url = gateway.replace("http", "ws", 1) + f"/ws/chat/{agent}?token={token}"
    origin = "http://127.0.0.1:3000"
    async with websockets.connect(ws_url, origin=origin, max_size=None) as ws:
        await ws.send(json.dumps({"type": "send", "content": text, "conversation": str(uuid.uuid4())}))
        while True:
            msg: dict[str, object] = json.loads(await asyncio.wait_for(ws.recv(), 240))
            if msg.get("type") in ("agent_message", "error"):
                return


def main(gateway: str, agent: str, rounds: int) -> None:
    hits = misses = false_saves = 0
    for round_no in range(rounds):
        for expects, text in CASES:
            before = keys(gateway, agent)
            asyncio.run(ask(gateway, agent, text))
            after = keys(gateway, agent)
            saved = sorted(k for k, at in after.items() if before.get(k) != at)
            # Every case starts from the same memory: what it saved goes.
            for key in saved:
                forget(gateway, agent, key)
            if expects and saved:
                hits += 1
            elif expects:
                misses += 1
            elif saved:
                false_saves += 1
            mark = "ok " if bool(saved) == expects else "BAD"
            print(f"{mark} r{round_no + 1} {'save' if expects else 'none'} {saved} {text[:40]}", flush=True)
    positives = hits + misses
    negatives = rounds * sum(1 for expects, _ in CASES if not expects)
    print(f"saved unprompted: {hits}/{positives}; saved when nothing to keep: {false_saves}/{negatives}")


main(sys.argv[1], sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 1)
