#!/usr/bin/env python3
"""
Example asynchronous Python client demonstrating concurrent requests to cfsolve-server.
Requires: pip install httpx
"""

import asyncio
import httpx
import time

SERVER_URL = "http://127.0.0.1:3000"


async def solve_turnstile(client: httpx.AsyncClient, task_id: int, page_url: str, sitekey: str, proxy: str = None):
    print(f"[Task {task_id}] Submitting Turnstile solve request for {page_url}...")
    start = time.perf_counter()

    payload = {
        "url": page_url,
        "sitekey": sitekey,
        "page_source": "navigate",  # or "blank" if skipping page loading
    }
    if proxy:
        payload["proxy"] = proxy

    try:
        response = await client.post(f"{SERVER_URL}/turnstile", json=payload, timeout=60.0)
        elapsed = time.perf_counter() - start
        data = response.json()
        if data.get("success"):
            print(f"[Task {task_id}] Success in {elapsed:.2f}s! Token: {data['token'][:30]}...")
            return data["token"]
        else:
            print(f"[Task {task_id}] Failed in {elapsed:.2f}s: {data.get('error')}")
            return None
    except Exception as e:
        print(f"[Task {task_id}] Request exception: {e}")
        return None


async def clear_challenge(client: httpx.AsyncClient, task_id: int, protected_url: str, proxy: str = None):
    print(f"[Task {task_id}] Submitting challenge clearance request for {protected_url}...")
    start = time.perf_counter()

    payload = {
        "url": protected_url,
    }
    if proxy:
        payload["proxy"] = proxy

    try:
        response = await client.post(f"{SERVER_URL}/clear", json=payload, timeout=60.0)
        elapsed = time.perf_counter() - start
        data = response.json()
        if data.get("success"):
            print(f"[Task {task_id}] Success in {elapsed:.2f}s! cf_clearance: {data['clearance'][:30]}...")
            print(f"[Task {task_id}] Cookie Header: {data['cookie_header'][:60]}...")
            return data["clearance"]
        else:
            print(f"[Task {task_id}] Failed in {elapsed:.2f}s: {data.get('error')}")
            return None
    except Exception as e:
        print(f"[Task {task_id}] Request exception: {e}")
        return None


async def check_health(client: httpx.AsyncClient):
    res = await client.get(f"{SERVER_URL}/health")
    print("Server Health & Metrics:", res.json())


async def main():
    async with httpx.AsyncClient() as client:
        # Check server health
        await check_health(client)

        # Example: Fanning out 5 concurrent Turnstile solve requests simultaneously
        # Replace URL and Sitekey with your target
        target_url = "https://example.com/login"
        target_sitekey = "0x4AAAAAAAJ8Q0gY-fS99999"

        print("\n--- Spawning 5 concurrent solve requests ---")
        tasks = [
            solve_turnstile(client, i, target_url, target_sitekey)
            for i in range(1, 6)
        ]

        # Execute concurrently
        results = await asyncio.gather(*tasks)
        print(f"\nAll 5 tasks completed. Successful results count: {sum(1 for r in results if r)}")

        # Check metrics after batch
        await check_health(client)


if __name__ == "__main__":
    asyncio.run(main())
