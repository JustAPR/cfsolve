#!/usr/bin/env python3
"""
Turnstile Solve Benchmark & Latency Profiler for cfsolve-server.
Uses standard library (no pip dependencies required).
"""

import argparse
import concurrent.futures
import json
import statistics
import sys
import time
import urllib.error
import urllib.request

DEFAULT_SERVER = "http://127.0.0.1:3000"
DEFAULT_URL = "https://jerryccs.com/auth/"
DEFAULT_SITEKEY = "0x4AAAAAAFOskVGnd7MM-REl"


def check_server_health(server_url: str) -> dict:
    req = urllib.request.Request(f"{server_url}/health")
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            return json.loads(resp.read().decode())
    except Exception as e:
        sys.exit(f"[-] Cannot reach cfsolve-server at {server_url}/health: {e}\nEnsure cfsolve-server is running.")


def solve_request(server_url: str, url: str, sitekey: str, page_source: str, profile: str, proxy: str = None) -> dict:
    payload = {
        "url": url,
        "sitekey": sitekey,
        "page_source": page_source,
        "profile": profile,
    }
    if proxy:
        payload["proxy"] = proxy

    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        f"{server_url}/turnstile",
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )

    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            t1 = time.perf_counter()
            body = json.loads(resp.read().decode())
            return {
                "success": body.get("success", False),
                "client_ms": (t1 - t0) * 1000.0,
                "server_ms": body.get("elapsed_ms", 0),
                "token": body.get("token"),
                "error": body.get("error"),
            }
    except urllib.error.HTTPError as e:
        t1 = time.perf_counter()
        err_msg = e.read().decode()
        return {
            "success": False,
            "client_ms": (t1 - t0) * 1000.0,
            "server_ms": 0,
            "token": None,
            "error": f"HTTP {e.code}: {err_msg}",
        }
    except Exception as e:
        t1 = time.perf_counter()
        return {
            "success": False,
            "client_ms": (t1 - t0) * 1000.0,
            "server_ms": 0,
            "token": None,
            "error": str(e),
        }


def format_stats(latencies: list[float]) -> dict:
    if not latencies:
        return {"min": 0, "max": 0, "mean": 0, "median": 0, "p95": 0}
    sorted_l = sorted(latencies)
    p95_idx = int(0.95 * len(sorted_l))
    return {
        "min": min(sorted_l),
        "max": max(sorted_l),
        "mean": statistics.mean(sorted_l),
        "median": statistics.median(sorted_l),
        "p95": sorted_l[min(p95_idx, len(sorted_l) - 1)],
    }


def print_banner(server_info: dict, target_url: str, sitekey: str, profile: str):
    print("=" * 72)
    print("  cfsolve Benchmark Suite — Turnstile Latency & Throughput Report")
    print("=" * 72)
    print(f"Server Target   : {DEFAULT_SERVER}")
    print(f"Engine Version  : cfsolve v{server_info.get('version', '0.1.0')}")
    print(f"Browser Profile : {profile}")
    print(f"Challenge URL   : {target_url}")
    print(f"Sitekey         : {sitekey}")
    print("=" * 72)


def run_benchmark(server: str, url: str, sitekey: str, profile: str, iterations: int, concurrency: int, page_source: str):
    print(f"\n[*] Running Benchmark [Mode: page_source='{page_source}', Iterations={iterations}, Concurrency={concurrency}]...")
    results = []

    if concurrency <= 1:
        for i in range(1, iterations + 1):
            print(f"  -> Run {i}/{iterations}...", end=" ", flush=True)
            res = solve_request(server, url, sitekey, page_source, profile)
            results.append(res)
            if res["success"]:
                print(f"OK [Client: {res['client_ms']:.1f}ms | Server: {res['server_ms']}ms | Token: {res['token'][:24]}...]")
            else:
                print(f"FAIL [Error: {res['error']}]")
    else:
        with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
            futures = [
                executor.submit(solve_request, server, url, sitekey, page_source, profile)
                for _ in range(iterations)
            ]
            for i, future in enumerate(concurrent.futures.as_completed(futures), 1):
                res = future.result()
                results.append(res)
                status = "OK" if res["success"] else "FAIL"
                print(f"  -> Concurrency Run {i}/{iterations} [{status}]: {res['client_ms']:.1f}ms (Server: {res['server_ms']}ms)")

    # Analyze results
    successes = [r for r in results if r["success"]]
    failures = [r for r in results if not r["success"]]
    client_times = [r["client_ms"] for r in successes]
    server_times = [r["server_ms"] for r in successes]

    print("\n" + "-" * 72)
    print(f"  RESULTS SUMMARY — Mode: {page_source.upper()}")
    print("-" * 72)
    print(f"  Total Requests    : {len(results)}")
    print(f"  Successful Solves : {len(successes)} ({len(successes)/len(results)*100:.1f}%)")
    print(f"  Failed Solves     : {len(failures)}")

    if successes:
        c_stats = format_stats(client_times)
        s_stats = format_stats(server_times)

        print("\n  Latency Metric         Client Round-Trip        Internal Server Core")
        print("  " + "-" * 66)
        print(f"  Min Latency            {c_stats['min']:8.1f} ms             {s_stats['min']:8.1f} ms")
        print(f"  Median (p50)           {c_stats['median']:8.1f} ms             {s_stats['median']:8.1f} ms")
        print(f"  Average (Mean)         {c_stats['mean']:8.1f} ms             {s_stats['mean']:8.1f} ms")
        print(f"  95th Percentile        {c_stats['p95']:8.1f} ms             {s_stats['p95']:8.1f} ms")
        print(f"  Max Latency            {c_stats['max']:8.1f} ms             {s_stats['max']:8.1f} ms")

        # First request (cold) vs warm
        if len(client_times) > 1:
            cold_time = client_times[0]
            warm_avg = statistics.mean(client_times[1:])
            print("\n  Cache Performance:")
            print(f"  • Cold / First Solve  : {cold_time:.1f} ms (includes AST parse & initial handshake)")
            print(f"  • Warm / Cached Solves: {warm_avg:.1f} ms avg (bypassing deobfuscation via AST cache)")
            speedup = ((cold_time - warm_avg) / cold_time) * 100.0 if cold_time > 0 else 0
            print(f"  • Deobfuscation Cache Gain: {speedup:+.1f}% latency reduction")
    print("-" * 72)


def main():
    parser = argparse.ArgumentParser(description="Turnstile Benchmark for cfsolve-server")
    parser.add_argument("--server", default=DEFAULT_SERVER, help="Server base URL (default: http://127.0.0.1:3000)")
    parser.add_argument("--url", default=DEFAULT_URL, help="Target page URL")
    parser.add_argument("--sitekey", default=DEFAULT_SITEKEY, help="Turnstile sitekey")
    parser.add_argument("--profile", default="safari-26-ios", help="Browser profile name")
    parser.add_argument("-n", "--iterations", type=int, default=3, help="Number of benchmark solves")
    parser.add_argument("-c", "--concurrency", type=int, default=1, help="Concurrent workers")
    parser.add_argument("--mode", choices=["blank", "navigate", "both"], default="both", help="Page source mode")
    args = parser.parse_args()

    health = check_server_health(args.server)
    print_banner(health, args.url, args.sitekey, args.profile)

    modes = ["blank", "navigate"] if args.mode == "both" else [args.mode]
    for mode in modes:
        run_benchmark(args.server, args.url, args.sitekey, args.profile, args.iterations, args.concurrency, mode)


if __name__ == "__main__":
    main()
