#!/usr/bin/env python3
"""Mock till service for exercising the POS UI. MOCK_SCENARIO=success|change|<error code>.
Run, then: adb reverse tcp:9877 tcp:9877"""
import json, os, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SCEN = os.environ.get("MOCK_SCENARIO", "success")
HTTP = {"bad_request": 400, "busy": 409, "no_card": 409, "relay_unreachable": 502, "card_error": 502,
        "insufficient": 402, "no_exact_plan": 422, "declined": 402, "undecided": 202, "recovering": 503, "internal": 500}
SALES, REFS, PENDING = [], {}, set()

def record(price, ref):
    sale = {"id": len(SALES) + 1, "ts_unix_ms": int(time.time() * 1000), "price_msat": price, "received_msat": price,
            "overpaid_msat": 0, "fees_msat": 0, "txid": os.urandom(32).hex(), "inputs": [], "change": [], "client_ref": ref}
    SALES.append(sale)
    change = [{"slot": 1, "msat": 3000}, {"slot": 2, "msat": 1000}] if SCEN == "change" else []
    resp = dict(sale, ok=True, sale_id=sale["id"], inputs=[{"slot": 0, "msat": price + sum(c["msat"] for c in change)}],
                change=change, collection_pending=False, warnings=[], duration_ms=6000)
    if ref: REFS[ref] = resp
    return resp

class H(BaseHTTPRequestHandler):
    def send(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)

    def do_GET(self):
        if self.path == "/v1/health": self.send(200, {"ok": True})
        elif self.path == "/v1/status":
            self.send(200, {"federation_id": "ab" * 32, "federation_name": "Mock Federation",
                            "wallet_balance_msat": sum(s["received_msat"] for s in SALES), "card_relay_ok": True, "busy": False})
        elif self.path == "/v1/sales":
            self.send(200, {"count": len(SALES), "total_price_msat": sum(s["price_msat"] for s in SALES),
                            "total_received_msat": sum(s["received_msat"] for s in SALES), "sales": SALES})
        else: self.send(404, {"ok": False, "error": "internal", "detail": "no such route"})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        if self.path != "/v1/charge": return self.send(404, {"ok": False, "error": "internal", "detail": "no such route"})
        time.sleep(6)
        ref = body.get("client_ref")
        if ref in REFS: return self.send(200, REFS[ref])
        if SCEN in HTTP and SCEN != "undecided": return self.send(HTTP[SCEN], {"ok": False, "error": SCEN, "detail": "mock " + SCEN})
        price = body["price_msat"]
        if SCEN == "undecided" and ref not in PENDING:  # records the sale ~8 s later
            PENDING.add(ref); threading.Timer(8, record, (price, ref)).start()
        if SCEN == "undecided":
            return self.send(202, {"ok": False, "error": "undecided", "detail": "mock pending"})
        self.send(200, record(price, ref))

ThreadingHTTPServer

SCEN = os.environ.get("MOCK_SCENARIO", "success")
HTTP = {"bad_request": 400, "busy": 409, "no_card": 409, "relay_unreachable": 502, "card_error": 502,
        "insufficient": 402, "no_exact_plan": 422, "declined": 402, "undecided": 202, "recovering": 503, "internal": 500}
SALES, REFS, PENDING = [], {}, set()

class H(BaseHTTPRequestHandler):
    def send(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)

    def do_GET(self):
        if self.path == "/v1/health": self.send(200, {"ok": True})
        elif self.path == "/v1/status":
            self.send(200, {"federation_id": "ab" * 32, "federation_name": "Mock Federation",
                            "wallet_balance_msat": sum(s["received_msat"] for s in SALES), "card_relay_ok": True, "busy": False})
        elif self.path == "/v1/sales":
            self.send(200, {"count": len(SALES), "total_price_msat": sum(s["price_msat"] for s in SALES),
                            "total_received_msat": sum(s["received_msat"] for s in SALES), "sales": SALES})
        else: self.send(404, {"ok": False, "error": "internal", "detail": "no such route"})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        if self.path != "/v1/charge": return self.send(404, {"ok": False, "error": "internal", "detail": "no such route"})
        time.sleep(6)
        ref = body.get("client_ref")
        if ref in REFS: return self.send(200, REFS[ref])
        if SCEN in HTTP and SCEN != "undecided": return self.send(HTTP[SCEN], {"ok": False, "error": SCEN, "detail": "mock " + SCEN})
        price = body["price_msat"]
        if SCEN == "undecided" and ref not in PENDING:  # records the sale ~8 s later
            PENDING.add(ref); threading.Timer(8, record, (price, ref)).start()
        if SCEN == "undecided":
            return self.send(202, {"ok": False, "error": "undecided", "detail": "mock pending"})
        paid = price + (1000 if SCEN == "change" else 0)
        sale = {"id": len(SALES) + 1, "ts_unix_ms": int(time.time() * 1000), "price_msat": price, "received_msat": price,
                "overpaid_msat": 0, "fees_msat": 0, "txid": os.urandom(32).hex(), "inputs": [], "change": [], "client_ref": ref}
        SALES.append(sale)
        change = [{"slot": 1, "msat": 3000}, {"slot": 2, "msat": 1000}] if SCEN == "change" else []
        resp = dict(sale, ok=True, sale_id=sale["id"], inputs=[{"slot": 0, "msat": paid}], change=change,
                    collection_pending=False, warnings=[], duration_ms=6000)
        if ref: REFS[ref] = resp
        self.send(200, resp)

ThreadingHTTPServer(("127.0.0.1", 9877), H).serve_forever()
