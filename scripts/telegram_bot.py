#!/usr/bin/env python3
"""
hash256 mining Telegram bot.
Runs ON the coordinator host (talks to localhost:8787).

Commands (public, anyone can use):
  /help        - command list
  /stats       - mining stats (block, hashrate, sol counters, tx counters)
  /info        - chain & wallet info (with etherscan link)
  /worker      - per-worker breakdown
  /subscribe   - turn on notifications for hits/confirmations
  /unsubscribe - turn off notifications

Background notifications (broadcast to all subscribers):
  🎯 new solution accepted
  💰 tx confirmed on chain (reward landed)
  ❌ tx failed (revert / block cap)
"""
import json
import os
import sys
import threading
import time
import urllib.parse
import urllib.request

TOKEN = os.environ.get("TELEGRAM_BOT_TOKEN", "8592117696:AAHNuFfj41ybTrCThF2YBBolgr9uE0E1N8Y")
COORD = os.environ.get("COORD_URL", "http://127.0.0.1:8787")
WALLET = os.environ.get("MINER_ADDRESS", "0xF2641c957BF8f7c35f1019ACC49Ec9732B3B825d")
CONTRACT = os.environ.get("CONTRACT", "0xAC7b5d06fa1e77D08aea40d46cB7C5923A87A0cc")
SUBS_FILE = os.environ.get("SUBS_FILE", "/workspace/hash256/telegram_subscribers.txt")
NOTIFY_INTERVAL = int(os.environ.get("NOTIFY_INTERVAL_SEC", "8"))

API = f"https://api.telegram.org/bot{TOKEN}"
SUBS_LOCK = threading.Lock()


def http_get(url, timeout=10):
    req = urllib.request.Request(url)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read().decode("utf-8")


def http_post_json(url, payload, timeout=10):
    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url, data=data, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read().decode("utf-8")


def send(chat_id, text):
    try:
        http_post_json(
            f"{API}/sendMessage",
            {
                "chat_id": chat_id,
                "text": text,
                "parse_mode": "Markdown",
                "disable_web_page_preview": True,
            },
        )
    except Exception as e:
        print(f"send err to {chat_id}: {e}", flush=True)


def fmt_h(hps):
    if hps >= 1e12:
        return f"{hps / 1e12:.2f} TH/s"
    if hps >= 1e9:
        return f"{hps / 1e9:.2f} GH/s"
    if hps >= 1e6:
        return f"{hps / 1e6:.2f} MH/s"
    if hps >= 1e3:
        return f"{hps / 1e3:.2f} kH/s"
    return f"{hps:.0f} H/s"


# ---- Subscriber persistence ----
def load_subs():
    try:
        with open(SUBS_FILE) as f:
            return set(int(x.strip()) for x in f if x.strip().lstrip("-").isdigit())
    except FileNotFoundError:
        return set()


def save_subs(subs):
    with SUBS_LOCK:
        with open(SUBS_FILE, "w") as f:
            for s in sorted(subs):
                f.write(f"{s}\n")


def add_sub(chat_id):
    with SUBS_LOCK:
        subs = load_subs()
        if chat_id not in subs:
            subs.add(chat_id)
            save_subs_unlocked(subs)
            return True
        return False


def remove_sub(chat_id):
    with SUBS_LOCK:
        subs = load_subs()
        if chat_id in subs:
            subs.discard(chat_id)
            save_subs_unlocked(subs)
            return True
        return False


def save_subs_unlocked(subs):
    with open(SUBS_FILE, "w") as f:
        for s in sorted(subs):
            f.write(f"{s}\n")


def broadcast(text):
    subs = load_subs()
    print(f"broadcasting to {len(subs)} subscribers: {text[:80]}", flush=True)
    for chat_id in subs:
        send(chat_id, text)


# ---- Coord API ----
def coord_get(path):
    return json.loads(http_get(f"{COORD}{path}", timeout=5))


def metrics_dict():
    text = http_get(f"{COORD}/metrics", timeout=5)
    out = {}
    for line in text.splitlines():
        if line.startswith("hash256_") and " " in line:
            k, v = line.split(" ", 1)
            base = k.split("{", 1)[0]
            try:
                out[base] = int(float(v.strip()))
            except Exception:
                pass
    return out


# ---- Commands ----
def cmd_help(chat_id):
    add_sub(chat_id)
    send(
        chat_id,
        (
            "*🤖 hash256 mining bot*\n\n"
            "*/stats* — mining stats\n"
            "*/info* — wallet & chain info\n"
            "*/worker* — per-worker breakdown\n"
            "*/subscribe* — notif on hits (default ON when first /help|/start)\n"
            "*/unsubscribe* — turn off notif\n\n"
            f"wallet: `{WALLET}`\n"
            "_you'll get pinged on each new solution & tx confirm_"
        ),
    )


def cmd_stats(chat_id):
    add_sub(chat_id)
    try:
        h = coord_get("/health")
        w = coord_get("/workers")
        m = metrics_dict()
        msg = (
            "*📊 hash256 stats*\n\n"
            f"⛓ block: `{h['block']}`  epoch: `{h['epoch']}`\n"
            f"👷 workers: *{w['active']}* active / {w['total']} total\n"
            f"⚡ hashrate: *{fmt_h(w['total_hashrate'])}*\n\n"
            f"🪙 solutions:  ok={m.get('hash256_solutions_accepted_total', 0)}  "
            f"bad={m.get('hash256_solutions_rejected_total', 0)}  "
            f"stale={m.get('hash256_stale_solutions_total', 0)}\n"
            f"📤 tx:  sent={m.get('hash256_txs_submitted_total', 0)}  "
            f"ok={m.get('hash256_confirmations_total', 0)}  "
            f"fail={m.get('hash256_failures_total', 0)}  "
            f"replaced={m.get('hash256_replacements_total', 0)}"
        )
        send(chat_id, msg)
    except Exception as e:
        send(chat_id, f"❌ stats error: `{e}`")


def cmd_info(chat_id):
    add_sub(chat_id)
    try:
        h = coord_get("/health")
        rpc_ok = sum(1 for x in h.get("rpc_health", []) if x.get("healthy"))
        rpc_total = len(h.get("rpc_health", []))
        msg = (
            "*ℹ️ hash256 info*\n\n"
            f"📜 contract: `{CONTRACT}`\n"
            f"💼 wallet:   `{WALLET}`\n"
            f"⛓ chain:    Ethereum mainnet\n\n"
            f"🧩 challenge: `{h['challenge'][:18]}…`\n"
            f"🎯 target:    `{h['target'][:24]}…`\n"
            f"📦 block:     `{h['block']}`\n"
            f"⏱ epoch:     `{h['epoch']}`\n"
            f"🌐 RPC:       {rpc_ok}/{rpc_total} healthy\n\n"
            f"🔍 etherscan: https://etherscan.io/address/{WALLET}"
        )
        send(chat_id, msg)
    except Exception as e:
        send(chat_id, f"❌ info error: `{e}`")


def cmd_worker(chat_id):
    add_sub(chat_id)
    try:
        w = coord_get("/workers")
        if w["active"] == 0:
            send(chat_id, "*👷 workers*\n\n_no active workers_")
            return
        lines = [f"*👷 workers — {w['active']} active*\n"]
        for x in sorted(w["workers"], key=lambda x: -x["hashrate"]):
            wid = x["id"]
            mid = x["miner_id"][:24]
            dev = x["device_name"][:30]
            lines.append(f"`{wid:>4}` `{mid}` `{fmt_h(x['hashrate'])}`\n   _{dev}_")
        lines.append(f"\n*total: {fmt_h(w['total_hashrate'])}*")
        send(chat_id, "\n".join(lines))
    except Exception as e:
        send(chat_id, f"❌ worker error: `{e}`")


def cmd_subscribe(chat_id):
    if add_sub(chat_id):
        send(chat_id, "🔔 *subscribed* — you'll be pinged on every hit & tx confirm.")
    else:
        send(chat_id, "🔔 you're already subscribed.")


def cmd_unsubscribe(chat_id):
    if remove_sub(chat_id):
        send(chat_id, "🔕 *unsubscribed* — no more notifications.")
    else:
        send(chat_id, "🔕 you weren't subscribed.")


def handle(update):
    msg = update.get("message") or update.get("edited_message")
    if not msg:
        return
    text = (msg.get("text") or "").strip()
    chat_id = msg.get("chat", {}).get("id")
    if not text or not chat_id:
        return
    cmd = text.split()[0].lower().split("@")[0]
    if cmd in ("/help", "/start"):
        cmd_help(chat_id)
    elif cmd == "/stats":
        cmd_stats(chat_id)
    elif cmd == "/info":
        cmd_info(chat_id)
    elif cmd in ("/worker", "/workers"):
        cmd_worker(chat_id)
    elif cmd == "/subscribe":
        cmd_subscribe(chat_id)
    elif cmd == "/unsubscribe":
        cmd_unsubscribe(chat_id)


# ---- Notification poller (background thread) ----
def notification_loop():
    print(f"notification loop starting (interval={NOTIFY_INTERVAL}s)", flush=True)
    # Initialize baseline so we don't spam past hits at startup.
    try:
        m = metrics_dict()
        last = {
            "accepted": m.get("hash256_solutions_accepted_total", 0),
            "confirmed": m.get("hash256_confirmations_total", 0),
            "failed": m.get("hash256_failures_total", 0),
            "submitted": m.get("hash256_txs_submitted_total", 0),
            "rejected": m.get("hash256_solutions_rejected_total", 0),
        }
        print(f"baseline: {last}", flush=True)
    except Exception as e:
        print(f"baseline err: {e}, defaulting to 0", flush=True)
        last = {"accepted": 0, "confirmed": 0, "failed": 0, "submitted": 0, "rejected": 0}

    while True:
        time.sleep(NOTIFY_INTERVAL)
        try:
            m = metrics_dict()
            cur = {
                "accepted": m.get("hash256_solutions_accepted_total", 0),
                "confirmed": m.get("hash256_confirmations_total", 0),
                "failed": m.get("hash256_failures_total", 0),
                "submitted": m.get("hash256_txs_submitted_total", 0),
                "rejected": m.get("hash256_solutions_rejected_total", 0),
            }

            # 🎯 New solution accepted (worker found valid PoW & coord is signing).
            if cur["accepted"] > last["accepted"]:
                delta = cur["accepted"] - last["accepted"]
                broadcast(
                    f"🎯 *FOUND solution!* +{delta}\n\n"
                    f"Accepted total: *{cur['accepted']}*\n"
                    f"Submitting tx now…"
                )

            # 💰 New tx confirmed on chain → reward landed.
            if cur["confirmed"] > last["confirmed"]:
                delta = cur["confirmed"] - last["confirmed"]
                hash_reward = delta * 100
                broadcast(
                    f"💰 *TX CONFIRMED!* +{delta} (on chain)\n\n"
                    f"+{hash_reward} HASH → `{WALLET}`\n\n"
                    f"Total confirmed: *{cur['confirmed']}*\n"
                    f"Total HASH mined: ~{cur['confirmed'] * 100}\n\n"
                    f"🔍 https://etherscan.io/address/{WALLET}"
                )

            # ❌ TX failed (reverted, likely block cap reached or stale).
            if cur["failed"] > last["failed"]:
                delta = cur["failed"] - last["failed"]
                broadcast(
                    f"❌ *TX FAILED* +{delta} (reverted)\n\n"
                    f"likely cause: block cap reached (10 winners) or stale challenge\n"
                    f"Total failed: {cur['failed']}\n"
                    f"_consider bumping PRIORITY_GWEI if this keeps happening_"
                )

            last = cur
        except Exception as e:
            print(f"notif loop err: {e}", flush=True)


# ---- Telegram polling ----
def main():
    print(
        f"telegram bot starting; coord={COORD} wallet={WALLET} subs_file={SUBS_FILE}",
        flush=True,
    )
    print(f"loaded {len(load_subs())} existing subscribers", flush=True)

    # Start notification thread.
    t = threading.Thread(target=notification_loop, daemon=True)
    t.start()

    last_update = 0
    backoff = 2
    while True:
        try:
            params = urllib.parse.urlencode(
                {"offset": last_update + 1, "timeout": 25}
            )
            r = http_get(f"{API}/getUpdates?{params}", timeout=30)
            data = json.loads(r)
            if not data.get("ok"):
                print(f"tg !ok: {data}", flush=True)
                time.sleep(backoff)
                backoff = min(backoff * 2, 30)
                continue
            backoff = 2
            for upd in data.get("result", []):
                last_update = upd["update_id"]
                try:
                    handle(upd)
                except Exception as e:
                    print(f"handle err: {e}", flush=True)
        except Exception as e:
            print(f"loop err: {e}", flush=True)
            time.sleep(backoff)
            backoff = min(backoff * 2, 30)


if __name__ == "__main__":
    main()
