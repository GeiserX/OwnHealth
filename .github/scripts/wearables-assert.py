"""Check completed Open Wearables polls and the samples recorded by those sources."""

import json
import os
import subprocess
import time


def query(sql):
    return json.loads(subprocess.check_output(
        ["psql", "-XAt", "-v", "ON_ERROR_STOP=1", "-d", os.environ["OWNHEALTH_DATABASE_URL"], "-c", sql],
        text=True,
        timeout=15,
    ))


def wait_for_polls(expected_links):
    deadline = time.monotonic() + 180
    while True:
        runs = query("""
            select coalesce(json_agg(r.status order by s.id), '[]')
            from source s
            left join lateral (
                select status from source_run
                where source_id = s.id and trigger = 'poll'
                order by id desc limit 1
            ) r on true
            where s.kind = 'openwearables'
        """)
        if len(runs) != expected_links:
            raise SystemExit(f"Expected {expected_links} Open Wearables links, got {len(runs)}")
        if any(status in ("error", "partial") for status in runs):
            raise SystemExit(f"Open Wearables poll did not complete cleanly: {runs}")
        if all(status == "ok" for status in runs):
            print(f"Completed {len(runs)} Open Wearables polls")
            return
        if time.monotonic() >= deadline:
            raise SystemExit(f"Open Wearables polls timed out: {runs}")
        time.sleep(2)


def assert_samples():
    counts = query("""
        select coalesce(json_object_agg(provider, samples), '{}')
        from (
            select w.provider, count(*) as samples
            from wearable_sample p
            join wearable_source w on w.id = p.source_id
            join source s on s.id = w.source_id
            where s.kind = 'openwearables'
            group by w.provider
        ) counted
    """)
    allowed = {"oura", "whoop", "polar", "withings"}
    expected = allowed | {"strava", "garmin"}
    for provider in sorted(expected | counts.keys()):
        print(f"{provider}: {counts.get(provider, 0)} samples")
    missing = sorted(allowed - counts.keys())
    unexpected = sorted(counts.keys() - allowed)
    if missing or unexpected:
        raise SystemExit(f"Open Wearables sample assertion failed. Missing: {missing}. Unexpected: {unexpected}.")


if __name__ == "__main__":
    wait_for_polls(expected_links=1 if os.environ.get("OW_SKIP_SEED") == "true" else 6)
    assert_samples()
