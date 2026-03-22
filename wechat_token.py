#!/usr/bin/env python3
"""WeChat iLink bot token generator.

Performs the QR code login flow and prints the bot_token
for use in acpconnector config.yaml.

Usage:
    uv run wechat_token.py
    uv run wechat_token.py --base-url https://ilinkai.weixin.qq.com
"""
import argparse
import json
import sys
import time
import urllib.request
import urllib.error

DEFAULT_BASE_URL = "https://ilinkai.weixin.qq.com"


def get_qrcode(base_url: str) -> dict:
    """Fetch QR code for bot login."""
    url = f"{base_url}/ilink/bot/get_bot_qrcode?bot_type=3"
    try:
        with urllib.request.urlopen(url, timeout=30) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"Failed to get QR code: {e.code} - {e.read().decode()}")
    except Exception as e:
        raise RuntimeError(f"Failed to get QR code: {e}")


def poll_qrcode_status(base_url: str, qrcode: str) -> dict:
    """Poll QR code status until confirmed or expired."""
    url = f"{base_url}/ilink/bot/get_qrcode_status?qrcode={qrcode}"
    try:
        with urllib.request.urlopen(url, timeout=30) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"Failed to poll QR status: {e.code} - {e.read().decode()}")
    except Exception as e:
        raise RuntimeError(f"Failed to poll QR status: {e}")


def main():
    parser = argparse.ArgumentParser(description="Generate WeChat iLink bot token")
    parser.add_argument(
        "--base-url",
        default=DEFAULT_BASE_URL,
        help=f"WeChat iLink API base URL (default: {DEFAULT_BASE_URL})",
    )
    args = parser.parse_args()

    print("[1/3] Fetching QR code...")
    try:
        qr = get_qrcode(args.base_url)
    except RuntimeError as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)

    qrcode = qr.get("qrcode")
    # API returns qrcode_img_content (not qrcode_url)
    qr_url = qr.get("qrcode_img_content") or qr.get("qrcode_url")

    if not qrcode or not qr_url:
        print(f"Error: Invalid QR code response: {qr}", file=sys.stderr)
        sys.exit(1)

    print(f"[2/3] Scan this QR code with WeChat:\n\n  {qr_url}\n")
    print("Waiting for confirmation...", end="", flush=True)

    try:
        while True:
            time.sleep(2)
            print(".", end="", flush=True)
            status = poll_qrcode_status(args.base_url, qrcode)
            status_code = status.get("status")

            if status_code == "confirmed":
                break
            elif status_code == "expired":
                print("\n\nError: QR code expired. Please run the script again.", file=sys.stderr)
                sys.exit(1)
            elif status_code == "scanned":
                # Continue polling
                pass
            # Other statuses: pending, etc. - continue polling

        bot_token = status.get("bot_token")
        bot_id = status.get("ilink_bot_id", "")
        user_id = status.get("ilink_user_id", "")

        if not bot_token:
            print(f"\n\nError: No bot_token in response: {status}", file=sys.stderr)
            sys.exit(1)

        print(f"\n\n[3/3] Success! Add this to your config.yaml:\n")
        print(f"  wechat:")
        print(f"    bot_token: \"{bot_token}\"")
        if bot_id:
            print(f"\n  # bot_id:  {bot_id}")
        if user_id:
            print(f"  # user_id: {user_id}")

    except KeyboardInterrupt:
        print("\n\nCancelled by user.", file=sys.stderr)
        sys.exit(1)
    except RuntimeError as e:
        print(f"\n\nError: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
