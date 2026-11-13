"""Self-test: config handling, plus a fake local server. Run: .venv/bin/python tests/test_voicecode.py"""
import asyncio
import json
import os
import tempfile
from pathlib import Path

from websockets.asyncio.server import serve

from voicecode import app as voicecode


class FakeUI:
    def __init__(self): self.calls = []
    def show_partial(self, t): self.calls.append(("partial", t))
    def add_final(self, t): self.calls.append(("final", t))
    def error(self, t): self.calls.append(("error", t))
    def hide(self): self.calls.append(("hide", ""))


async def run(server_handler, audio_cmd, vocabulary=()):
    got = {"headers": None, "texts": [], "audio_bytes": 0}

    async def handler(ws):
        got["headers"] = ws.request.headers
        await server_handler(ws, got)

    typed = []

    async def fake_type(text): typed.append(text)

    async with serve(handler, "127.0.0.1", 0) as server:
        port = server.sockets[0].getsockname()[1]
        ui, stop = FakeUI(), asyncio.Event()
        await voicecode.run_session(stop, ui, key="test-key", type_text=fake_type,
                                    url=f"ws://127.0.0.1:{port}", audio_cmd=audio_cmd,
                                    vocabulary=vocabulary)
    return got, ui, typed


async def happy_server(ws, got):
    async for m in ws:
        if isinstance(m, bytes):
            got["audio_bytes"] += len(m)
            continue
        got["texts"].append(json.loads(m))
        if got["texts"][-1]["type"] == "audio.done":
            await ws.send(json.dumps({"type": "transcript.partial", "text": "hello"}))
            await ws.send(json.dumps({"type": "transcript.final", "text": "hello world"}))
            await ws.send(json.dumps({"type": "transcript.final", "text": "  "}))  # skipped
            await ws.send(json.dumps({"type": "session.ended"}))
            await ws.close(1000, "session complete")


async def error_server(ws, got):
    await ws.send(json.dumps({"type": "error", "message": "bad key"}))
    await ws.wait_closed()


async def silent_server(ws, got):
    await ws.wait_closed()  # never sends a final


def test_config():
    d = Path(tempfile.mkdtemp())
    cfg_file = d / "config.json"
    assert voicecode.load_config(cfg_file) == voicecode.DEFAULT_CONFIG  # missing file
    cfg_file.write_text("{not json")
    assert voicecode.load_config(cfg_file) == voicecode.DEFAULT_CONFIG  # corrupt file
    cfg_file.write_text('{"language": "fr-FR", "ptt_key": "F9", "paused": "yes"}')
    assert voicecode.load_config(cfg_file) == {**voicecode.DEFAULT_CONFIG, "ptt_key": "F9"}
    saved = {"language": "", "ptt_key": "Pause", "paused": True, "endpoint": "wss://my.host/s"}
    voicecode.save_config(saved, d / "sub" / "config.json")
    assert voicecode.load_config(d / "sub" / "config.json") == saved

    ep = "wss://stt.example.com/v1/stream"
    assert voicecode.stream_url("", ep) == ep
    assert voicecode.stream_url("zh-CN", ep) == ep + "?language=zh-CN"

    os.environ[voicecode.ENDPOINT_ENV] = ep
    assert voicecode.resolve_endpoint({"endpoint": ""}) == ep  # no override -> env
    assert voicecode.resolve_endpoint({"endpoint": "wss://my.host/s"}) == "wss://my.host/s"
    del os.environ[voicecode.ENDPOINT_ENV]
    assert voicecode.resolve_endpoint({"endpoint": ""}) == ""  # main() exits on this
    assert voicecode.stream_url("en-US", "wss://h/x?region=eu") == "wss://h/x?region=eu&language=en-US"

    ok = ["wss://api.example.com/v1/audio/stream", "ws://localhost:8000/s", "ws://127.0.0.1/s"]
    bad = ["ws://api.example.com/s", "https://api.example.com", "wss://", "not a url", "",
           "wss://[bad"]
    assert all(voicecode.valid_endpoint(u) for u in ok)
    assert not any(voicecode.valid_endpoint(u) for u in bad)
    cfg_file.write_text('{"endpoint": "ws://evil.example.com/s"}')  # insecure -> no override
    assert voicecode.load_config(cfg_file)["endpoint"] == ""
    cfg_file.write_text('{"endpoint": "wss://my.host/s"}')
    assert voicecode.load_config(cfg_file)["endpoint"] == "wss://my.host/s"

    vocab = d / "vocabulary.txt"
    vocab.write_text("# comment\n kubectl \n\nuseEffect\n")
    assert voicecode.load_vocabulary(vocab) == ["kubectl", "useEffect"]


async def test_type_waits_for_key_release():
    calls = []
    real_exec = asyncio.create_subprocess_exec

    async def fake_exec(*argv, **kw):
        calls.append(argv)
        return await real_exec("true")

    asyncio.create_subprocess_exec = fake_exec
    try:
        voicecode.ptt_down.set()  # user still holds the key
        task = asyncio.create_task(voicecode.xdotool_type("hi "))
        await asyncio.sleep(0.2)
        assert calls == [], "typed while the PTT key was held"
        voicecode.ptt_down.clear()  # user releases
        await asyncio.wait_for(task, 1)
        assert calls == [("xdotool", "type", "--delay", "5", "--", "hi ")], calls
    finally:
        asyncio.create_subprocess_exec = real_exec


async def main():
    test_config()
    await test_type_waits_for_key_release()
    audio = ["head", "-c", "32000", "/dev/zero"]  # 1 s of silence, then EOF

    got, ui, typed = await run(happy_server, audio, vocabulary=["kubectl"])
    assert got["headers"]["Authorization"] == "Bearer test-key"
    assert got["texts"][0] == {"type": "vocabulary", "phrases": ["kubectl"]}
    assert got["texts"][-1] == {"type": "audio.done"}
    assert got["audio_bytes"] == 32000, got["audio_bytes"]
    assert typed == ["hello world "], typed
    assert ui.calls == [("partial", "hello"), ("final", "hello world"), ("hide", "")], ui.calls

    got, ui, typed = await run(error_server, ["sleep", "30"])  # error must not wait for audio
    assert typed == [] and ui.calls[-1][0] == "error" and "bad key" in ui.calls[-1][1], ui.calls

    voicecode.FINAL_TIMEOUT_S = 0.5
    got, ui, typed = await run(silent_server, audio)
    assert ui.calls == [("error", "No final transcript within 5 s")], ui.calls

    print("all tests passed")


if __name__ == "__main__":
    asyncio.run(asyncio.wait_for(main(), 20))
