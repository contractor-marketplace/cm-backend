"""A small Apify REST client, covering exactly the four calls this job makes.

Async pattern only — start, poll, page the dataset. `run-sync-get-dataset-items`
is deliberately not implemented: it holds one HTTP connection open for the whole
run and times out on batches this size, and having it available would invite
somebody to use it.

The token never appears in a log line. It travels in the query string because
that is what the Apify API takes, so every URL is passed through `_redact`
before it is printed or attached to an exception.
"""

from __future__ import annotations

import json
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any, Callable, Iterator, Optional

API_ROOT = "https://api.apify.com/v2"

# Poll cadence and ceiling, both from the spec.
POLL_SECONDS = 15
RUN_TIMEOUT_SECONDS = 30 * 60

# Backoff for 429 and 5xx: three retries, base five seconds, doubling.
MAX_RETRIES = 3
BACKOFF_BASE_SECONDS = 5

DATASET_PAGE = 1000

TERMINAL_STATUSES = {"SUCCEEDED", "FAILED", "TIMED-OUT", "ABORTED"}


class ApifyError(RuntimeError):
    """A call failed after its retries. Carries a redacted URL, never a token."""


def _redact(url: str) -> str:
    """Replace the token value in a URL with a marker.

    Used on every path that could reach a log, an exception message or the
    database. A leaked token in `scrape_runs.error_message` would be a
    credential sitting in a table people query casually.
    """
    parts = urllib.parse.urlsplit(url)
    query = urllib.parse.parse_qsl(parts.query, keep_blank_values=True)
    cleaned = [(k, "***" if k == "token" else v) for k, v in query]
    return urllib.parse.urlunsplit(
        (parts.scheme, parts.netloc, parts.path, urllib.parse.urlencode(cleaned), parts.fragment)
    )


@dataclass
class RunHandle:
    run_id: str
    dataset_id: Optional[str]
    status: str
    raw: dict


class ApifyClient:
    def __init__(self, token: str, actor_id: str, *, log: Callable[[str], None] = print):
        if not token:
            raise ValueError("an Apify token is required")
        self._token = token
        self.actor_id = actor_id
        self._log = log

    # ── transport ────────────────────────────────────────────────────────

    def _url(self, path: str, **params: Any) -> str:
        query = {k: v for k, v in params.items() if v is not None}
        query["token"] = self._token
        return f"{API_ROOT}{path}?{urllib.parse.urlencode(query)}"

    def _request(self, method: str, url: str, payload: Optional[dict] = None) -> Any:
        body = json.dumps(payload).encode() if payload is not None else None
        headers = {"Content-Type": "application/json"} if body else {}

        last_error: Optional[Exception] = None
        for attempt in range(MAX_RETRIES + 1):
            request = urllib.request.Request(url, data=body, headers=headers, method=method)
            try:
                with urllib.request.urlopen(request, timeout=120) as response:
                    raw = response.read()
                    return json.loads(raw) if raw else None
            except urllib.error.HTTPError as error:
                # 4xx other than 429 is our fault and will not improve on
                # retry — a bad actor id or a rejected input shape.
                if error.code != 429 and error.code < 500:
                    detail = error.read().decode("utf-8", "replace")[:500]
                    raise ApifyError(
                        f"{method} {_redact(url)} -> {error.code}: {detail}"
                    ) from None
                last_error = error
            except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
                last_error = error

            if attempt < MAX_RETRIES:
                delay = BACKOFF_BASE_SECONDS * (2**attempt)
                self._log(f"    apify call failed ({last_error}); retrying in {delay}s")
                time.sleep(delay)

        raise ApifyError(
            f"{method} {_redact(url)} failed after {MAX_RETRIES} retries: {last_error}"
        )

    # ── the four calls ───────────────────────────────────────────────────

    def start_run(self, actor_input: dict) -> RunHandle:
        """POST /v2/acts/{id}/runs — returns as soon as the run is queued."""
        url = self._url(f"/acts/{self.actor_id}/runs")
        body = self._request("POST", url, actor_input) or {}
        data = body.get("data") or {}
        run_id = data.get("id")
        if not run_id:
            raise ApifyError(f"the run response carried no id: {json.dumps(body)[:400]}")
        return RunHandle(
            run_id=run_id,
            dataset_id=data.get("defaultDatasetId"),
            status=data.get("status", "READY"),
            raw=data,
        )

    def get_run(self, run_id: str) -> RunHandle:
        url = self._url(f"/actor-runs/{run_id}")
        data = (self._request("GET", url) or {}).get("data") or {}
        return RunHandle(
            run_id=run_id,
            dataset_id=data.get("defaultDatasetId"),
            status=data.get("status", "UNKNOWN"),
            raw=data,
        )

    def abort_run(self, run_id: str) -> None:
        try:
            self._request("POST", self._url(f"/actor-runs/{run_id}/abort"))
        except ApifyError as error:
            # A run that finished on its own between the timeout check and this
            # call refuses the abort. Not worth failing the batch over — the
            # dataset is fetched either way.
            self._log(f"    abort of {run_id} was refused: {error}")

    def wait_for_run(self, run_id: str) -> RunHandle:
        """Poll to a terminal status, aborting at the hard timeout.

        Returns whatever state the run reached. A non-SUCCEEDED status is not
        raised here on purpose: partial results are usable, and the caller
        fetches the dataset regardless.
        """
        deadline = time.monotonic() + RUN_TIMEOUT_SECONDS
        while True:
            handle = self.get_run(run_id)
            if handle.status in TERMINAL_STATUSES:
                return handle

            if time.monotonic() >= deadline:
                self._log(
                    f"    run {run_id} exceeded {RUN_TIMEOUT_SECONDS // 60}m; aborting "
                    "and taking whatever landed"
                )
                self.abort_run(run_id)
                return self.get_run(run_id)

            time.sleep(POLL_SECONDS)

    def iter_dataset(self, dataset_id: str) -> Iterator[dict]:
        """Page the dataset until a short page arrives.

        Never assumes one page: `limit` is the ceiling, not a promise, and a
        batch of twenty places at fifty reviews each can exceed it.
        """
        offset = 0
        while True:
            url = self._url(
                f"/datasets/{dataset_id}/items",
                offset=offset,
                limit=DATASET_PAGE,
                clean="true",
            )
            page = self._request("GET", url) or []
            if not isinstance(page, list):
                raise ApifyError(f"dataset page {offset} was not a list")

            yield from page

            if len(page) < DATASET_PAGE:
                return
            offset += DATASET_PAGE


def run_cost_usd(handle: RunHandle) -> float:
    """What the run actually cost, from Apify rather than from a guess.

    The run object reports `usageTotalUsd`. Reading it beats modelling the
    price locally: the model would be wrong the first time the actor changed
    its pricing, and it is the number the spend cap is supposed to be counting.
    """
    for key in ("usageTotalUsd", "usageUsd"):
        value = handle.raw.get(key)
        if isinstance(value, (int, float)):
            return float(value)
    return 0.0
