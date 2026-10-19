import hmac
import os

_EXPECTED = b"Bearer " + os.environ["VLLM_API_KEY"].encode()
if _EXPECTED == b"Bearer ":
    raise RuntimeError("VLLM_API_KEY is empty")


class RequireKey:
    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        kind = scope["type"]
        if kind not in ("http", "websocket") or (
            kind == "http" and scope["method"] == "GET" and scope["path"] == "/health"
        ):
            return await self.app(scope, receive, send)
        got = dict(scope["headers"]).get(b"authorization", b"")
        if hmac.compare_digest(got, _EXPECTED):
            return await self.app(scope, receive, send)
        if kind == "websocket":
            return await send({"type": "websocket.close", "code": 1008})
        await send({
            "type": "http.response.start",
            "status": 401,
            "headers": [(b"content-type", b"application/json")],
        })
        await send({"type": "http.response.body", "body": b'{"error":"Unauthorized"}'})
